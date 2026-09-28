use std::{fs, path::Path};

use super::{
    discard_tree, repair_at, restore_tree, retain_tree, revert, test_stop, update, update_decision,
    UpdateError, UpdateRefusal,
};
use crate::{
    lifecycle::{previous_tree_path, read_rollback_anchor},
    lifecycle_gate,
    paths::Paths,
    registry::{self, Platform, RegistryEntry, Source, SourceKind, State},
    update_transaction,
};

/// Whether the journal is still there — `update::repair_required`'s own
/// check, inlined here since plan 064 moved the guard into `lifecycle_gate`
/// and deleted the shared fence.
fn journal_present(paths: &Paths) -> bool {
    let installed = registry::load(paths).expect("a readable registry");
    let entry = installed.get("demo").expect("the seeded entry");
    let data_dir = paths.app_data_dir(&entry.identifier).expect("a data dir");
    update_transaction::read(&data_dir)
        .expect("a readable journal")
        .is_some()
}

fn version(text: &str) -> semver::Version {
    semver::Version::parse(text).expect("a semver version")
}

#[test]
fn a_partial_revert_names_its_failed_half_and_never_claims_success() {
    let data_root = tempfile::tempdir().expect("a data directory");
    let data_subdir = data_root.path().join("data");
    let app_dir = tempfile::tempdir()
        .expect("an apps directory")
        .path()
        .join("demo");
    fs::write(&data_subdir, "not a directory").expect("a blocked data directory");

    let outcome = revert(&data_subdir, &app_dir, "0.6.0");
    let error = UpdateError::Reverted {
        detail: "the update command failed.".to_string(),
        outcome,
    };
    let message = error.to_string();

    assert!(message.contains("data version rewrite"), "{message}");
    assert!(message.contains("config.json"), "{message}");
    assert!(message.contains("not put back"), "{message}");
    assert!(
        !message.contains("was put back to its previous state"),
        "{message}"
    );
}

// --- update_decision -------------------------------------------------------

#[test]
fn a_newer_source_applies_the_update_event() {
    assert!(update_decision(Some("0.5.0"), &version("0.6.0")).is_ok());
}

#[test]
fn an_equal_source_refuses() {
    let error =
        update_decision(Some("0.6.0"), &version("0.6.0")).expect_err("an equal record refuses");
    assert_eq!(
        error,
        UpdateRefusal::Equal {
            version: version("0.6.0")
        }
    );
}

#[test]
fn a_downgrade_refuses() {
    // Update never goes backwards, whatever the reason to want to.
    let error =
        update_decision(Some("0.6.0"), &version("0.5.0")).expect_err("a downgrade always refuses");
    assert_eq!(
        error,
        UpdateRefusal::Downgrade {
            recorded: version("0.6.0"),
            source: version("0.5.0"),
        }
    );
}

#[test]
fn no_record_refuses_as_an_install() {
    // A missing record is an install's, not update's: `update` never adopts
    // the install event.
    let error = update_decision(None, &version("0.6.0"))
        .expect_err("no record at all is install's, not update's");
    assert_eq!(error, UpdateRefusal::NoRecord);
}

#[test]
fn an_unparseable_recorded_version_is_reported_rather_than_panicking() {
    let error = update_decision(Some("not-a-version"), &version("0.6.0"))
        .expect_err("an unparseable record cannot be compared");
    assert!(matches!(error, UpdateRefusal::InvalidRecordedVersion(_)));
}

// --- the tree-anchor helpers -------------------------------------------------

fn app_tree(root: &std::path::Path, marker: &str) {
    fs::create_dir_all(root).expect("an app dir");
    fs::write(root.join("marker"), marker).expect("a marker file");
}

#[test]
fn retaining_a_tree_renames_it_to_its_previous_sibling() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&app_dir, "outgoing");

    retain_tree(&app_dir).expect("a retain");

    assert!(!app_dir.exists(), "the tree was renamed, not copied");
    let previous = previous_tree_path(&app_dir);
    assert_eq!(
        fs::read_to_string(previous.join("marker")).expect("the retained marker"),
        "outgoing"
    );
}

#[test]
fn retaining_a_tree_replaces_a_leftover_previous_from_an_interrupted_earlier_update() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&app_dir, "this update's outgoing tree");
    // A stale `.previous` from an update that was interrupted before its own
    // anchor was ever consumed.
    app_tree(
        &previous_tree_path(&app_dir),
        "an earlier, already-abandoned tree",
    );

    retain_tree(&app_dir).expect("a retain");

    let previous = previous_tree_path(&app_dir);
    assert_eq!(
        fs::read_to_string(previous.join("marker")).expect("the retained marker"),
        "this update's outgoing tree",
        "the stale .previous must be replaced, not appended to"
    );
}

