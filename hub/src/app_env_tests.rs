use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use super::{resolve, Mode};
use crate::{lifecycle::CacheStamp, manifest, paths::Paths, registry::Platform};

/// A stamp for tests that only need `Mode::Launch` to resolve at all, not to
/// care whether it matches — this file's cache-reuse behaviour itself is
/// `lifecycle_tests.rs`'s job (`read_cache_stamp`) and `app_env_tests.rs`'s own
/// wipe-decision tests, not every other test that merely needs a `Launch`.
fn any_cache_stamp() -> CacheStamp {
    CacheStamp {
        app_version: "0.6.0".to_string(),
        snapshot_path: "/apps/demo".to_string(),
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "deadbeef".to_string(),
        },
    }
}

/// One `identifier` per test, named after the test.
///
/// A tempdir isolates everything here **except** the keyring, because the
/// identifier *is* the Secret Service name and that namespace belongs to the
/// login session, not to the test (see `secrets.rs` on why it is a namespace and
/// not a boundary). Two tests sharing an identifier therefore share one
/// `app-secret` account, and `cargo test`'s threads turn that into a race:
/// `remove_tests`' purge deletes accounts under its identifier, which used to be
/// this one — so a resolve here could have its own entry deleted between the
/// write and the read-back, fall through to the file, and come back with a
/// different secret than the resolve before it.
///
/// Named after the test rather than made unique per *run* (a pid, a counter):
/// both isolate, but only this one leaves a bounded set of accounts behind.
/// `make check` stands up a throwaway Secret Service, so nothing accumulates
/// there — a bare `cargo test` writes to the developer's real keyring, and
/// growing it by five entries per run would be a poor trade for the same
/// isolation.
fn identifier_for(test: &str) -> String {
    format!("dev.local.demo-env-{test}")
}

fn manifest_for(identifier: &str, extra: &str) -> manifest::Manifest {
    let json = format!(
        r#"{{
          "product_name": "Demo App",
          "identifier": "{identifier}",
          "project_name": "demo",
          "app_version": "0.6.0"
          {extra}
        }}"#
    );
    manifest::parse(Path::new("tfsapp.config.json"), &json)
        .expect("a valid manifest")
        .manifest
}

/// An installed app's `state_root`, created the way `install::prepare` and
/// `main::prepare` create it — `resolve` itself no longer does, since a dev
/// session's `var/` must not get the same `0700` tightening (see the module
/// header).
fn state_root(paths: &Paths, identifier: &str) -> PathBuf {
    paths
        .create_app_data_dir(identifier)
        .expect("a created data dir")
}

