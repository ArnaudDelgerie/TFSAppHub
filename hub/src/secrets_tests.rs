use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use super::{
    is_keyring, keys_for_window, new_fake_keyring, new_fake_keyring_store, new_file_store_for_test,
    new_store, resolve_app_secret, secret_key_allowed, secrets_delete, secrets_get, secrets_has,
    secrets_set, store_for_window, APP_SECRET_ACCOUNT, PROBE_ACCOUNT,
};

fn file_store(directory: &Path) -> super::SecretStore {
    new_file_store_for_test(directory.join("secrets.json"))
}

/// Make a directory read-only, and hand back a guard that restores it — the
/// tempdir's own cleanup needs the write bit back before it can unlink
/// anything inside.
fn read_only(directory: &Path) {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o500))
        .expect("the directory permissions change");
}

fn writable(directory: &Path) {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .expect("the directory permissions change");
}

// --- the file fallback ---------------------------------------------------

#[test]
fn the_file_backend_round_trips() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = file_store(directory.path());

    assert!(!secrets_has(&store, "openai").expect("the store answers"));
    assert_eq!(
        secrets_get(&store, "openai").expect("the store answers"),
        None
    );

    secrets_set(&store, "openai", "sk-test".to_string()).expect("the store accepts the write");
    assert!(secrets_has(&store, "openai").expect("the store answers"));
    assert_eq!(
        secrets_get(&store, "openai").expect("the store answers"),
        Some("sk-test".to_string())
    );

    assert!(secrets_delete(&store, "openai").expect("the store answers"));
    assert!(!secrets_has(&store, "openai").expect("the store answers"));
}

#[test]
fn deleting_a_key_that_was_never_there_is_false_not_a_panic() {
    let directory = tempfile::tempdir().expect("a temp dir");

    assert!(!secrets_delete(&file_store(directory.path()), "never-set").expect("the store answers"));
}

#[test]
fn the_fallback_file_is_0600() {
    let directory = tempfile::tempdir().expect("a temp dir");
    secrets_set(&file_store(directory.path()), "openai", "sk-test".into())
        .expect("the store accepts the write");

    // The whole point of the fallback is that it is plaintext, so the file's
    // own permissions are the only thing left protecting it.
    let metadata = fs::metadata(directory.path().join("secrets.json")).expect("the store file");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}

#[test]
fn a_store_under_a_read_only_directory_reports_the_failed_set() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = file_store(directory.path());
    read_only(directory.path());

    let outcome = secrets_set(&store, "openai", "sk-test".to_string());
    writable(directory.path());

    // The one regression audit 024 named: a write the backend refused must
    // not be answered as a success the next launch disproves.
    assert!(outcome.is_err(), "a set under a read-only dir must fail");
    assert_eq!(
        secrets_get(&store, "openai").expect("the store answers"),
        None
    );
}

#[test]
fn a_failed_delete_keeps_the_key_and_reports_it() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = file_store(directory.path());
    secrets_set(&store, "openai", "sk-test".to_string()).expect("the store accepts the write");
    read_only(directory.path());

    let outcome = secrets_delete(&store, "openai");
    writable(directory.path());

    assert!(outcome.is_err(), "a delete under a read-only dir must fail");
    // The token the app believes revoked must still be assumed present.
    assert!(secrets_has(&store, "openai").expect("the store answers"));
    assert_eq!(
        secrets_get(&store, "openai").expect("the store answers"),
        Some("sk-test".to_string())
    );
}

#[test]
fn a_corrupt_store_file_is_an_error_and_is_never_written_over() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = file_store(directory.path());
    let corrupt = b"{ not json".to_vec();
    fs::write(directory.path().join("secrets.json"), &corrupt).expect("a broken store");

    assert!(secrets_get(&store, "openai").is_err());
    assert!(secrets_has(&store, "openai").is_err());
    let set = secrets_set(&store, "openai", "sk-test".to_string());
    assert!(set.is_err(), "a set over a corrupt file must fail");
    assert!(secrets_delete(&store, "openai").is_err());

    // The file is left byte-for-byte as it was found: a write over it would
    // destroy every other secret it holds, silently.
    let after = fs::read(directory.path().join("secrets.json")).expect("the untouched store");
    assert_eq!(after, corrupt);
}

// --- the explicit production backend smoke test -------------------------

#[test]
#[ignore = "requires the private Secret Service from make keyring-integration"]
fn production_keyring_round_trip() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let service = format!("test.tfsapp-hub.keyring-integration.{}", std::process::id());
    let store = new_store(&service, directory.path());

    assert!(
        is_keyring(&store),
        "the production store fell back to a file backend; make keyring-integration requires its private Secret Service"
    );
    assert_eq!(
        secrets_get(&store, "round-trip").expect("the store answers"),
        None
    );

    secrets_set(&store, "round-trip", "ephemeral-value".to_string())
        .expect("the store accepts the write");
    assert_eq!(
        secrets_get(&store, "round-trip").expect("the store answers"),
        Some("ephemeral-value".to_string())
    );
    assert!(secrets_delete(&store, "round-trip").expect("the store answers"));
    assert_eq!(
        secrets_get(&store, "round-trip").expect("the store answers"),
        None
    );
}

// --- the key manifest ----------------------------------------------------

#[test]
fn only_declared_keys_resolve() {
    let declared = vec!["openai".to_string()];

    assert!(secret_key_allowed(&declared, "openai"));
    assert!(!secret_key_allowed(&declared, "anthropic"));
}