#[test]
fn restoring_a_tree_renames_previous_back_to_the_app_dir() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&previous_tree_path(&app_dir), "retained");

    restore_tree(&app_dir).expect("a restore");

    assert!(!previous_tree_path(&app_dir).exists());
    assert_eq!(
        fs::read_to_string(app_dir.join("marker")).expect("the restored marker"),
        "retained"
    );
}

#[test]
fn discarding_a_tree_removes_previous_without_touching_the_app_dir() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    app_tree(&app_dir, "current");
    app_tree(&previous_tree_path(&app_dir), "abandoned");

    discard_tree(&app_dir);

    assert!(!previous_tree_path(&app_dir).exists());
    assert_eq!(
        fs::read_to_string(app_dir.join("marker")).expect("the untouched marker"),
        "current"
    );
}

#[test]
fn discarding_an_absent_previous_tree_is_not_an_error() {
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    discard_tree(&app_dir); // must not panic
}

// --- the `update <id>` command --------------------------------------------

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A registry entry recording `dev.local.demo` at `0.6.0`, installed from
/// the local release archive at `location` — the state a real `install`
/// would have left behind, without paying for one.
fn seeded_entry(location: &str) -> RegistryEntry {
    RegistryEntry {
        id: "demo".to_string(),
        identifier: "dev.local.demo".to_string(),
        source: Source {
            kind: SourceKind::LocalArchive,
            location: location.to_string(),
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
        installed_at: registry::now_timestamp(),
        updated_at: registry::now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

/// The smallest tree `install::validate` accepts, with no lifecycle commands
/// declared — enough to test the refusals, which never reach a PHP process.
fn minimal_app_tree(root: &Path, version: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{version}"
            }}"#
        ),
    )
    .expect("a manifest");
    fs::write(root.join("composer.json"), "{\"require\": {}}").expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(root.join("bin/console"), "#!/usr/bin/env php\n").expect("a console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

#[test]
fn updating_an_unregistered_id_refuses() {
    let (_base, paths) = temp_paths();

    let error =
        update(&paths, "demo", None, None, true, "0.1.0").expect_err("nothing is installed");
    assert!(matches!(error, UpdateError::NotInstalled { .. }), "{error}");
}

#[test]
fn a_held_maintenance_lease_refuses_update_before_source_resolution() {
    let (base, paths) = temp_paths();
    let source = base.path().join("gone-0.1.0.tar.gz");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&source.display().to_string()))
    })
    .expect("a seeded registry");
    let _held = lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "import")
        .expect("the first maintenance command owns the gate");

    let error = update(&paths, "demo", None, None, true, "0.1.0")
        .expect_err("update must refuse before reading the competing source");

    assert!(matches!(error, UpdateError::Gate(_)), "{error}");
    assert!(error.to_string().contains("import"), "{error}");
}

#[test]
fn an_interleaved_update_cannot_replace_the_first_updates_previous_tree() {
    let (base, paths) = temp_paths();
    let source = base.path().join("gone-0.1.0.tar.gz");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&source.display().to_string()))
    })
    .expect("a seeded registry");

    // This is the precise window Audit 008 found: the first update has
    // retained its outgoing tree, and a second update used to remove it
    // before failing its own rename. Holding the first command's lease makes
    // the second one refuse before it can reach `retain_tree`.
    let app_dir = paths.app_dir("demo").expect("an app directory");
    app_tree(
        &previous_tree_path(&app_dir),
        "first update's outgoing tree",
    );
    let held = lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "update")
        .expect("the first update owns the gate");

    let error = update(&paths, "demo", None, None, true, "0.1.0")
        .expect_err("the interleaved update must refuse before source resolution");

    assert!(matches!(error, UpdateError::Gate(_)), "{error}");
    assert_eq!(
        fs::read_to_string(previous_tree_path(&app_dir).join("marker"))
            .expect("the first update's anchor survives"),
        "first update's outgoing tree"
    );
    drop(held);
    assert!(
        lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "rollback").is_ok(),
        "the refusal must not retain a competing handle"
    );
}