fn value<'a>(vars: &'a [(&'static str, String)], key: &str) -> &'a str {
    vars.iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| panic!("{key} should be injected — CONTRACT.md §3 lists it"))
}

fn var_names(vars: &[(&'static str, String)]) -> BTreeSet<&'static str> {
    vars.iter().map(|(name, _)| *name).collect()
}

#[test]
fn every_variable_the_contract_lists_is_injected() {
    // §3 is the app's whole interface to its host. A variable missing here does
    // not fail loudly: Symfony resolves it to the project's own `.env`, so the
    // app runs against the wrong database and says nothing.
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("every-variable");
    let state_root = state_root(&paths, &identifier);

    let environment = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("the environment resolves");

    for key in [
        "TFS_APP_IDENTIFIER",
        "TFS_APP_VERSION",
        "APP_ENV",
        "APP_DEBUG",
        "APP_SECRET",
        "APP_PORT",
        "APP_ORIGIN",
        "APP_PUBLIC_DIR",
        "APP_CACHE_DIR",
        "APP_BUILD_DIR",
        "APP_LOG_DIR",
        "APP_SESSION_DIR",
        "DATABASE_URL",
        "MESSENGER_TRANSPORT_DSN",
        "MERCURE_URL",
        "MERCURE_PUBLIC_URL",
        "MERCURE_JWT_SECRET",
        "TFS_ASYNC_WORKER",
        "TFS_KEYRING_AVAILABLE",
    ] {
        assert!(!value(&environment.vars, key).is_empty(), "{key} is empty");
    }

    // Conditional on a bridge *running*, and an install starts none. Present
    // but dead would be worse than absent: an app would connect to nothing.
    assert!(
        !environment
            .vars
            .iter()
            .any(|(name, _)| name.starts_with("TFS_BRIDGE")),
        "no bridge runs during an install"
    );
}

#[test]
fn the_data_the_app_reads_hangs_off_identifier_and_nothing_else() {
    // The migration scenario, by construction: a user of the packaged AppImage
    // who installs the same app in the hub must land on the same database. That
    // holds only while the path derives from `identifier` — not from the hub's
    // root, not from the hub-local id.
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("data-dir");
    let expected = state_root(&paths, &identifier);

    let environment = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/whatever"),
        &identifier,
        &expected,
        Mode::Install,
    )
    .expect("the environment resolves");

    assert_eq!(environment.data_dir, expected);
    assert_eq!(
        value(&environment.vars, "DATABASE_URL"),
        format!("sqlite:///{}", expected.join("data/app.db").display())
    );
    // The one directory that *is* the hub's: the installed snapshot.
    assert_eq!(
        value(&environment.vars, "APP_PUBLIC_DIR"),
        "/apps/whatever/public"
    );

    // It holds the app's database and its generated APP_SECRET (CONTRACT.md
    // §6), so the mode is the caller's to have applied before `resolve` ever
    // touches the directory — `state_root` above is `create_app_data_dir`,
    // exactly as `install::prepare` and `main::prepare` call it.
    let mode = std::fs::metadata(&expected)
        .expect("a created data dir")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "{mode:o}");
}

#[test]
fn the_async_worker_toggle_reaches_both_the_transport_and_the_app() {
    // Read from the manifest exactly as the station's dev mode reads it. Had
    // the hub not looked, the same manifest would mean a real worker under one
    // host and `sync://` under the other — which is what the shared contract
    // forbids.
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());

    let identifier = identifier_for("async-worker");
    let state_root = state_root(&paths, &identifier);

    let off = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("it resolves");
    assert_eq!(value(&off.vars, "MESSENGER_TRANSPORT_DSN"), "sync://");
    assert_eq!(value(&off.vars, "TFS_ASYNC_WORKER"), "0");

    let on = resolve(
        &manifest_for(&identifier, r#", "async_worker": true"#),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("it resolves");
    assert_eq!(
        value(&on.vars, "MESSENGER_TRANSPORT_DSN"),
        "doctrine://default?queue_name=async"
    );
    assert_eq!(value(&on.vars, "TFS_ASYNC_WORKER"), "1");
}

#[test]
fn a_pinned_port_is_honoured_and_an_absent_one_is_picked() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());

    let identifier = identifier_for("ports");
    let state_root = state_root(&paths, &identifier);

    let pinned = resolve(
        &manifest_for(&identifier, r#", "app_port": 8123"#),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("it resolves");
    assert_eq!(value(&pinned.vars, "APP_PORT"), "8123");
    assert_eq!(value(&pinned.vars, "APP_ORIGIN"), "http://127.0.0.1:8123");

    let dynamic = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("it resolves");
    let picked: u16 = value(&dynamic.vars, "APP_PORT")
        .parse()
        .expect("a port number");
    assert_ne!(picked, 0, "a picked port is one something can bind");
}

#[test]
fn the_secret_is_the_same_one_the_next_command_will_read() {
    // Anything Symfony signs during an install has to keep validating after it.
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    // One identifier across both resolves, or this would compare two different
    // apps' secrets and pass for the wrong reason.
    let identifier = identifier_for("stable-secret");
    let state_root = state_root(&paths, &identifier);

    let first = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("it resolves");
    let second = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &state_root,
        Mode::Install,
    )
    .expect("it resolves");

    assert_eq!(
        value(&first.vars, "APP_SECRET"),
        value(&second.vars, "APP_SECRET")
    );
    // The Mercure secret is the opposite case: internal to one process image,
    // never persisted, so a fresh one every time is strictly better.
    assert_ne!(
        value(&first.vars, "MERCURE_JWT_SECRET"),
        value(&second.vars, "MERCURE_JWT_SECRET")
    );
}

#[test]
fn a_dev_launch_roots_every_app_path_under_var() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let identifier = format!("dev.{}", identifier_for("dev-var-root"));
    let state_root = project.path().join("var");

    let environment = resolve(
        &manifest_for(&identifier_for("dev-var-root"), ""),
        project.path(),
        &identifier,
        &state_root,
        Mode::Dev,
    )
    .expect("the dev environment resolves");

    assert_eq!(environment.data_dir, state_root);
    for key in [
        "APP_CACHE_DIR",
        "APP_BUILD_DIR",
        "APP_LOG_DIR",
        "APP_SESSION_DIR",
    ] {
        let path = value(&environment.vars, key);
        assert!(
            Path::new(path).starts_with(&state_root),
            "{key} ({path}) should be under {}",
            state_root.display()
        );
    }
    assert_eq!(
        value(&environment.vars, "DATABASE_URL"),
        format!("sqlite:///{}", state_root.join("data/app.db").display())
    );
    assert_eq!(value(&environment.vars, "APP_ENV"), "dev");
    assert_eq!(value(&environment.vars, "APP_DEBUG"), "1");
    // The prefix travels as the runtime identifier, not just the identity's:
    // `TFS_APP_IDENTIFIER` is what the app itself reads.
    assert_eq!(value(&environment.vars, "TFS_APP_IDENTIFIER"), identifier);

    // Fixed and throwaway, never the keyring-backed value an installed launch
    // resolves — see `DEV_APP_SECRET`'s own doc.
    assert_eq!(
        value(&environment.vars, "APP_SECRET"),
        super::DEV_APP_SECRET
    );

    // Named, never created by this call alone up front — but by the time
    // `resolve` returns, its five subdirectories exist under `var/` and
    // nowhere else in the project.
    assert!(state_root.join("data").is_dir());
    assert!(state_root.join("cache").is_dir());
    assert!(!project.path().join("cache").exists());
}

#[test]
fn a_dev_launch_never_wipes_cache_or_build() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let identifier = format!("dev.{}", identifier_for("dev-no-wipe"));
    let state_root = project.path().join("var");
    std::fs::create_dir_all(state_root.join("cache")).expect("a pre-existing cache dir");
    std::fs::write(state_root.join("cache/marker"), "warm").expect("a marker file");

    resolve(
        &manifest_for(&identifier_for("dev-no-wipe"), ""),
        project.path(),
        &identifier,
        &state_root,
        Mode::Dev,
    )
    .expect("the dev environment resolves");

    assert!(
        state_root.join("cache/marker").is_file(),
        "dev must never wipe a warm cache — Symfony's own container invalidation is the \
         mechanism, not a wipe on every relaunch"
    );
}

