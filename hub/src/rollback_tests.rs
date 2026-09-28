use std::{fs, path::Path, path::PathBuf};

use super::{
    marker_path, rollback, write_marker, RollbackError, RollbackMarker, MARKER_FORMAT_VERSION,
};
use crate::{
    lifecycle::{self, RollbackAnchor},
    paths::Paths,
    registry::{self, Platform, RegistryEntry, Source, SourceKind, State},
    update::test_stop,
};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A registry entry recording `dev.local.demo` at `0.7.0`, sourced from
/// `location` — the state a real `update` would have left behind, without
/// paying for one. Self-contained per this crate's own convention rather than
/// imported from `update_tests.rs`.
fn seeded_entry(location: &str) -> RegistryEntry {
    RegistryEntry {
        id: "demo".to_string(),
        identifier: "dev.local.demo".to_string(),
        source: Source {
            kind: SourceKind::LocalPath,
            location: location.to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "0.7.0".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        app_port: None,
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "a1b2c3d4".repeat(8),
        },
        state: State::Ready,
        installed_at: registry::now_timestamp(),
        updated_at: registry::now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

fn seed_registry(paths: &Paths, entry: RegistryEntry) {
    registry::update(paths, |registry| registry.upsert(entry)).expect("a seeded registry");
}

fn app_tree(root: &Path, marker: &str) {
    fs::create_dir_all(root).expect("an app dir");
    fs::write(root.join("marker"), marker).expect("a marker file");
}

fn write_anchor(data_subdir: &Path) {
    fs::create_dir_all(data_subdir).expect("a data subdir");
    lifecycle::write_rollback_anchor(
        data_subdir,
        &RollbackAnchor {
            app_version: "0.6.0".to_string(),
            source_revision: "sha256:previous".to_string(),
            created_at: registry::now_timestamp(),
        },
    )
    .expect("a seeded anchor");
}

// --- refusals ----------------------------------------------------------

#[test]
fn rolling_back_an_unregistered_id_refuses() {
    let (_base, paths) = temp_paths();

    let error = rollback(&paths, "demo", true).expect_err("nothing is installed");
    assert!(
        matches!(error, RollbackError::NotInstalled { .. }),
        "{error}"
    );
}

#[test]
fn refuses_with_no_anchor_at_all_naming_all_three_halves() {
    let (_base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry("/dev/null"));

    let error = rollback(&paths, "demo", true).expect_err("a fresh install has no anchor");
    let RollbackError::NoAnchor { missing, .. } = error else {
        panic!("expected NoAnchor, got {error}");
    };
    assert_eq!(missing.len(), 3, "{missing:?}");
}

#[test]
fn refuses_when_only_the_retained_tree_is_missing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry("/dev/null"));
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    write_anchor(&data_subdir);
    fs::write(data_subdir.join("app.db.pre-update"), b"snapshot").expect("a db snapshot");
    // No `apps/demo.previous` tree.

    let error = rollback(&paths, "demo", true).expect_err("the tree half is missing");
    let RollbackError::NoAnchor { missing, .. } = error else {
        panic!("expected NoAnchor, got {error}");
    };
    assert_eq!(missing, vec!["the retained previous version of its code"]);
}

#[test]
fn refuses_when_only_the_database_snapshot_is_missing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry("/dev/null"));
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    write_anchor(&data_subdir);
    let app_dir = paths.app_dir("demo").expect("an app dir");
    app_tree(&lifecycle::previous_tree_path(&app_dir), "retained");
    // No `app.db.pre-update`.

    let error = rollback(&paths, "demo", true).expect_err("the snapshot half is missing");
    let RollbackError::NoAnchor { missing, .. } = error else {
        panic!("expected NoAnchor, got {error}");
    };
    assert_eq!(missing, vec!["its pre-update database snapshot"]);
}

#[test]
fn refuses_when_only_the_rollback_record_is_missing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry("/dev/null"));
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db.pre-update"), b"snapshot").expect("a db snapshot");
    let app_dir = paths.app_dir("demo").expect("an app dir");
    app_tree(&lifecycle::previous_tree_path(&app_dir), "retained");
    // No `rollback.json`.

    let error = rollback(&paths, "demo", true).expect_err("the record half is missing");
    let RollbackError::NoAnchor { missing, .. } = error else {
        panic!("expected NoAnchor, got {error}");
    };
    assert_eq!(missing, vec!["its rollback record (rollback.json)"]);
}

// --- the round trip, against a real update ------------------------------