#[test]
fn a_missing_archive_is_refused_naming_its_path() {
    let (base, paths) = temp_paths();
    let gone = base.path().join("gone-0.1.0.tar.gz");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&gone.display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", Some(&gone), None, true, "0.1.0")
        .expect_err("the release archive is gone");
    assert!(matches!(error, UpdateError::Source(_)), "{error}");
    assert!(
        error.to_string().contains(&gone.display().to_string()),
        "{error}"
    );
}

#[test]
fn a_directory_is_refused_naming_the_publish_local_route() {
    // `update <id> <dir>` always builds a local archive from its argument, so
    // a directory is owed the same `publish --local` route as `install <dir>`.
    let (base, paths) = temp_paths();
    let project = base.path().join("project");
    fs::create_dir_all(&project).expect("a project directory");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry("/releases/demo-0.6.0.tar.gz"))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", Some(&project), None, true, "0.1.0")
        .expect_err("a directory is not an update source");

    assert!(
        matches!(
            error,
            UpdateError::Source(crate::source::SourceError::DirectoryNotASource { .. })
        ),
        "{error}"
    );
    assert!(error.to_string().contains("publish"), "{error}");
    // The app is untouched: the refusal happens before any mutation.
    let entry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry");
    assert_eq!(entry.app_version, "0.6.0");
    assert!(!journal_present(&paths));
}

#[test]
fn a_source_already_at_the_recorded_version_refuses() {
    let (_base, paths) = temp_paths();
    let release = tempfile::tempdir().expect("a release directory");
    let archive = built_archive(release.path(), "0.6.0");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&archive.display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", Some(&archive), None, true, "0.1.0")
        .expect_err("an equal source refuses");
    assert!(matches!(error, UpdateError::Equal { .. }), "{error}");
    assert!(
        lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "import").is_ok(),
        "an early update refusal releases its maintenance lease"
    );
}

#[test]
fn a_downgrade_refuses_naming_both_versions() {
    let (_base, paths) = temp_paths();
    let release = tempfile::tempdir().expect("a release directory");
    // Older than the recorded 0.6.0.
    let archive = built_archive(release.path(), "0.5.0");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&archive.display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", Some(&archive), None, true, "0.1.0")
        .expect_err("a downgrade refuses");
    assert!(matches!(error, UpdateError::Downgrade { .. }), "{error}");
}

/// The bundled interpreter and Composer, or a reason to skip — the same gate
/// `install_tests.rs` uses, duplicated rather than imported: each `_tests.rs`
/// file is self-contained, and a 170 MB download is not something a unit test
/// should trigger.
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

/// A fixture app whose `bin/console` records what it was asked to do, and
/// fails on the word `boom` — `install_tests.rs`'s own `runnable_app_tree`,
/// parameterised on `version` since this module rewrites the manifest in
/// place to simulate a newer source landing.
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
        file_put_contents(
            getenv('APP_LOG_DIR') . '/hooks.log',
            implode(' ', $arguments) . "\n",
            FILE_APPEND
        );
        exit(in_array('boom', $arguments, true) ? 1 : 0);
        "#,
    )
    .expect("a console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

#[test]
fn an_update_runs_pre_update_then_post_update_and_no_install_hooks() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let release = tempfile::tempdir().expect("a release dir");
    let (base, paths) = temp_paths();

    runnable_app_tree(
        source.path(),
        "0.6.0",
        r#"{"pre-install": ["about"], "post-install": ["doctrine:migrations:migrate"]}"#,
    );
    let first = crate::test_release::release_of(source.path(), release.path());
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    // A newer source, with update hooks instead of install ones.
    runnable_app_tree(
        source.path(),
        "0.7.0",
        r#"{"pre-update": ["cache:clear"], "post-update": ["about"]}"#,
    );
    let second = crate::test_release::release_of(source.path(), release.path());

    update(&paths, "demo", Some(&second), None, true, "0.1.0").expect("the update applies");

    let log = base.path().join("TFSApp/dev.local.demo/log/hooks.log");
    // The hub's own `cache:warmup` (plan 024) closes out each event — the
    // install's, then the update's — through this same fixture console.
    assert_eq!(
        fs::read_to_string(log).expect("a hook trace"),
        "about\ndoctrine:migrations:migrate\ncache:warmup --env=prod --no-debug\n\
         cache:clear\nabout\ncache:warmup --env=prod --no-debug\n",
        "the install's own hooks must not repeat, and the update's must run in order"
    );

    let updated = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(updated.app_version, "0.7.0");
    assert_eq!(
        crate::lifecycle::read_data_version(&data_subdir).expect("a readable data record"),
        Some("0.7.0".to_string()),
        "prepare's success point advances the data version"
    );
    let anchor = read_rollback_anchor(&data_subdir).expect("a complete rollback anchor");
    assert_eq!(anchor.app_version, "0.6.0");
    assert!(
        previous_tree_path(&paths.app_dir("demo").expect("an app dir")).is_dir(),
        "a successful update keeps the outgoing tree"
    );
    assert!(data_subdir.join("app.db.pre-update").is_file());
}