// --- the cache stamp decides the launch-time wipe (plan 024) ---------------

#[test]
fn a_matching_cache_stamp_keeps_cache_and_build() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("matching-stamp-keeps-cache");
    let installed_state_root = state_root(&paths, &identifier);
    std::fs::create_dir_all(installed_state_root.join("cache")).expect("a pre-existing cache dir");
    std::fs::write(installed_state_root.join("cache/marker"), "warm").expect("a marker file");
    std::fs::create_dir_all(installed_state_root.join("data")).expect("a data dir");

    let stamp = any_cache_stamp();
    crate::lifecycle::write_cache_stamp(&installed_state_root.join("data"), &stamp)
        .expect("a written stamp");

    resolve(
        &manifest_for(&identifier, ""),
        Path::new(&stamp.snapshot_path),
        &identifier,
        &installed_state_root,
        Mode::Launch(stamp.clone()),
    )
    .expect("the launch environment resolves");

    assert!(
        installed_state_root.join("cache/marker").is_file(),
        "a matching stamp must keep the warm cache rather than wipe it"
    );
}

#[test]
fn a_mismatched_cache_stamp_wipes_cache_and_build() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("mismatched-stamp-wipes-cache");
    let installed_state_root = state_root(&paths, &identifier);
    std::fs::create_dir_all(installed_state_root.join("cache")).expect("a pre-existing cache dir");
    std::fs::write(installed_state_root.join("cache/marker"), "warm").expect("a marker file");
    std::fs::create_dir_all(installed_state_root.join("data")).expect("a data dir");

    let recorded = any_cache_stamp();
    crate::lifecycle::write_cache_stamp(&installed_state_root.join("data"), &recorded)
        .expect("a written stamp");
    let mut expected = recorded.clone();
    expected.app_version = "0.7.0".to_string();

    resolve(
        &manifest_for(&identifier, ""),
        Path::new(&recorded.snapshot_path),
        &identifier,
        &installed_state_root,
        Mode::Launch(expected),
    )
    .expect("the launch environment resolves");

    assert!(
        !installed_state_root.join("cache/marker").exists(),
        "a mismatched app_version must wipe the stale cache rather than reuse it"
    );
}

