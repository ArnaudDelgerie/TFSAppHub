use std::fs;

use super::{
    lifecycle_decision, prepare_dev_launch, read_data_version, write_data_version,
    LifecycleDecisionError, LifecycleError, LifecycleEvent,
};

fn version(text: &str) -> semver::Version {
    semver::Version::parse(text).expect("a semver version")
}

// --- lifecycle_decision --------------------------------------------------
//
// The station's own table, ported unchanged: the rule is CONTRACT.md §6 and it
// has to read identically on both hosts, or the two would disagree about a data
// dir they share.

#[test]
fn no_record_is_an_install() {
    assert_eq!(
        lifecycle_decision(None, &version("1.0.0")).expect("a decision"),
        LifecycleEvent::Install
    );
}

#[test]
fn an_older_record_is_an_update() {
    assert_eq!(
        lifecycle_decision(Some("1.0.0"), &version("1.1.0")).expect("a decision"),
        LifecycleEvent::Update
    );
}

#[test]
fn a_matching_record_is_an_ordinary_launch() {
    assert_eq!(
        lifecycle_decision(Some("1.0.0"), &version("1.0.0")).expect("a decision"),
        LifecycleEvent::None
    );
}

#[test]
fn a_newer_record_is_a_downgrade_and_has_no_event() {
    let error = lifecycle_decision(Some("2.0.0"), &version("1.0.0")).expect_err("a downgrade");

    match error {
        LifecycleDecisionError::Downgrade { recorded, current } => {
            // Both values, because the refusal has to name them: the user is the
            // only one who can decide which of the two they meant to keep.
            assert_eq!(recorded, version("2.0.0"));
            assert_eq!(current, version("1.0.0"));
        }
        other => panic!("expected a downgrade, got {other:?}"),
    }
}

#[test]
fn an_unparseable_record_is_an_error_not_a_fresh_install() {
    // The distinction that protects data: read as "no record", this would rerun
    // an install event over a data dir that already holds a database.
    let error = lifecycle_decision(Some("not-a-version"), &version("1.0.0"))
        .expect_err("an unreadable record");

    assert!(matches!(error, LifecycleDecisionError::InvalidVersion(_)));
}

// --- data/config.json ----------------------------------------------------

#[test]
fn a_data_dir_with_no_record_reads_as_none() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");

    // The state of every app on its first launch, which must not be an error.
    assert_eq!(
        read_data_version(data_subdir.path()).expect("a readable data dir"),
        None
    );
}

#[test]
fn a_written_version_reads_back() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");

    write_data_version(data_subdir.path(), "0.6.0").expect("a written record");

    assert_eq!(
        read_data_version(data_subdir.path()).expect("a readable record"),
        Some("0.6.0".to_string())
    );
}

#[test]
fn writing_a_version_keeps_the_user_s_port_override() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{"version": "0.5.0", "port_override": 9876}"#,
    )
    .expect("a hand-written record");

    write_data_version(data_subdir.path(), "0.6.0").expect("a written record");

    // `port_override` is the user's own escape hatch for a pinned port already
    // taken on their machine (CONTRACT.md §6). Nothing that merely records a
    // version has any business forgetting it.
    let written = fs::read_to_string(data_subdir.path().join("config.json")).expect("the record");
    assert!(written.contains("9876"), "kept the override: {written}");
    assert!(written.contains("0.6.0"), "recorded the version: {written}");
}

#[test]
fn writing_a_version_leaves_no_temp_file_behind() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");

    write_data_version(data_subdir.path(), "0.6.0").expect("a written record");

    // The write is temp-file-plus-rename so a crash can never leave a truncated
    // record; the rename is also what must leave nothing beside it.
    assert!(!data_subdir.path().join("config.json.tmp").exists());
}

// --- prepare_dev_launch ----------------------------------------------------

#[test]
fn a_dev_launch_takes_the_lock_and_writes_no_version_record() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let data_subdir = data_dir.path().join("data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");

    let lock = prepare_dev_launch(data_dir.path(), &data_subdir, "dev.local.demo", None);

    assert!(lock.is_some(), "a fresh dev session takes the lock");
    // The load-bearing difference from `prepare_launch`: no version guard
    // runs, so nothing is ever stamped here — an author editing
    // `app_version` in their own tree must never be refused a launch or
    // have a stray record written under it.
    assert!(!data_subdir.join("config.json").exists());
}

#[test]
fn a_malformed_record_is_refused_rather_than_read_as_absent() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(data_subdir.path().join("config.json"), "{ not json").expect("a broken record");

    let error = read_data_version(data_subdir.path()).expect_err("an unreadable record");

    assert!(matches!(error, LifecycleError::MalformedDataConfig { .. }));
    assert!(
        error.to_string().contains("config.json"),
        "names the file: {error}"
    );
}