#[test]
fn a_failing_pre_update_leaves_the_tree_the_database_and_the_registry_entry_unchanged() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let release = tempfile::tempdir().expect("a release dir");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    let first = crate::test_release::release_of(source.path(), release.path());
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let before_manifest =
        fs::read_to_string(app_dir.join("tfsapp.config.json")).expect("the outgoing manifest");
    let before_db = fs::read(data_subdir.join("app.db")).expect("the outgoing database");
    let before_registry = registry::load(&paths).expect("the outgoing registry");

    runnable_app_tree(source.path(), "0.7.0", r#"{"pre-update": ["boom"]}"#);
    let second = crate::test_release::release_of(source.path(), release.path());

    let error = update(&paths, "demo", Some(&second), None, true, "0.1.0")
        .expect_err("a failing pre-update must fail the whole update");
    assert!(matches!(error, UpdateError::Reverted { .. }), "{error}");

    assert_eq!(
        fs::read_to_string(app_dir.join("tfsapp.config.json")).expect("the restored manifest"),
        before_manifest,
        "the tree must come back exactly as it was"
    );
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the restored database"),
        before_db,
        "the database must come back exactly as it was"
    );
    assert_eq!(
        registry::load(&paths).expect("the untouched registry"),
        before_registry,
        "the registry must never be touched on a reverted update"
    );
    assert_eq!(
        crate::lifecycle::read_data_version(&data_subdir).expect("a readable data record"),
        Some("0.6.0".to_string()),
        "a reverted update leaves its data version at the outgoing release"
    );
    assert!(
        !previous_tree_path(&app_dir).exists(),
        "a reverted update leaves no anchor behind"
    );
    assert!(
        !data_subdir.join("app.db.pre-update").exists(),
        "a reverted update consumes the database half of its anchor"
    );
    assert!(
        read_rollback_anchor(&data_subdir).is_none(),
        "a reverted update consumes the anchor record too"
    );
}

#[test]
fn an_update_leaves_a_cache_stamp_the_next_launch_will_match() {
    // Plan 024 step 5, invalidation path 1: an update to a new `app_version`
    // must leave a stamp naming that new version — the one `open::resolve`
    // will build as `expected` on the very next launch.
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let release = tempfile::tempdir().expect("a release dir");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    let first = crate::test_release::release_of(source.path(), release.path());
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    runnable_app_tree(source.path(), "0.7.0", "{}");
    let second = crate::test_release::release_of(source.path(), release.path());
    update(&paths, "demo", Some(&second), None, true, "0.1.0").expect("the update applies");

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join("data");
    let cache_dir = data_dir.join("cache");
    // A real launch always finds `cache/` repopulated by its own warm-up —
    // this fixture's console never actually writes one, so it is faked here
    // to isolate what this test cares about: whether the stamp's *fields*
    // would let a launch reuse it, not whether this fixture writes a cache.
    fs::create_dir_all(&cache_dir).expect("a cache dir");
    fs::write(cache_dir.join("marker"), b"warm").expect("a cache entry");

    let entry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    let expected = crate::lifecycle::CacheStamp {
        app_version: entry.app_version.clone(),
        snapshot_path: tfsapp_core::sidecar::path_to_string(&app_dir),
        platform: entry.platform.clone(),
    };

    assert_eq!(
        crate::lifecycle::read_cache_stamp(&data_subdir, &cache_dir, &expected),
        crate::lifecycle::CacheStatus::Matches,
        "an update's own warm-up must leave a stamp the very next launch reuses"
    );
}

