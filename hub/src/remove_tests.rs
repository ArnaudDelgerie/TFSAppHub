use std::fs;

use super::{plan, remove, RemoveError, APP_SECRET_ACCOUNT, PROBE_ACCOUNT};
use crate::{
    identity::Identity,
    paths::Paths,
    registry::{now_timestamp, Platform, Registry, RegistryEntry, Source, SourceKind, State},
};

/// This module's own `identifier`, and deliberately not the one every other
/// module uses.
///
/// A tempdir isolates the files; nothing isolates the keyring, whose service
/// name *is* the identifier and whose namespace belongs to the login session.
/// These tests are the only ones in the suite that **delete** keyring accounts,
/// so sharing an identifier with `app_env_tests` or `install_tests` meant a
/// purge here could remove an `app-secret` another test had just written, in
/// another thread, and make it read back a different secret.
const IDENTIFIER: &str = "dev.local.demo-remove";

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// An installed snapshot whose manifest declares two secret keys.
fn installed_snapshot(paths: &Paths, id: &str) {
    let app_dir = paths.app_dir(id).expect("an app dir");
    fs::create_dir_all(&app_dir).expect("an installed tree");
    fs::write(
        app_dir.join("tfsapp.config.json"),
        format!(
            r#"{{
          "product_name": "Demo App",
          "identifier": "{IDENTIFIER}",
          "project_name": "demo",
          "app_version": "0.6.0",
          "actions": {{"secrets": {{"ipc": true, "keys": ["openai", "anthropic"]}}}}
        }}"#
        ),
    )
    .expect("a manifest");
}

