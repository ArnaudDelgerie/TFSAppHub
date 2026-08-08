use std::{os::unix::fs::PermissionsExt, path::Path};

use super::{resolve, Mode};
use crate::{manifest, paths::Paths};

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

fn value<'a>(vars: &'a [(&'static str, String)], key: &str) -> &'a str {
    vars.iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| panic!("{key} should be injected — CONTRACT.md §3 lists it"))
}

#[test]
fn every_variable_the_contract_lists_is_injected() {
    // §3 is the app's whole interface to its host. A variable missing here does
    // not fail loudly: Symfony resolves it to the project's own `.env`, so the
    // app runs against the wrong database and says nothing.
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    let identifier = identifier_for("every-variable");

    let environment = resolve(
        &paths,
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
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

    let environment = resolve(
        &paths,
        &manifest_for(&identifier, ""),
        Path::new("/apps/whatever"),
        Mode::Install,
    )
    .expect("the environment resolves");

    let expected = base.path().join("TFSApp").join(&identifier);
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
    // §6), so the mode is re-applied on every use, not only at creation.
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

    let off = resolve(
        &paths,
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        Mode::Install,
    )
    .expect("it resolves");
    assert_eq!(value(&off.vars, "MESSENGER_TRANSPORT_DSN"), "sync://");
    assert_eq!(value(&off.vars, "TFS_ASYNC_WORKER"), "0");

    let on = resolve(
        &paths,
        &manifest_for(&identifier, r#", "async_worker": true"#),
        Path::new("/apps/demo"),
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

    let pinned = resolve(
        &paths,
        &manifest_for(&identifier, r#", "app_port": 8123"#),
        Path::new("/apps/demo"),
        Mode::Install,
    )
    .expect("it resolves");
    assert_eq!(value(&pinned.vars, "APP_PORT"), "8123");
    assert_eq!(value(&pinned.vars, "APP_ORIGIN"), "http://127.0.0.1:8123");

    let dynamic = resolve(
        &paths,
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
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

    let first = resolve(
        &paths,
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
        Mode::Install,
    )
    .expect("it resolves");
    let second = resolve(
        &paths,
        &manifest_for(&identifier, ""),
        Path::new("/apps/demo"),
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