#[test]
fn an_update_s_post_replacement_cache_wipe_never_reaches_uploads() {
    // Plan 049 step 1: `apply`'s best-effort `remove_dir_all` on `cache/`/
    // `build/`, right after the replacement snapshot lands, is the second of
    // the two places a directory is emptied at update time — `app_env.rs`'s
    // own launch-time wipe is the first, pinned in `app_env_tests.rs`. Both
    // must stay a two-directory wipe forever; `uploads/` (decision 006) is
    // never emptied by the host, at any launch, under any circumstance.
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let release = tempfile::tempdir().expect("a release dir");
    let (base, paths) = temp_paths();

    runnable_app_tree(source.path(), "0.6.0", "{}");
    let first = crate::test_release::release_of(source.path(), release.path());
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let uploads_dir = data_dir.join("uploads");
    fs::create_dir_all(&uploads_dir).expect("an uploads dir");
    let content = b"a file the app wrote and must never lose";
    fs::write(uploads_dir.join("invoice.pdf"), content).expect("a planted upload");

    runnable_app_tree(source.path(), "0.7.0", "{}");
    let second = crate::test_release::release_of(source.path(), release.path());
    update(&paths, "demo", Some(&second), None, true, "0.1.0").expect("the update applies");

    assert_eq!(
        fs::read(uploads_dir.join("invoice.pdf")).expect("the upload survives an update"),
        content,
        "update's post-replacement cache wipe must never reach uploads/"
    );
}

// --- kill-safe transaction boundaries (plan 060) ----------------------------
//
// Each boundary here is a real `update` run stopped, via a `#[cfg(test)]`
// hook in `apply` itself, at the exact instant a mutation
// has happened but the journal write recording it has not — the two audited
// critical windows (`hub/src/update.rs`'s `retain_tree`/`finalise_anchor`
// calls) plus every other such boundary the real pipeline has. `repair`'s own
// behaviour, not a hand-built fixture, is what is under test.

/// An installed `0.6.0` app with a seeded database and a built `0.7.0`
/// release archive, ready for a killed `apply`. The new tree carries a
/// `README` the outgoing one never had — the cheap way to tell which tree is
/// live without inspecting `tfsapp.config.json`.
fn seeded_for_apply_kill() -> (
    tempfile::TempDir,
    Paths,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let source = tempfile::tempdir().expect("a temp source");
    let release = tempfile::tempdir().expect("a release dir");
    let (base, paths) = temp_paths();

    runnable_app_tree(
        source.path(),
        "0.6.0",
        r#"{"pre-install": [], "post-install": []}"#,
    );
    let first = crate::test_release::release_of(source.path(), release.path());
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    runnable_app_tree(
        source.path(),
        "0.7.0",
        r#"{"pre-update": [], "post-update": []}"#,
    );
    fs::write(source.path().join("README"), "new tree").expect("a tree marker");
    let next = crate::test_release::release_of(source.path(), release.path());

    (base, paths, release, next, app_dir)
}

/// Boundaries at which the outgoing state is still the answer: the journal is
/// left at the phase just before the one named, always earlier than
/// `RegistryCommitted`.
const APPLY_REVERT_BOUNDARIES: [&str; 5] = [
    "snapshot_complete",
    "tree_retained",
    "replacement_installed",
    "lifecycle_complete",
    "registry_committed",
];

/// Boundaries at which the registry already names the new version: the three
/// stop points reached from inside `finalise_anchor` itself, plus the two
/// right after it, in `apply`.
const APPLY_FORWARD_BOUNDARIES: [&str; 5] = [
    "finalise_tree_promoted",
    "finalise_members_promoted",
    "finalise_rollback_written",
    "anchor_finalised",
    "journal_discarded",
];

#[test]
fn a_killed_apply_reverts_to_the_outgoing_version_before_registry_commit() {
    if !resources_present() {
        return;
    }
    for point in APPLY_REVERT_BOUNDARIES {
        let (base, paths, _release, next, app_dir) = seeded_for_apply_kill();
        let data_subdir = base.path().join("TFSApp/dev.local.demo/data");

        test_stop::arm(point);
        let result = update(&paths, "demo", Some(&next), None, true, "0.1.0");
        test_stop::disarm();
        assert!(result.is_err(), "{point}: the stop point should have fired");

        assert!(journal_present(&paths), "{point}");
        repair_at(&paths, "demo", true).unwrap_or_else(|error| panic!("{point}: {error}"));

        assert!(!journal_present(&paths), "{point}");
        assert!(
            !app_dir.join("README").exists(),
            "{point}: the outgoing tree must be live again"
        );
        assert_eq!(
            fs::read_to_string(data_subdir.join("app.db")).unwrap(),
            "pre-update-bytes",
            "{point}: the outgoing database must be restored"
        );
        assert!(
            !previous_tree_path(&app_dir).exists(),
            "{point}: a revert creates no anchor"
        );
        let entry = registry::load(&paths)
            .unwrap()
            .get("demo")
            .cloned()
            .unwrap();
        assert_eq!(entry.app_version, "0.6.0", "{point}");

        assert!(
            matches!(
                repair_at(&paths, "demo", true),
                Err(UpdateError::NoJournal { .. })
            ),
            "{point}: repairing twice must say there is nothing to repair"
        );
    }
}

