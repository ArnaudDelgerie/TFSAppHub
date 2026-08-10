use std::fs;

use super::{
    discard_tree, restore_tree, retain_tree, update_decision, UpdateAction, UpdateRefusal,
};
use crate::lifecycle::previous_tree_path;

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
