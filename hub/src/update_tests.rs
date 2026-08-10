use std::{fs, path::Path};

use super::{
    discard_tree, restore_tree, retain_tree, update, update_decision, UpdateAction, UpdateError,
    UpdateRefusal,
};
use crate::{
    lifecycle::previous_tree_path,
    paths::Paths,
    registry::{self, Platform, RegistryEntry, Source, SourceKind, State},
};

fn version(text: &str) -> semver::Version {
    semver::Version::parse(text).expect("a semver version")
}

// --- update_decision -------------------------------------------------------

#[test]
fn a_newer_source_applies_the_update_event() {
    assert_eq!(
        update_decision(Some("0.5.0"), &version("0.6.0"), false),
        Ok(UpdateAction::Apply)
    );
}

#[test]
fn a_newer_source_applies_regardless_of_force() {
    // `--force` is for the equal case only — a real update needs no unlocking.
    assert_eq!(
        update_decision(Some("0.5.0"), &version("0.6.0"), true),
        Ok(UpdateAction::Apply)
    );
}

#[test]
fn an_equal_source_refuses_without_force() {
    let error = update_decision(Some("0.6.0"), &version("0.6.0"), false)
        .expect_err("an equal record refuses without --force");
    assert_eq!(
        error,
        UpdateRefusal::Equal {
            version: version("0.6.0")
        }
    );
}

#[test]
fn an_equal_source_with_force_resyncs_only() {
    assert_eq!(
        update_decision(Some("0.6.0"), &version("0.6.0"), true),
        Ok(UpdateAction::ResyncOnly)
    );
}

#[test]
fn a_downgrade_refuses_whether_or_not_force_is_given() {
    // `--force` is for the case `source_revision` exists to catch — a tree
    // edited without bumping the version — never for reverting to an older
    // release.
    for force in [false, true] {
        let error = update_decision(Some("0.6.0"), &version("0.5.0"), force)
            .expect_err("a downgrade always refuses");
        assert_eq!(
            error,
            UpdateRefusal::Downgrade {
                recorded: version("0.6.0"),
                source: version("0.5.0"),
            }
        );
    }
}

#[test]
fn no_record_refuses_as_an_install_whether_or_not_force_is_given() {
    // A missing record with `--force` is still an install: `update` never
    // adopts the install event, whatever flag is passed.
    for force in [false, true] {
        let error = update_decision(None, &version("0.6.0"), force)
            .expect_err("no record at all is install's, not update's");
        assert_eq!(error, UpdateRefusal::NoRecord);
    }
}

#[test]
fn an_unparseable_recorded_version_is_reported_rather_than_panicking() {
    let error = update_decision(Some("not-a-version"), &version("0.6.0"), false)
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

/// A registry entry recording `dev.local.demo` at `0.6.0`, sourced from
/// `location` — the state a real `install` would have left behind, without
/// paying for one.
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
        update(&paths, "demo", None, false, true, "0.1.0").expect_err("nothing is installed");
    assert!(matches!(error, UpdateError::NotInstalled { .. }), "{error}");
}

#[test]
fn a_source_that_no_longer_exists_is_refused_naming_the_recorded_location() {
    let (base, paths) = temp_paths();
    let gone = base.path().join("gone");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&gone.display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", None, false, true, "0.1.0")
        .expect_err("the recorded source directory is gone");
    assert!(matches!(error, UpdateError::Source(_)), "{error}");
    assert!(
        error.to_string().contains(&gone.display().to_string()),
        "{error}"
    );
}

#[test]
fn a_source_already_at_the_recorded_version_refuses_without_force() {
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    minimal_app_tree(source.path(), "0.6.0");
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&source.path().display().to_string()))
    })
    .expect("a seeded registry");

    let error = update(&paths, "demo", None, false, true, "0.1.0")
        .expect_err("an equal source refuses without --force");
    assert!(matches!(error, UpdateError::Equal { .. }), "{error}");
}

#[test]
fn a_downgrade_refuses_naming_both_versions() {
    let (_base, paths) = temp_paths();
    let source = tempfile::tempdir().expect("a temp source");
    minimal_app_tree(source.path(), "0.5.0"); // older than the recorded 0.6.0
    registry::update(&paths, |registry| {
        registry.upsert(seeded_entry(&source.path().display().to_string()))
    })
    .expect("a seeded registry");

    let error =
        update(&paths, "demo", None, false, true, "0.1.0").expect_err("a downgrade refuses");
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
    let (base, paths) = temp_paths();

    runnable_app_tree(
        source.path(),
        "0.6.0",
        r#"{"pre-install": ["about"], "post-install": ["doctrine:migrations:migrate"]}"#,
    );
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

    // A newer source, with update hooks instead of install ones.
    runnable_app_tree(
        source.path(),
        "0.7.0",
        r#"{"pre-update": ["cache:clear"], "post-update": ["about"]}"#,
    );

    update(&paths, "demo", None, false, true, "0.1.0").expect("the update applies");

    let log = base.path().join("TFSApp/dev.local.demo/log/hooks.log");
    assert_eq!(
        fs::read_to_string(log).expect("a hook trace"),
        "about\ndoctrine:migrations:migrate\ncache:clear\nabout\n",
        "the install's own hooks must not repeat, and the update's must run in order"
    );

    let updated = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry survives");
    assert_eq!(updated.app_version, "0.7.0");
}

#[test]
fn a_failing_pre_update_leaves_the_tree_the_database_and_the_registry_entry_unchanged() {
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
    fs::write(data_subdir.join("app.db"), b"pre-update-bytes").expect("a seeded database");

    let app_dir = paths.app_dir("demo").expect("an app dir");
    let before_manifest =
        fs::read_to_string(app_dir.join("tfsapp.config.json")).expect("the outgoing manifest");
    let before_db = fs::read(data_subdir.join("app.db")).expect("the outgoing database");
    let before_registry = registry::load(&paths).expect("the outgoing registry");

    runnable_app_tree(source.path(), "0.7.0", r#"{"pre-update": ["boom"]}"#);

    let error = update(&paths, "demo", None, false, true, "0.1.0")
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
    assert!(
        !previous_tree_path(&app_dir).exists(),
        "a reverted update leaves no anchor behind"
    );
}

#[test]
fn force_on_an_equal_source_resyncs_without_running_any_hook() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    runnable_app_tree(
        source.path(),
        "0.6.0",
        r#"{"pre-update": ["cache:clear"], "post-update": ["about"]}"#,
    );
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

    let before_registry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry");

    // A developer edits the source without bumping the version — a different
    // `source_revision`, same `app_version`.
    fs::write(source.path().join("README"), "edited").expect("an edit");

    update(&paths, "demo", None, true, true, "0.1.0").expect("--force resyncs");

    let log = base.path().join("TFSApp/dev.local.demo/log/hooks.log");
    assert!(
        !log.exists(),
        "a resync must run no lifecycle command at all"
    );
    assert!(
        !previous_tree_path(&paths.app_dir("demo").expect("an app dir")).exists(),
        "a resync must not rotate the anchor"
    );

    let after_registry = registry::load(&paths)
        .expect("a readable registry")
        .get("demo")
        .cloned()
        .expect("the entry");
    assert_eq!(after_registry.app_version, before_registry.app_version);
    assert_ne!(
        after_registry.source_revision, before_registry.source_revision,
        "a resync must still catch the source's own change"
    );
}