#[test]
fn reserved_keys_are_refused_even_when_an_app_declares_them() {
    // An app that lists `app-secret` in its own `keys` — by mistake or
    // otherwise — must not be able to read the key everything it signs depends
    // on, nor the probe account the store's own availability check uses.
    let declared = vec![APP_SECRET_ACCOUNT.to_string(), PROBE_ACCOUNT.to_string()];

    assert!(!secret_key_allowed(&declared, APP_SECRET_ACCOUNT));
    assert!(!secret_key_allowed(&declared, PROBE_ACCOUNT));
}

// --- APP_SECRET resolution -----------------------------------------------

#[test]
fn an_existing_keyring_entry_always_wins_and_clears_the_stale_file() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = new_fake_keyring_store();
    secrets_set(&store, APP_SECRET_ACCOUNT, "from-keyring".to_string())
        .expect("the store accepts the write");
    fs::write(directory.path().join("app.secret"), "from-file").expect("a stale file");

    let secret = resolve_app_secret(&store, directory.path()).expect("a resolved secret");

    assert_eq!(secret, "from-keyring");
    // Left over from an installation that migrated on an earlier launch, and
    // safe to drop now the keyring is confirmed to hold the value.
    assert!(!directory.path().join("app.secret").exists());
}

#[test]
fn a_pre_keyring_file_is_migrated_and_only_then_deleted() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = new_fake_keyring_store();
    fs::write(directory.path().join("app.secret"), "carried-over").expect("an existing file");

    let secret = resolve_app_secret(&store, directory.path()).expect("a resolved secret");

    // The same value, which is what matters: everything the app has ever
    // signed stays valid across the migration.
    assert_eq!(secret, "carried-over");
    assert_eq!(
        secrets_get(&store, APP_SECRET_ACCOUNT).expect("the store answers"),
        Some("carried-over".to_string())
    );
    assert!(!directory.path().join("app.secret").exists());
}

#[test]
fn with_nothing_stored_a_secret_is_generated_and_kept() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = new_fake_keyring_store();

    let first = resolve_app_secret(&store, directory.path()).expect("a resolved secret");
    let second = resolve_app_secret(&store, directory.path()).expect("a resolved secret");

    assert!(!first.is_empty());
    // Stable across launches, or every restart would invalidate every CSRF
    // token and remember-me cookie the app ever issued.
    assert_eq!(first, second);
}

#[test]
fn with_no_keyring_the_secret_still_resolves_from_the_file() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = file_store(directory.path());

    let first = resolve_app_secret(&store, directory.path()).expect("a resolved secret");
    let second = resolve_app_secret(&store, directory.path()).expect("a resolved secret");

    // Resolution must never be the reason an app will not open: a degraded
    // store is still a stable one.
    assert!(!first.is_empty());
    assert_eq!(first, second);
}

#[test]
fn a_failing_keyring_never_generates_and_falls_back_to_the_file() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let keyring = new_fake_keyring();
    // Seeded through the working view: the entry exists behind the failing
    // one's refusal to say whether it does.
    secrets_set(
        &keyring.store("test.tfsapp-hub"),
        APP_SECRET_ACCOUNT,
        "existing".to_string(),
    )
    .expect("the store accepts the write");
    let failing = keyring.failing_store("test.tfsapp-hub");
    fs::write(directory.path().join("app.secret"), "from-file").expect("an existing file");

    let secret = resolve_app_secret(&failing, directory.path()).expect("a resolved secret");

    // The file is the only value this launch can trust: the keyring said
    // nothing about what it holds, so nothing is written to it and nothing
    // is generated over the entry that may exist.
    assert_eq!(secret, "from-file");
    assert_eq!(
        secrets_get(&keyring.store("test.tfsapp-hub"), APP_SECRET_ACCOUNT)
            .expect("the store answers"),
        Some("existing".to_string())
    );
    assert!(directory.path().join("app.secret").exists());
}

// --- the regression this plan must not ship ------------------------------

#[test]
fn a_window_reaches_its_own_app_s_store_and_no_other() {
    // Two apps, each in its own process — which is exactly what the hub does,
    // and what a `secret_get(app_id: String)` signature would have undone. There
    // is no argument here that names a store: the window is the whole address,
    // and a webview does not get to choose which window it is.
    let first = tauri::test::mock_app();
    let first_store = new_fake_keyring_store();
    secrets_set(&first_store, "openai", "first-app-value".to_string())
        .expect("the store accepts the write");
    first.manage(first_store);
    first.manage(crate::manifest::SecretsActions {
        ipc: true,
        bridge: false,
        keys: vec!["openai".to_string()],
    });

    let second = tauri::test::mock_app();
    let second_store = new_fake_keyring_store();
    secrets_set(&second_store, "openai", "second-app-value".to_string())
        .expect("the store accepts the write");
    second.manage(second_store);
    second.manage(crate::manifest::SecretsActions::default());

    let first_window =
        WebviewWindowBuilder::new(&first, "main", WebviewUrl::App("index.html".into()))
            .build()
            .expect("a window");
    let second_window =
        WebviewWindowBuilder::new(&second, "main", WebviewUrl::App("index.html".into()))
            .build()
            .expect("a window");

    let first_handle = first_window.as_ref().window();
    let second_handle = second_window.as_ref().window();

    assert_eq!(
        secrets_get(
            &store_for_window(&first_handle).expect("the first app's store"),
            "openai"
        )
        .expect("the store answers"),
        Some("first-app-value".to_string())
    );
    assert_eq!(
        secrets_get(
            &store_for_window(&second_handle).expect("the second app's store"),
            "openai"
        )
        .expect("the store answers"),
        Some("second-app-value".to_string())
    );

    // The declared keys follow the window too, so one app's manifest can never
    // widen what another app's webview may ask for.
    assert_eq!(keys_for_window(&first_handle), vec!["openai".to_string()]);
    assert!(keys_for_window(&second_handle).is_empty());
}
