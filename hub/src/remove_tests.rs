use std::fs;

use super::{
    keyring_note_path, orphaned_data, plan, plan_orphan, purge_identifier, read_keyring_note,
    remove, render_orphans, write_keyring_note, KeyringNote, OrphanedData, RemoveError,
    APP_SECRET_ACCOUNT, PROBE_ACCOUNT,
};
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

/// The orphan-side tests' own identifier, distinct from [`IDENTIFIER`] for
/// the same reason that one is distinct from every other module's: the ones
/// below that reach [`purge_identifier`]'s execution path delete real
/// keyring accounts, under a service name nothing else in the suite touches.
const ORPHAN_IDENTIFIER: &str = "dev.local.demo-orphan-remove";

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
fn a_plan_carries_every_zone_and_the_full_declared_keyring_set() {
    // `plan` computes the same thing for both forms now (plan 023 step 4): a
    // plain `remove` needs the full declared set as much as `--purge` does,
    // to write it into the note it leaves behind.
    let (base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");

    let planned = plan(&paths, "demo", IDENTIFIER).expect("a plan");

    assert_eq!(
        planned.app_dir,
        Some(paths.app_dir("demo").expect("an app dir"))
    );
    assert_eq!(
        planned.data_dir,
        base.path().join("TFSApp").join(IDENTIFIER)
    );
    // A sibling of TFSApp/, not a child — WebKit derives it from the GTK app id
    // and has never heard of this project's vendor folder.
    assert_eq!(planned.webkit_data_dir, base.path().join(IDENTIFIER));
    assert_eq!(
        planned.keyring_accounts,
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

    let planned = plan(&paths, "demo", IDENTIFIER).expect("a plan");

    assert_eq!(
        planned.keyring_accounts,
        vec![APP_SECRET_ACCOUNT.to_string(), PROBE_ACCOUNT.to_string()]
    );
    assert!(
        !planned.keyring_note_found,
        "the purge caveat must name declared accounts it cannot inspect"
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
fn a_live_window_refuses_a_purge() {
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    registry::save_entry(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let pid_file = data_dir.join("sidecar.pid");
    // Held in this test's own process, the same fake `install_tests.rs` and
    // `portability_tests.rs` use for their own busy-guard tests: `flock` is
    // per open file description, so a second, fresh open of the same path
    // still sees it held.
    let _holder = tfsapp_core::process::try_lock_file(&tfsapp_core::process::lock_path(&pid_file))
        .expect("no I/O error")
        .expect("the lock is free to take");

    let error = remove(&paths, "demo", true, true).expect_err("a live window owns this data dir");

    assert!(matches!(error, RemoveError::StillRunning { .. }), "{error}");
    assert!(error.to_string().contains("demo"), "{error}");
}

#[test]
fn a_live_window_refuses_a_plain_remove() {
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    registry::save_entry(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let pid_file = data_dir.join("sidecar.pid");
    let _holder = tfsapp_core::process::try_lock_file(&tfsapp_core::process::lock_path(&pid_file))
        .expect("no I/O error")
        .expect("the lock is free to take");

    let error = remove(&paths, "demo", false, true)
        .expect_err("a live window must keep its served tree intact");

    assert!(matches!(error, RemoveError::StillRunning { .. }), "{error}");
    assert!(error.to_string().contains("demo"), "{error}");
    assert!(paths.app_dir("demo").expect("an app dir").exists());
}

#[test]
fn an_active_run_command_now_refuses_a_purge() {
    // The gap this step closes: `remove --purge` used to probe only
    // `sidecar.pid` and ignore `run.lock`, so an active `run` command did not
    // stop a purge from deleting the data underneath it.
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    registry::save_entry(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let run_lock_path = data_dir.join("run.lock");
    let _holder = tfsapp_core::process::try_lock_file(&run_lock_path)
        .expect("no I/O error")
        .expect("the lock is free to take");
    fs::write(&run_lock_path, "migrate\n1234").expect("a run.lock record");

    let error =
        remove(&paths, "demo", true, true).expect_err("an active run command owns this data dir");

    let message = error.to_string();
    assert!(message.contains("migrate"), "{message}");
    assert!(message.contains("run --stop demo"), "{message}");
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

#[test]
fn an_orphan_is_listed_and_an_installed_app_is_not() {
    let (_base, paths) = temp_paths();
    fs::create_dir_all(
        paths
            .app_data_dir("com.example.orphan")
            .expect("a data dir")
            .join("data"),
    )
    .expect("an orphan data dir");
    fs::create_dir_all(
        paths
            .app_data_dir(IDENTIFIER)
            .expect("a data dir")
            .join("data"),
    )
    .expect("an installed app's data dir");
    registry::save_entry(&paths, "demo");

    let loaded = crate::registry::load(&paths).expect("a readable registry");
    let orphans = orphaned_data(&paths, &loaded).expect("an enumeration");

    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].identifier, "com.example.orphan");
}

#[test]
fn hub_dir_is_never_a_candidate() {
    let (_base, paths) = temp_paths();
    fs::create_dir_all(paths.hub_root()).expect("the hub root");

    let orphans =
        orphaned_data(&paths, &crate::registry::Registry::default()).expect("an enumeration");

    assert!(orphans.is_empty(), "{orphans:?}");
}

#[test]
fn a_symlink_and_a_plain_file_are_both_skipped() {
    let (base, paths) = temp_paths();
    fs::create_dir_all(paths.vendor_dir()).expect("the vendor dir");
    std::os::unix::fs::symlink(base.path(), paths.vendor_dir().join("com.example.link"))
        .expect("a symlink");
    fs::write(
        paths.vendor_dir().join("com.example.file"),
        b"not a directory",
    )
    .expect("a plain file");

    let orphans =
        orphaned_data(&paths, &crate::registry::Registry::default()).expect("an enumeration");

    assert!(orphans.is_empty(), "{orphans:?}");
}

#[test]
fn a_missing_config_json_lists_as_no_version_recorded() {
    let (_base, paths) = temp_paths();
    fs::create_dir_all(
        paths
            .app_data_dir("com.example.orphan")
            .expect("a data dir"),
    )
    .expect("an orphan data dir with no data/ subdir at all");

    let orphans =
        orphaned_data(&paths, &crate::registry::Registry::default()).expect("an enumeration");

    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].app_version, None);
    assert!(render_orphans(&orphans).contains("no version recorded"));
}

#[test]
fn an_empty_vendor_dir_lists_nothing() {
    let (_base, paths) = temp_paths();

    let orphans =
        orphaned_data(&paths, &crate::registry::Registry::default()).expect("an enumeration");

    assert!(orphans.is_empty());
    assert_eq!(render_orphans(&orphans), "nothing to purge\n");
}

#[test]
fn size_is_the_sum_of_file_lengths() {
    let (_base, paths) = temp_paths();
    let dir = paths
        .app_data_dir("com.example.orphan")
        .expect("a data dir");
    fs::create_dir_all(dir.join("data")).expect("an orphan data dir");
    fs::write(dir.join("data/app.db"), vec![0u8; 42]).expect("a database file");

    let orphans =
        orphaned_data(&paths, &crate::registry::Registry::default()).expect("an enumeration");

    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].size_bytes, 42);
}

#[test]
fn a_webkit_sibling_is_detected() {
    let (_base, paths) = temp_paths();
    fs::create_dir_all(
        paths
            .app_data_dir("com.example.orphan")
            .expect("a data dir"),
    )
    .expect("an orphan data dir");
    fs::create_dir_all(
        paths
            .webkit_data_dir("com.example.orphan")
            .expect("a webkit dir"),
    )
    .expect("a webkit data dir");

    let orphans =
        orphaned_data(&paths, &crate::registry::Registry::default()).expect("an enumeration");

    assert_eq!(orphans.len(), 1);
    assert!(orphans[0].has_webkit_data);
}

#[test]
fn render_orphans_shows_the_recorded_version_and_the_webkit_sibling() {
    let orphans = vec![OrphanedData {
        identifier: "com.example.orphan".to_string(),
        app_version: Some("1.2.0".to_string()),
        size_bytes: 2048,
        has_webkit_data: true,
    }];

    let text = render_orphans(&orphans);

    assert!(text.contains("com.example.orphan"), "{text}");
    assert!(text.contains("1.2.0"), "{text}");
    assert!(text.contains("with WebKit data"), "{text}");
}

#[test]
fn purging_an_installed_identifier_names_remove_purge() {
    let (_base, paths) = temp_paths();
    registry::save_entry(&paths, "demo");

    let error =
        purge_identifier(&paths, IDENTIFIER, true).expect_err("an installed identifier refuses");

    assert!(
        matches!(error, RemoveError::AlreadyInstalled { .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("demo"), "{message}");
    assert!(message.contains("remove demo --purge"), "{message}");
}

#[test]
fn reserved_identifiers_refuse_before_purge_reads_the_registry_or_prompts() {
    for (identifier, directory) in [
        ("hub", "hub's own directory under TFSApp/"),
        ("TFSApp", "shared vendor directory"),
        ("applications", "XDG desktop-entry directory"),
    ] {
        let (_base, paths) = temp_paths();
        fs::create_dir_all(paths.registry_path()).expect("a deliberately unreadable registry");

        let error = purge_identifier(&paths, identifier, false)
            .expect_err("reserved infrastructure must refuse before every later step");

        assert!(
            matches!(&error, RemoveError::ReservedIdentifier { identifier: actual } if actual == identifier),
            "{error}"
        );
        assert!(error.to_string().contains(directory), "{error}");
    }
}

#[test]
fn purging_an_identifier_with_no_data_points_at_bare_purge() {
    let (_base, paths) = temp_paths();

    let error = purge_identifier(&paths, "com.example.nothing", true)
        .expect_err("nothing exists under TFSApp/ for this identifier");

    assert!(matches!(error, RemoveError::NoOrphanData { .. }), "{error}");
    assert!(error.to_string().contains("tfsapp-hub purge"), "{error}");
}

#[test]
fn a_symlinked_data_dir_refuses_the_purge_and_deletes_nothing() {
    let (base, paths) = temp_paths();
    let target = base.path().join("elsewhere");
    fs::create_dir_all(&target).expect("a link target");
    fs::create_dir_all(paths.vendor_dir()).expect("the vendor dir");
    let data_dir = paths.app_data_dir(ORPHAN_IDENTIFIER).expect("a data dir");
    std::os::unix::fs::symlink(&target, &data_dir).expect("a symlink data dir");

    let error = purge_identifier(&paths, ORPHAN_IDENTIFIER, true)
        .expect_err("a symlink is refused, not followed");

    assert!(matches!(error, RemoveError::SymlinkData { .. }), "{error}");
    assert!(
        target.is_dir(),
        "the link's target must survive a refused purge"
    );
}

#[test]
fn a_live_window_refuses_an_orphan_purge() {
    let (_base, paths) = temp_paths();
    let data_dir = paths.app_data_dir(ORPHAN_IDENTIFIER).expect("a data dir");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let pid_file = data_dir.join("sidecar.pid");
    let _holder = tfsapp_core::process::try_lock_file(&tfsapp_core::process::lock_path(&pid_file))
        .expect("no I/O error")
        .expect("the lock is free to take");

    let error = purge_identifier(&paths, ORPHAN_IDENTIFIER, true)
        .expect_err("a live window owns this data dir");

    assert!(matches!(error, RemoveError::StillRunning { .. }), "{error}");
    assert!(error.to_string().contains(ORPHAN_IDENTIFIER), "{error}");
}

#[test]
fn an_orphan_plan_carries_no_id_and_the_two_hub_accounts() {
    let (_base, paths) = temp_paths();

    let orphan = plan_orphan(&paths, ORPHAN_IDENTIFIER).expect("a plan");

    assert_eq!(orphan.id, None);
    assert_eq!(orphan.app_dir, None);
    assert_eq!(
        orphan.keyring_accounts,
        vec![APP_SECRET_ACCOUNT.to_string(), PROBE_ACCOUNT.to_string()]
    );
    assert!(
        !orphan.keyring_note_found,
        "no note was left behind, so the caveat must still apply"
    );
    assert_eq!(
        orphan.data_dir,
        paths.app_data_dir(ORPHAN_IDENTIFIER).expect("a data dir")
    );
}

#[test]
fn declining_a_purge_confirmation_touches_nothing() {
    // stdin is not a terminal under the test harness, and assume_yes is
    // false: `prompt::confirmed` refuses non-interactively, which is exactly
    // the decline path this exercises.
    let (_base, paths) = temp_paths();
    let data_dir = paths.app_data_dir(ORPHAN_IDENTIFIER).expect("a data dir");
    fs::create_dir_all(data_dir.join("data")).expect("an orphan data dir");
    fs::write(data_dir.join("data/app.db"), b"existing").expect("a database");

    let proceeded = purge_identifier(&paths, ORPHAN_IDENTIFIER, false)
        .expect("a non-interactive decline is Ok(false), not an error");

    assert!(!proceeded);
    assert!(
        data_dir.join("data/app.db").is_file(),
        "declining must touch nothing"
    );
}

#[test]
fn an_orphan_purge_takes_the_data_and_the_webkit_dir_with_it() {
    let (_base, paths) = temp_paths();
    let data_dir = paths.app_data_dir(ORPHAN_IDENTIFIER).expect("a data dir");
    let webkit_dir = paths
        .webkit_data_dir(ORPHAN_IDENTIFIER)
        .expect("a webkit dir");
    fs::create_dir_all(data_dir.join("data")).expect("an orphan data dir");
    fs::write(data_dir.join("data/app.db"), b"not really a database").expect("a database");
    fs::create_dir_all(&webkit_dir).expect("a webkit data dir");

    assert!(purge_identifier(&paths, ORPHAN_IDENTIFIER, true).expect("it purges"));

    assert!(!data_dir.exists(), "the data dir goes with the purge");
    assert!(!webkit_dir.exists(), "so does WebKit's own");
}

#[test]
fn the_keyring_note_round_trips_with_an_unknown_key_surviving() {
    let contents = r#"{"accounts":["openai"],"written_by_a_newer_hub":"kept"}"#;

    let note: KeyringNote = serde_json::from_str(contents).expect("valid json");
    assert_eq!(note.accounts, vec!["openai".to_string()]);

    let rendered = serde_json::to_string(&note).expect("a serializable note");
    assert!(rendered.contains("written_by_a_newer_hub"), "{rendered}");
}

#[test]
fn a_retaining_remove_writes_the_manifests_declared_keys_into_a_note() {
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    fs::create_dir_all(data_dir.join("data")).expect("an app data dir");
    registry::save_entry(&paths, "demo");

    assert!(remove(&paths, "demo", false, true).expect("it removes"));

    let note = read_keyring_note(&data_dir).expect("a note was written");
    assert_eq!(
        note,
        vec![
            APP_SECRET_ACCOUNT.to_string(),
            PROBE_ACCOUNT.to_string(),
            "openai".to_string(),
            "anthropic".to_string(),
        ]
    );
}

#[test]
fn a_purge_writes_no_keyring_note() {
    // The directory the note would live in is itself going away — writing
    // one would only be deleted along with it.
    let (_base, paths) = temp_paths();
    installed_snapshot(&paths, "demo");
    let data_dir = paths.app_data_dir(IDENTIFIER).expect("a data dir");
    fs::create_dir_all(data_dir.join("data")).expect("an app data dir");
    registry::save_entry(&paths, "demo");

    assert!(remove(&paths, "demo", true, true).expect("it purges"));

    assert!(!keyring_note_path(&data_dir).exists());
}

#[test]
fn a_purge_with_a_note_deletes_the_declared_accounts() {
    let (_base, paths) = temp_paths();
    let data_dir = paths.app_data_dir(ORPHAN_IDENTIFIER).expect("a data dir");
    fs::create_dir_all(data_dir.join("data")).expect("an orphan data dir");
    write_keyring_note(&data_dir, &["openai".to_string()]).expect("a note is written");

    let entry = keyring::Entry::new(ORPHAN_IDENTIFIER, "openai").expect("a keyring entry");
    entry
        .set_password("secret")
        .expect("a real keyring account");

    assert!(purge_identifier(&paths, ORPHAN_IDENTIFIER, true).expect("it purges"));

    assert!(
        entry.get_password().is_err(),
        "the account this note declared must be gone"
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
        crate::registry::update(paths, |stored| *stored = registry).expect("a written registry");
    }
}