/// A fixture app whose `bin/console` records what it was asked to do, and
/// fails on the word `boom` — `update_tests.rs`'s own fixture, duplicated
/// per this crate's "each `_tests.rs` file is self-contained" convention.
fn runnable_app_tree(root: &Path, version: &str, commands: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{version}",
              "commands": {commands}
            }}"#
        ),
    )
    .expect("a manifest");
    fs::write(root.join("composer.json"), "{}").expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(
        root.join("bin/console"),
        r#"<?php
        $arguments = array_slice($argv, 1);
        exit(in_array('boom', $arguments, true) ? 1 : 0);
        "#,
    )
    .expect("a console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

/// The bundled interpreter and Composer, or a reason to skip — this crate's
/// usual gate, duplicated rather than imported.
fn resources_present() -> bool {
    let missing: Vec<_> = [
        crate::platform::bundled_frankenphp(),
        crate::php::bundled_composer(),
    ]
    .into_iter()
    .filter(|candidates| !candidates.iter().any(|path| path.is_file()))
    .collect();

    for candidates in &missing {
        eprintln!(
            "skipped: none of {candidates:?} are there — run `make resources` to cover this one"
        );
    }
    missing.is_empty()
}

#[test]
fn a_successful_update_can_be_rolled_back_end_to_end() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");
    let before = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the seeded entry");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    runnable_app_tree(source.path(), "0.7.0", "{}");
    crate::update::update(&paths, "demo", None, false, true, "0.1.0").expect("the update applies");

    // Data written after the update, which the rollback must set aside
    // rather than silently drop.
    fs::write(data_subdir.join("app.db"), b"post-update-bytes").expect("post-update writes");

    assert!(rollback(&paths, "demo", true).expect("it rolls back"));

    let app_dir = paths.app_dir("demo").expect("an app dir");
    assert!(
        fs::read_to_string(app_dir.join("tfsapp.config.json"))
            .expect("the restored manifest")
            .contains("0.6.0"),
        "the tree's own manifest must show the restored version"
    );
    assert_eq!(
        lifecycle::read_data_version(&data_subdir).expect("a readable record"),
        Some("0.6.0".to_string()),
        "data/config.json must show the restored version"
    );

    let after = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(after.app_version, "0.6.0");
    assert_eq!(after.source_revision, before.source_revision);

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the restored database"),
        b"pre-update-bytes"
    );
    let rescue = fs::read_dir(&data_subdir)
        .expect("the data directory")
        .map(|entry| entry.expect("a directory entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("app.db.rescue-"))
        })
        .expect("the rescue dump must exist at its printed path");
    assert!(
        rescue.is_file(),
        "the rescue dump must exist at its printed path"
    );
    assert_eq!(
        fs::read(&rescue).expect("the rescued database"),
        b"post-update-bytes"
    );

    // The anchor is consumed, not rotated.
    assert!(!lifecycle::previous_tree_path(&app_dir).exists());
    assert!(!lifecycle::db_snapshot_path(&data_subdir, "app.db").exists());
    assert!(!lifecycle::rollback_anchor_path(&data_subdir).exists());
}

#[test]
fn a_rollback_leaves_a_stale_cache_stamp_that_mismatches_the_restored_version() {
    // Plan 024 step 5, invalidation path 2: unlike `update`'s own revert
    // (which discards `cache.json` outright on failure), a *successful*
    // rollback never touches it — the stamp `install::prepare`'s warm-up
    // wrote for the version the update moved *to* is left stale next to a
    // tree just restored to the version it moved *from*. Its `app_version`
    // alone must be enough for the next launch to catch the mismatch and
    // rebuild, with no code in `rollback.rs` needing to clear it.
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    // `update`'s own `snapshot_db` only copies files that exist — the
    // rollback anchor's database half needs one to snapshot, same as the
    // round-trip test above.
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    runnable_app_tree(source.path(), "0.7.0", "{}");
    crate::update::update(&paths, "demo", None, false, true, "0.1.0").expect("the update applies");

    assert!(rollback(&paths, "demo", true).expect("it rolls back"));

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let cache_dir = base.path().join("TFSApp/dev.local.demo/cache");
    let entry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(
        entry.app_version, "0.6.0",
        "the restored version, which `expected` below must be built from"
    );

    let expected = lifecycle::CacheStamp {
        app_version: entry.app_version.clone(),
        snapshot_path: tfsapp_core::sidecar::path_to_string(&app_dir),
        platform: entry.platform.clone(),
    };

    match lifecycle::read_cache_stamp(&data_subdir, &cache_dir, &expected) {
        lifecycle::CacheStatus::Mismatch { reason } => {
            assert!(
                reason.contains("0.7.0") && reason.contains("0.6.0"),
                "the mismatch must name both versions: {reason}"
            );
        }
        other => panic!("expected a mismatch on the stale stamp's app_version, got {other:?}"),
    }
}