#[test]
fn a_killed_apply_finishes_forward_from_registry_commit_on() {
    if !resources_present() {
        return;
    }
    for point in APPLY_FORWARD_BOUNDARIES {
        let (base, paths, _release, next, app_dir) = seeded_for_apply_kill();
        let data_subdir = base.path().join("TFSApp/dev.local.demo/data");

        test_stop::arm(point);
        let result = update(&paths, "demo", Some(&next), None, true, "0.1.0");
        test_stop::disarm();
        assert!(result.is_err(), "{point}: the stop point should have fired");

        assert!(journal_present(&paths), "{point}");
        repair_at(&paths, "demo", true).unwrap_or_else(|error| panic!("{point}: {error}"));

        assert!(!journal_present(&paths), "{point}");
        assert!(
            app_dir.join("README").exists(),
            "{point}: the update already happened — the new tree stays live"
        );
        let entry = registry::load(&paths)
            .unwrap()
            .get("demo")
            .cloned()
            .unwrap();
        assert_eq!(entry.app_version, "0.7.0", "{point}");

        assert!(
            previous_tree_path(&app_dir).is_dir(),
            "{point}: the outgoing tree is promoted to the anchor"
        );
        assert!(
            !previous_tree_path(&app_dir).join("README").exists(),
            "{point}: the anchor holds the outgoing tree, not the new one"
        );
        assert_eq!(
            fs::read_to_string(data_subdir.join("app.db.pre-update")).unwrap(),
            "pre-update-bytes",
            "{point}: the anchor's database half is the outgoing one"
        );
        let anchor = read_rollback_anchor(&data_subdir).expect("a written rollback anchor");
        assert_eq!(anchor.app_version, "0.6.0", "{point}");

        assert!(
            matches!(
                repair_at(&paths, "demo", true),
                Err(UpdateError::NoJournal { .. })
            ),
            "{point}: repairing twice must say there is nothing to repair"
        );
    }
}

// --- update on a remote source ----------------------------------------------
//
// `install_tests.rs`'s own `fixture_release_archive`/`stub_release`,
// duplicated rather than imported — each `_tests.rs` file is self-contained.

fn fixture_release_archive(top_level: &str, app_version: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));

    let mut dir_header = tar::Header::new_gnu();
    dir_header.set_entry_type(tar::EntryType::Directory);
    dir_header.set_size(0);
    dir_header.set_mode(0o755);
    dir_header
        .set_path(format!("{top_level}/"))
        .expect("a dir path");
    dir_header.set_cksum();
    builder
        .append(&dir_header, std::io::empty())
        .expect("append the top-level dir");

    let manifest = format!(
        r#"{{
            "product_name": "Demo App",
            "identifier": "dev.local.demo",
            "project_name": "demo",
            "app_version": "{app_version}"
        }}"#
    );
    append_archive_file(
        &mut builder,
        &format!("{top_level}/tfsapp.config.json"),
        manifest.as_bytes(),
    );
    append_archive_file(&mut builder, &format!("{top_level}/composer.json"), b"{}");
    append_archive_file(
        &mut builder,
        &format!("{top_level}/bin/console"),
        b"#!/usr/bin/env php\n",
    );
    append_archive_file(
        &mut builder,
        &format!("{top_level}/public/index.php"),
        b"<?php",
    );

    builder
        .into_inner()
        .expect("finish the tar layer")
        .finish()
        .expect("finish the gzip layer")
}

fn append_archive_file(
    builder: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    path: &str,
    content: &[u8],
) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_path(path).expect("a valid file path");
    header.set_cksum();
    builder.append(&header, content).expect("append a file");
}