#[test]
fn no_cache_stamp_at_all_wipes_cache_and_build() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("absent-stamp-wipes-cache");
    let installed_state_root = state_root(&paths, &identifier);
    std::fs::create_dir_all(installed_state_root.join("cache")).expect("a pre-existing cache dir");
    std::fs::write(installed_state_root.join("cache/marker"), "warm").expect("a marker file");
    // No `data/cache.json` written at all — every app installed before plan 024,
    // or a first launch that raced a still-running install/update.

    resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &installed_state_root,
        Mode::Launch(any_cache_stamp()),
    )
    .expect("the launch environment resolves");

    assert!(
        !installed_state_root.join("cache/marker").exists(),
        "no stamp at all must wipe rather than trust an unrecorded cache"
    );
}

#[test]
fn the_dev_secrets_store_service_carries_the_prefix() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let bare_identifier = identifier_for("dev-secrets-service");
    let prefixed = format!("dev.{bare_identifier}");
    let state_root = project.path().join("var");

    let environment = resolve(
        &manifest_for(&bare_identifier, ""),
        project.path(),
        &prefixed,
        &state_root,
        Mode::Dev,
    )
    .expect("the dev environment resolves");

    // A dev session's declared secrets and the same project's installed
    // secrets must never resolve to the same keyring entry — the trap the
    // station's own `dev_secrets_service` already solved, carried over here as
    // the whole runtime identity rather than a service string built ad hoc.
    crate::secrets::secrets_set(&environment.secret_store, "probe", "dev-value".to_string());
    let installed_store = crate::secrets::new_store(&bare_identifier, &state_root.join("data"));
    assert_ne!(
        crate::secrets::secrets_get(&installed_store, "probe").as_deref(),
        Some("dev-value"),
        "a dev session's secret must not be visible under the installed app's own service"
    );
}

#[test]
fn dev_and_installed_inject_the_same_set_of_variable_names() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("same-variable-names");
    let installed_state_root = state_root(&paths, &identifier);

    let installed = resolve(
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        &identifier,
        &installed_state_root,
        Mode::Launch(any_cache_stamp()),
    )
    .expect("the installed environment resolves");

    let project = tempfile::tempdir().expect("a temp project dir");
    let dev_identifier = format!("dev.{identifier}");
    let dev_state_root = project.path().join("var");

    let dev = resolve(
        &manifest_for(&identifier, ""),
        project.path(),
        &dev_identifier,
        &dev_state_root,
        Mode::Dev,
    )
    .expect("the dev environment resolves");

    assert_eq!(
        var_names(&installed.vars),
        var_names(&dev.vars),
        "§3 is one list — dev and installed must inject the same variable names, \
         never a different set"
    );
}