// --- interrupted rollbacks, resumed by running the command again --------

/// Every boundary a kill can strike between two of `finish`'s steps, from
/// the marker's own write to the last discard before its removal.
const ROLLBACK_STOP_POINTS: [&str; 7] = [
    "rollback_marked",
    "rollback_db_restored",
    "rollback_tree_removed",
    "rollback_tree_restored",
    "rollback_version_written",
    "rollback_registry_written",
    "rollback_anchor_discarded",
];

/// Install at `0.6.0`, update to `0.7.0`, then write post-update database
/// bytes — the exact state `a_successful_update_can_be_rolled_back_end_to_end`
/// starts its rollback from, factored so the stop-point test below can have
/// one per boundary.
fn updated_for_rollback_stop() -> (tempfile::TempDir, Paths, PathBuf) {
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    crate::install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    runnable_app_tree(source.path(), "0.7.0", "{}");
    crate::update::update(&paths, "demo", None, false, true, "0.1.0").expect("the update applies");

    // Data written after the update, which the rollback must set aside
    // rather than silently drop.
    fs::write(data_subdir.join("app.db"), b"post-update-bytes").expect("post-update writes");

    (base, paths, data_subdir)
}

#[test]
fn a_killed_rollback_is_refused_by_every_other_command_and_finished_by_rerunning_it() {
    if !resources_present() {
        return;
    }
    for point in ROLLBACK_STOP_POINTS {
        let (base, paths, data_subdir) = updated_for_rollback_stop();
        let app_dir = paths.app_dir("demo").expect("an app dir");
        let data_dir = base.path().join("TFSApp/dev.local.demo");

        test_stop::arm(point);
        let result = rollback(&paths, "demo", true);
        test_stop::disarm();
        assert!(result.is_err(), "{point}: the stop point should have fired");

        // The marker refuses every operation but rollback — including the
        // two a user might try to escape with.
        for operation in ["export", "repair"] {
            match crate::lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", operation) {
                Err(crate::lifecycle_gate::GateError::RollbackRequired { .. }) => {}
                Err(other) => panic!("{point}: {operation} must be refused, not {other}"),
                Ok(_) => panic!("{point}: the marker must refuse the {operation}"),
            }
        }

        // Resumed with no prompt: `false` proves none is asked, since the
        // first run was already confirmed and there is nothing to go back
        // to.
        assert!(
            rollback(&paths, "demo", false)
                .unwrap_or_else(|error| panic!("{point}: the resume failed: {error}")),
            "{point}: the resume must report success"
        );

        // The same end state as an uninterrupted rollback.
        assert!(
            fs::read_to_string(app_dir.join("tfsapp.config.json"))
                .expect("the restored manifest")
                .contains("0.6.0"),
            "{point}: the tree's own manifest must show the restored version"
        );
        assert_eq!(
            fs::read(data_subdir.join("app.db")).expect("the restored database"),
            b"pre-update-bytes",
            "{point}: the restored database"
        );
        assert_eq!(
            lifecycle::read_data_version(&data_subdir).expect("a readable record"),
            Some("0.6.0".to_string()),
            "{point}: data/config.json must show the restored version"
        );
        let entry = registry::load(&paths)
            .expect("a readable registry")
            .get("demo")
            .cloned()
            .expect("the entry survives");
        assert_eq!(entry.app_version, "0.6.0", "{point}");

        // Nothing of the interruption or the anchor remains.
        assert!(!marker_path(&data_dir).exists(), "{point}");
        assert!(
            !lifecycle::db_snapshot_path(&data_subdir, "app.db").exists(),
            "{point}"
        );
        assert!(
            !lifecycle::rollback_anchor_path(&data_subdir).exists(),
            "{point}"
        );
        assert!(!lifecycle::previous_tree_path(&app_dir).exists(), "{point}");
    }
}

#[test]
fn a_marker_with_neither_tree_left_reports_the_lost_tree_and_keeps_the_marker() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry("/dev/null"));
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    fs::create_dir_all(&data_dir).expect("a data dir");
    write_marker(
        &data_dir,
        &RollbackMarker {
            format_version: MARKER_FORMAT_VERSION,
            target_version: "0.6.0".to_string(),
            source_revision: "sha256:previous".to_string(),
            rescue_path: None,
        },
    )
    .expect("a seeded marker");

    let error = rollback(&paths, "demo", true).expect_err("neither tree exists");
    assert!(matches!(error, RollbackError::TreeLost { .. }), "{error}");
    // The marker stays: the situation is still nameable, and the refusal
    // still names the command that owns it.
    assert!(marker_path(&data_dir).exists());
}
