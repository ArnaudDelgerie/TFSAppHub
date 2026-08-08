use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use super::{
    keys_for_window, new_fake_keyring_store, new_file_store_for_test, resolve_app_secret,
    secret_key_allowed, secrets_delete, secrets_get, secrets_has, secrets_set, store_for_window,
    APP_SECRET_ACCOUNT, PROBE_ACCOUNT,
};

fn file_store(directory: &Path) -> super::SecretStore {
    new_file_store_for_test(directory.join("secrets.json"))
}

// --- the file fallback ---------------------------------------------------

#[test]
fn the_file_backend_round_trips() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let store = file_store(directory.path());

    assert!(!secrets_has(&store, "openai"));
    assert_eq!(secrets_get(&store, "openai"), None);

    secrets_set(&store, "openai", "sk-test".to_string());
    assert!(secrets_has(&store, "openai"));
    assert_eq!(secrets_get(&store, "openai"), Some("sk-test".to_string()));

    assert!(secrets_delete(&store, "openai"));
    assert!(!secrets_has(&store, "openai"));
}

#[test]
fn deleting_a_key_that_was_never_there_is_false_not_a_panic() {
    let directory = tempfile::tempdir().expect("a temp dir");

    assert!(!secrets_delete(&file_store(directory.path()), "never-set"));
}

#[test]
fn the_fallback_file_is_0600() {
    let directory = tempfile::tempdir().expect("a temp dir");
    secrets_set(&file_store(directory.path()), "openai", "sk-test".into());

    // The whole point of the fallback is that it is plaintext, so the file's
    // own permissions are the only thing left protecting it.
    let metadata = fs::metadata(directory.path().join("secrets.json")).expect("the store file");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}

#[test]
fn an_unreadable_store_file_reads_as_empty_rather_than_panicking() {
    let directory = tempfile::tempdir().expect("a temp dir");
    fs::write(directory.path().join("secrets.json"), "{ not json").expect("a broken store");

    assert!(!secrets_has(&file_store(directory.path()), "openai"));
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
    secrets_set(&store, APP_SECRET_ACCOUNT, "from-keyring".to_string());
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
        secrets_get(&store, APP_SECRET_ACCOUNT),
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

// --- the regression this plan must not ship ------------------------------

#[test]
fn a_window_reaches_its_own_app_s_store_and_no_other() {
    // Two apps, each in its own process — which is exactly what the hub does,
    // and what a `secret_get(app_id, key)` signature would have undone. There
    // is no argument here that names a store: the window is the whole address,
    // and a webview does not get to choose which window it is.
    let first = tauri::test::mock_app();
    let first_store = new_fake_keyring_store();
    secrets_set(&first_store, "openai", "first-app-value".to_string());
    first.manage(first_store);
    first.manage(crate::manifest::SecretsActions {
        ipc: true,
        bridge: false,
        keys: vec!["openai".to_string()],
    });

    let second = tauri::test::mock_app();
    let second_store = new_fake_keyring_store();
    secrets_set(&second_store, "openai", "second-app-value".to_string());
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
        ),
        Some("first-app-value".to_string())
    );
    assert_eq!(
        secrets_get(
            &store_for_window(&second_handle).expect("the second app's store"),
            "openai"
        ),
        Some("second-app-value".to_string())
    );

    // The declared keys follow the window too, so one app's manifest can never
    // widen what another app's webview may ask for.
    assert_eq!(keys_for_window(&first_handle), vec!["openai".to_string()]);
    assert!(keys_for_window(&second_handle).is_empty());
}