/// Answers the three requests resolving one release makes, in whatever order
/// they arrive, then stops.
fn stub_release(
    repo: &str,
    tag: &str,
    archive_name: &str,
    archive_bytes: Vec<u8>,
) -> (String, std::thread::JoinHandle<()>) {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(&archive_bytes);
    let checksums_body = format!("{:x}  {archive_name}\n", hasher.finalize());

    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let base_url = format!("http://127.0.0.1:{port}");
    let release_path = format!("/repos/{repo}/releases/latest");
    let archive_path = format!("/assets/{archive_name}");
    let release_body = format!(
        r#"{{"tag_name": "{tag}", "html_url": "{base_url}/releases/tag/{tag}", "assets": [
            {{"name": "{archive_name}", "browser_download_url": "{base_url}{archive_path}", "size": {archive_size}}},
            {{"name": "SHA256SUMS.txt", "browser_download_url": "{base_url}/assets/SHA256SUMS.txt", "size": {sums_size}}}
        ]}}"#,
        archive_size = archive_bytes.len(),
        sums_size = checksums_body.len(),
    );

    let handle = std::thread::spawn(move || {
        for _ in 0..3 {
            let request = server.recv().expect("a request");
            let url = request.url().to_string();
            if url == release_path {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .expect("a valid header");
                request
                    .respond(
                        tiny_http::Response::from_string(release_body.clone()).with_header(header),
                    )
                    .expect("respond with the release");
            } else if url == archive_path {
                request
                    .respond(tiny_http::Response::from_data(archive_bytes.clone()))
                    .expect("respond with the archive");
            } else if url == "/assets/SHA256SUMS.txt" {
                request
                    .respond(tiny_http::Response::from_string(checksums_body.clone()))
                    .expect("respond with the checksums");
            } else {
                request
                    .respond(tiny_http::Response::from_string("not found").with_status_code(404))
                    .expect("respond 404");
            }
        }
    });

    (base_url, handle)
}