#[test]
fn a_plain_remove_keeps_the_data_and_a_purge_names_every_zone() {
    // The whole reason the two forms exist: someone reinstalling a broken app
    // must not lose their database to a command that only had to replace a tree.
    let (base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");

    let kept = plan(&paths, "demo", IDENTIFIER, false).expect("a plan");
    assert_eq!(kept.app_dir, paths.app_dir("demo").expect("an app dir"));
    assert!(
        kept.keyring_accounts.is_empty(),
        "a plain remove never touches the keyring"
    );

    let purged = plan(&paths, "demo", IDENTIFIER, true).expect("a plan");
    assert_eq!(purged.data_dir, base.path().join("TFSApp").join(IDENTIFIER));
    // A sibling of TFSApp/, not a child — WebKit derives it from the GTK app id
    // and has never heard of this project's vendor folder.
    assert_eq!(purged.webkit_data_dir, base.path().join(IDENTIFIER));
    assert_eq!(
        purged.keyring_accounts,
        vec![
            APP_SECRET_ACCOUNT.to_string(),
            PROBE_ACCOUNT.to_string(),
            "openai".to_string(),
            "anthropic".to_string(),
        ]
    );
}

#[test]
fn an_unreadable_manifest_costs_the_declared_keys_and_nothing_else() {
    // The manifest is inside the tree being deleted, so it can legitimately be
    // missing by the time a second `remove` runs. The hub's own two accounts
    // need no manifest to be known.
    let (_base, paths) = temp_paths();

    let purged = plan(&paths, "demo", IDENTIFIER, true).expect("a plan");

    assert_eq!(
        purged.keyring_accounts,
        vec![APP_SECRET_ACCOUNT.to_string(), PROBE_ACCOUNT.to_string()]
    );
}

#[test]
fn removing_something_that_is_not_installed_says_where_to_look() {
    let (_base, paths) = temp_paths();

    let error = remove(&paths, "demo", false, true).expect_err("nothing is installed");

    assert!(matches!(error, RemoveError::NotInstalled { .. }), "{error}");
    assert!(error.to_string().contains("list"), "{error}");
}

#[test]
fn a_remove_drops_the_tree_and_the_entry_and_keeps_the_data() {
    let (base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    fs::create_dir_all(data_dir.join("data")).expect("an app data dir");
    fs::write(data_dir.join("data/app.db"), "not really a database").expect("a database");

    registry::save_entry(&paths, "demo");

    assert!(remove(&paths, "demo", false, true).expect("it removes"));

    assert!(!paths.app_dir("demo").expect("an app dir").exists());
    assert!(crate::registry::load(&paths)
        .expect("a readable registry")
        .apps
        .is_empty());
    // The point of the plain form.
    assert!(data_dir.join("data/app.db").is_file());
    assert!(base.path().exists());
}

#[test]
fn a_remove_takes_the_retained_previous_tree_with_it() {
    // The rollback anchor's tree half: left behind by an `update`, and
    // exactly the kind of internal bookkeeping a plain `remove` must not
    // strand — a later install under the same `id` has no legitimate reason
    // to find a `.previous` sibling waiting for it.
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    registry::save_entry(&paths, "demo");
    let previous =
        crate::lifecycle::previous_tree_path(&paths.app_dir("demo").expect("an app dir"));
    fs::create_dir_all(&previous).expect("a retained previous tree");

    assert!(remove(&paths, "demo", false, true).expect("it removes"));

    assert!(
        !previous.exists(),
        "the retained tree must go with the app dir"
    );
}

#[test]
fn a_purge_takes_the_data_with_it() {
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    let webkit_dir = paths.webkit_data_dir(IDENTIFIER).expect("a webkit dir");
    fs::create_dir_all(data_dir.join("data")).expect("an app data dir");
    fs::write(data_dir.join("data/app.db"), "not really a database").expect("a database");
    fs::create_dir_all(&webkit_dir).expect("a webkit data dir");

    registry::save_entry(&paths, "demo");

    assert!(remove(&paths, "demo", true, true).expect("it purges"));

    assert!(!data_dir.exists(), "the data dir goes with --purge");
    assert!(!webkit_dir.exists(), "so does WebKit's own");
}

#[test]
fn a_remove_takes_the_entry_it_wrote_and_leaves_the_stable_copy_alone() {
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    registry::save_entry(&paths, "demo");

    let identity = Identity {
        identifier: IDENTIFIER.to_string(),
        product_name: "Demo App".to_string(),
        icon_path: None,
    };
    let hub_executable = paths.hub_executable_path();
    fs::create_dir_all(hub_executable.parent().expect("a bin dir")).expect("the hub's own bin dir");
    fs::write(&hub_executable, "not really a binary").expect("the stable hub copy");
    crate::desktop::write("demo", &identity, &hub_executable, &paths)
        .expect("the entry is written");
    let entry = paths
        .desktop_entry_path(IDENTIFIER)
        .expect("a safe identifier");
    assert!(entry.is_file(), "the entry must exist before removal");

    assert!(remove(&paths, "demo", false, true).expect("it removes"));

    assert!(!entry.exists(), "the entry this hub wrote must be gone");
    assert!(
        hub_executable.is_file(),
        "the stable copy is the hub's own file, shared by every app's entry"
    );
}

#[test]
fn a_remove_leaves_an_unmarked_entry_untouched() {
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    registry::save_entry(&paths, "demo");

    let entry = paths
        .desktop_entry_path(IDENTIFIER)
        .expect("a safe identifier");
    fs::create_dir_all(paths.applications_dir()).expect("the applications dir");
    fs::write(
        &entry,
        "[Desktop Entry]\nType=Application\nName=Hand Written\n",
    )
    .expect("a hand-written entry with no marker");

    assert!(remove(&paths, "demo", false, true).expect("it removes"));

    assert!(
        entry.is_file(),
        "an entry this hub did not write must survive removal"
    );
}

/// Registry fixtures, kept out of the tests above so they read as what they are
/// about rather than as struct literals.
mod registry {
    use super::*;

    pub fn save_entry(paths: &Paths, id: &str) {
        let mut registry = Registry::default();
        registry.upsert(RegistryEntry {
            id: id.to_string(),
            identifier: IDENTIFIER.to_string(),
            source: Source {
                kind: SourceKind::LocalPath,
                location: "/home/arnaud/Dev/Demo".to_string(),
                reference: None,
                reference_kind: None,
                index: None,
            },
            app_version: "0.6.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            app_port: None,
            platform: Platform {
                php_version: "8.5".to_string(),
                extensions_hash: "a1b2c3d4".repeat(8),
            },
            state: State::Ready,
            installed_at: now_timestamp(),
            updated_at: now_timestamp(),
            unknown: serde_json::Map::new(),
        });
        crate::registry::save(paths, &registry).expect("a written registry");
    }
}