#[test]
fn an_update_with_no_ref_moves_to_the_newest_remote_release() {
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let first_archive = fixture_release_archive("demo-0.6.0", "0.6.0");
    let (first_url, first_handle) =
        stub_release("example/demo", "v0.6.0", "demo-0.6.0.tar.gz", first_archive);
    crate::install::install_into(
        &paths,
        scratch.path(),
        &first_url,
        "github:example/demo",
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the first install");
    first_handle.join().expect("the first stub finishes");

    let second_archive = fixture_release_archive("demo-0.7.0", "0.7.0");
    let (second_url, second_handle) = stub_release(
        "example/demo",
        "v0.7.0",
        "demo-0.7.0.tar.gz",
        second_archive,
    );
    super::update_into(
        &paths,
        scratch.path(),
        &second_url,
        "demo",
        None,
        None,
        true,
        "0.1.0",
    )
    .expect("update moves to the newest release, no --ref needed");
    second_handle.join().expect("the second stub finishes");

    let entry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(entry.app_version, "0.7.0");
    assert_eq!(entry.source.reference.as_deref(), Some("v0.7.0"));
}

#[test]
fn an_equal_remote_source_refuses() {
    // 017's decision table is source-kind-agnostic (`update_decision` never
    // reads `SourceKind`) — this is the one place that fires it end to end
    // over a *remote* source, to prove the table really did carry over
    // unchanged rather than merely being untested for this kind.
    if !resources_present() {
        return;
    }
    let (_base, paths) = temp_paths();
    let scratch = tempfile::tempdir().expect("a scratch dir");

    let archive = fixture_release_archive("demo-0.6.0", "0.6.0");
    let (install_url, install_handle) =
        stub_release("example/demo", "v0.6.0", "demo-0.6.0.tar.gz", archive);
    crate::install::install_into(
        &paths,
        scratch.path(),
        &install_url,
        "github:example/demo",
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the install");
    install_handle.join().expect("the install stub finishes");

    let same_archive = fixture_release_archive("demo-0.6.0", "0.6.0");
    let (update_url, update_handle) =
        stub_release("example/demo", "v0.6.0", "demo-0.6.0.tar.gz", same_archive);
    let error = super::update_into(
        &paths,
        scratch.path(),
        &update_url,
        "demo",
        None,
        None,
        true,
        "0.1.0",
    )
    .expect_err("the same release again refuses");
    update_handle.join().expect("the update stub finishes");

    assert!(matches!(error, UpdateError::Equal { .. }), "{error}");
}

fn built_archive(root: &Path, version: &str) -> std::path::PathBuf {
    let project = tempfile::tempdir().unwrap();
    minimal_app_tree(project.path(), version);
    crate::test_release::release_of(project.path(), root)
}

#[test]
fn archive_sourced_update_requires_next_archive_and_moves_forward() {
    if !resources_present() {
        return;
    }
    let release = tempfile::tempdir().unwrap();
    let first = built_archive(release.path(), "0.6.0");
    let (base, paths) = temp_paths();
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .unwrap();
    let before = registry::load(&paths).unwrap().get("demo").unwrap().clone();
    let error = update(&paths, "demo", None, None, true, "0.1.0").unwrap_err();
    assert!(
        matches!(error, UpdateError::ArchiveRequired { .. }),
        "{error}"
    );
    assert!(error
        .to_string()
        .contains("tfsapp-hub update demo <path.tar.gz>"));
    assert_eq!(registry::load(&paths).unwrap().get("demo"), Some(&before));
    assert!(!journal_present(&paths));

    let second = built_archive(release.path(), "0.7.0");
    assert!(update(&paths, "demo", Some(&second), None, true, "0.1.0").unwrap());
    let after = registry::load(&paths).unwrap().get("demo").unwrap().clone();
    assert_eq!(after.app_version, "0.7.0");
    assert_eq!(after.source.kind, SourceKind::LocalArchive);
    assert_eq!(
        after.source.location,
        second.canonicalize().unwrap().display().to_string()
    );
    assert!(!journal_present(&paths));
    assert!(base.path().exists());
}

#[test]
fn archive_update_refuses_bad_checksum_ref_and_downgrade_before_mutation() {
    let release = tempfile::tempdir().unwrap();
    let newer = built_archive(release.path(), "0.7.0");
    let (_base, paths) = temp_paths();
    let entry = seeded_entry("/releases/demo-0.6.0.tar.gz");
    registry::update(&paths, |registry| registry.upsert(entry.clone())).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let reference = super::update_into(
        &paths,
        scratch.path(),
        "http://unused.invalid",
        "demo",
        Some(&newer),
        Some("v0.7.0"),
        true,
        "0.1.0",
    )
    .unwrap_err();
    assert!(matches!(
        reference,
        UpdateError::Source(crate::source::SourceError::ReferenceOnLocalArchive { .. })
    ));
    let mut bytes = fs::read(&newer).unwrap();
    bytes[20] ^= 1;
    fs::write(&newer, bytes).unwrap();
    let error = super::update_into(
        &paths,
        scratch.path(),
        "http://unused.invalid",
        "demo",
        Some(&newer),
        None,
        true,
        "0.1.0",
    )
    .unwrap_err();
    assert!(matches!(
        error,
        UpdateError::Source(crate::source::SourceError::ChecksumMismatch { .. })
    ));
    assert_eq!(registry::load(&paths).unwrap().get("demo"), Some(&entry));
    assert!(!journal_present(&paths));
}

#[test]
fn forge_record_updated_from_archive_switches_its_source() {
    if !resources_present() {
        return;
    }
    let release = tempfile::tempdir().unwrap();
    let first = built_archive(release.path(), "0.6.0");
    let (_base, paths) = temp_paths();
    crate::install::install(
        &paths,
        &first.display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .unwrap();
    registry::update(&paths, |registry| {
        let entry = registry
            .apps
            .iter_mut()
            .find(|entry| entry.id == "demo")
            .unwrap();
        entry.source.kind = SourceKind::Release;
        entry.source.location = "example/demo".to_string();
        entry.source.reference = Some("v0.6.0".to_string());
        entry.source.reference_kind = Some(crate::registry::ReferenceKind::Tag);
        entry.source.index = Some("github".to_string());
    })
    .unwrap();
    let folder = tempfile::tempdir().unwrap();
    let archive = built_archive(folder.path(), "0.7.0");
    assert!(update(&paths, "demo", Some(&archive), None, true, "0.1.0").unwrap());
    let entry = registry::load(&paths).unwrap().get("demo").unwrap().clone();
    assert_eq!(entry.source.kind, SourceKind::LocalArchive);
    assert_eq!(entry.source.reference, None);
    assert_eq!(entry.source.index, None);
}

#[test]
fn an_older_archive_is_still_a_downgrade() {
    let folder = tempfile::tempdir().unwrap();
    let archive = built_archive(folder.path(), "0.6.0");
    let (_base, paths) = temp_paths();
    let mut entry = seeded_entry("/missing/demo-0.6.0.tar.gz");
    entry.app_version = "0.7.0".to_string();
    registry::update(&paths, |registry| registry.upsert(entry.clone())).unwrap();
    let error = update(&paths, "demo", Some(&archive), None, true, "0.1.0").unwrap_err();
    assert!(matches!(error, UpdateError::Downgrade { .. }), "{error}");
    assert_eq!(registry::load(&paths).unwrap().get("demo"), Some(&entry));
    assert!(!journal_present(&paths));
}
