use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use super::{
    acquire_launch_locks, anchor_state, cache_stamp_path, claim_teardown, close_action,
    close_effect, close_stops_backend, close_warning, copy_rescue_dump_at, data_dir_holder,
    db_snapshot_path, decide_launch, dialog_is_warranted, discard_db_snapshot,
    discard_rollback_anchor, handle_close_answer, handle_close_request, lifecycle_decision,
    on_window_event, prepare_dev_launch, previous_tree_path, probe_run_lock, read_cache_stamp,
    read_data_version, read_microphone_revoked, read_rollback_anchor, rescue_dump_pattern,
    restore_db_snapshot, rollback_anchor_path, serving_lock_path, snapshot_db, veto_exit,
    write_cache_stamp, write_data_version, write_rollback_anchor, Anchor, CacheStamp, CacheStatus,
    CloseAction, CloseEffect, CloseWorld, LaunchDecision, LaunchLockError, LifecycleDecisionError,
    LifecycleError, LifecycleEvent, RollbackAnchor, RunLockHeld, DB_FILE_NAMES,
};
use crate::{registry::Platform, run::format_run_entry};

fn version(text: &str) -> semver::Version {
    semver::Version::parse(text).expect("a semver version")
}

/// Spawn a controlled child that holds an exclusive flock on `path` until
/// killed — the same shell lock-file idiom `core`'s own process tests use
/// (see that crate's `process_tests.rs` for why: a single process holds the
/// lock via its own fd for its whole life, so killing it releases the lock at
/// once with nothing orphaned behind it). Blocks until the lock is observed
/// held so the caller never races it.
fn spawn_lock_holder(path: &Path) -> std::process::Child {
    let child = Command::new("sh")
        .args([
            "-c",
            r#"exec 9>"$1"; flock -n 9 || exit 1; exec sleep 30"#,
            "sh",
        ])
        .arg(path)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if tfsapp_core::process::try_lock_file(path).unwrap().is_none() {
            return child;
        }
        thread::sleep(Duration::from_millis(5));
    }
    child
}

fn kill_and_wait(mut child: std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// --- decide_launch -----------------------------------------------------------
//
// Pure and total over its two inputs (see the plan's Overview): every one of
// the four combinations is covered rather than just the three named outcomes,
// so `serving_held` alone deciding `HandOff` regardless of `liveness_held` is
// itself part of what is asserted.

#[test]
fn decide_launch_hands_off_when_serving_is_held_and_liveness_is_free() {
    assert_eq!(decide_launch(true, false), LaunchDecision::HandOff);
}

#[test]
fn decide_launch_hands_off_when_both_locks_are_held() {
    assert_eq!(decide_launch(true, true), LaunchDecision::HandOff);
}

#[test]
fn decide_launch_waits_when_serving_is_free_and_liveness_is_held() {
    assert_eq!(decide_launch(false, true), LaunchDecision::Wait);
}

#[test]
fn decide_launch_launches_when_both_locks_are_free() {
    assert_eq!(decide_launch(false, false), LaunchDecision::Launch);
}

// --- veto_exit ----------------------------------------------------------------

#[test]
fn veto_exit_vetoes_the_event_loop_running_out_of_windows_mid_teardown() {
    // `destroy_windows` empties the event loop's own window store, which is
    // what raises this. Letting it through would end the process before
    // FrankenPHP has been signalled at all.
    assert!(veto_exit(None, true));
}

#[test]
fn veto_exit_lets_our_own_app_exit_through_mid_teardown() {
    // The last line of the teardown. Vetoing this one would leave a process
    // with no window and nothing left to do, for ever.
    assert!(!veto_exit(Some(0), true));
}

#[test]
fn veto_exit_leaves_a_windowless_exit_alone_outside_a_teardown() {
    // A splash closed before the sidecar ever existed: `on_window_event` does
    // not veto that close, and the process is right to end there.
    assert!(!veto_exit(None, false));
}

// --- acquire_launch_locks / the serving lock ----------------------------------

#[test]
fn a_sibling_holding_the_serving_lock_is_a_hand_off() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let pid_file = data_dir.path().join("sidecar.pid");
    let holder = spawn_lock_holder(&serving_lock_path(data_dir.path()));

    let start = Instant::now();
    let locks = acquire_launch_locks(
        &pid_file,
        data_dir.path(),
        "dev.local.demo",
        Duration::from_secs(5),
    )
    .expect("hand-off is not an error");

    assert!(
        locks.is_none(),
        "a live sibling holding the serving lock is a hand-off"
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "a hand-off must pay nothing — it must not wait at all"
    );

    kill_and_wait(holder);
}

#[test]
fn a_sibling_holding_only_the_liveness_lock_is_waited_out_then_succeeds() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let pid_file = data_dir.path().join("sidecar.pid");
    let mut holder = spawn_lock_holder(&tfsapp_core::process::lock_path(&pid_file));

    let waiting = {
        let pid_file = pid_file.clone();
        let data_dir = data_dir.path().to_path_buf();
        thread::spawn(move || {
            acquire_launch_locks(
                &pid_file,
                &data_dir,
                "dev.local.demo",
                Duration::from_secs(5),
            )
        })
    };
    thread::sleep(Duration::from_millis(150));
    assert!(
        !waiting.is_finished(),
        "it must still be waiting on the dying sibling"
    );
    holder.kill().unwrap();
    holder.wait().unwrap();

    let locks = waiting
        .join()
        .expect("the waiter did not panic")
        .expect("a lock released mid-wait is not an error")
        .expect("the launch proceeds once the sibling is gone");

    // Genuinely held, not a dropped-and-forgotten probe: a third acquisition
    // of either lock must be excluded for as long as `locks` is alive.
    assert!(
        tfsapp_core::process::try_lock_file(&serving_lock_path(data_dir.path()))
            .unwrap()
            .is_none()
    );
    assert!(
        tfsapp_core::process::try_lock_file(&tfsapp_core::process::lock_path(&pid_file))
            .unwrap()
            .is_none()
    );
    drop(locks);
}

#[test]
fn a_sibling_that_outlives_the_wait_budget_is_a_timeout() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let pid_file = data_dir.path().join("sidecar.pid");
    let holder = spawn_lock_holder(&tfsapp_core::process::lock_path(&pid_file));

    let budget = Duration::from_millis(200);
    let error = acquire_launch_locks(&pid_file, data_dir.path(), "dev.local.demo", budget)
        .expect_err("a sibling that never lets go must not be waited out forever");

    assert!(matches!(error, LaunchLockError::Timeout(b) if b == budget));

    kill_and_wait(holder);
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
fn writing_a_version_keeps_the_user_s_port_override_and_revocation_together() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{
            "version": "0.5.0",
            "port_override": 9876,
            "revoked": {"media": {"microphone": true}}
        }"#,
    )
    .expect("a hand-written record");

    write_data_version(data_subdir.path(), "0.6.0").expect("a written record");

    // Both hand-edited keys are carried over: one escape hatch, one
    // revocation, and a version record has no business forgetting either.
    let written = fs::read_to_string(data_subdir.path().join("config.json")).expect("the record");
    assert!(written.contains("9876"), "kept the override: {written}");
    assert!(
        written.contains("revoked"),
        "kept the revocation: {written}"
    );
    assert!(
        written.contains("microphone"),
        "kept the revoked member: {written}"
    );
    assert!(written.contains("0.6.0"), "recorded the version: {written}");
}

// --- `read_microphone_revoked` (plan 070 step 1) ----------------------------
//
// The five cases the plan names. A broken switch fails closed, and says so.

#[test]
fn a_revocation_read_from_an_absent_file_is_false() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    assert!(!read_microphone_revoked(data_subdir.path()));
}

#[test]
fn a_revocation_read_from_an_absent_key_is_false() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{"version": "0.5.0"}"#,
    )
    .expect("a hand-written record");
    assert!(!read_microphone_revoked(data_subdir.path()));
}

#[test]
fn a_written_true_revocation_is_true() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{"version": "0.5.0", "revoked": {"media": {"microphone": true}}}"#,
    )
    .expect("a hand-written record");
    assert!(read_microphone_revoked(data_subdir.path()));
}

#[test]
fn a_written_false_revocation_is_false() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{"version": "0.5.0", "revoked": {"media": {"microphone": false}}}"#,
    )
    .expect("a hand-written record");
    assert!(!read_microphone_revoked(data_subdir.path()));
}

#[test]
fn a_file_that_does_not_parse_reads_as_revoked() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    fs::write(data_subdir.path().join("config.json"), "{ not json").expect("a hand-written record");
    // Fails closed: a broken switch must not fall back to "not revoked".
    assert!(read_microphone_revoked(data_subdir.path()));
}

#[test]
fn a_file_that_cannot_be_read_reads_as_revoked() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");
    // A directory in the file's place: it exists, so it is not the "absent"
    // case, and reading it fails — the same fail-closed answer as a file
    // that does not parse.
    fs::create_dir(data_subdir.path().join("config.json")).expect("a directory in its place");
    assert!(read_microphone_revoked(data_subdir.path()));
}

#[test]
fn writing_a_version_leaves_no_temp_file_behind() {
    let data_subdir = tempfile::tempdir().expect("a temp data dir");

    write_data_version(data_subdir.path(), "0.6.0").expect("a written record");

    // The write is temp-file-plus-rename so a crash can never leave a truncated
    // record; the rename is also what must leave nothing beside it.
    assert!(!data_subdir.path().join("config.json.tmp").exists());
}

// --- the cache stamp (plan 024) ---------------------------------------------

fn platform(php_version: &str) -> Platform {
    Platform {
        php_version: php_version.to_string(),
        extensions_hash: "deadbeef".to_string(),
    }
}

fn a_stamp() -> CacheStamp {
    CacheStamp {
        app_version: "0.6.0".to_string(),
        snapshot_path: "/home/arnaud/.local/share/TFSApp/hub/apps/tfsapp-test".to_string(),
        platform: platform("8.5"),
    }
}

/// A cache dir with at least one file in it — what a real `cache:warmup`
/// leaves behind, and what [`read_cache_stamp`] requires beside a matching
/// stamp before it will call the container reusable.
fn populate_cache_dir(cache_dir: &Path) {
    fs::create_dir_all(cache_dir).expect("a cache dir");
    fs::write(cache_dir.join("container.php"), b"<?php").expect("a cache file");
}

#[test]
fn no_stamp_at_all_is_absent() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    populate_cache_dir(&cache_dir);

    assert_eq!(
        read_cache_stamp(data_subdir.path(), &cache_dir, &a_stamp()),
        CacheStatus::Absent
    );
}

#[test]
fn a_matching_stamp_with_a_populated_cache_dir_matches() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    populate_cache_dir(&cache_dir);
    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    assert_eq!(
        read_cache_stamp(data_subdir.path(), &cache_dir, &a_stamp()),
        CacheStatus::Matches
    );
}

#[test]
fn a_different_app_version_is_a_mismatch() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    populate_cache_dir(&cache_dir);
    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    let mut expected = a_stamp();
    expected.app_version = "0.7.0".to_string();

    match read_cache_stamp(data_subdir.path(), &cache_dir, &expected) {
        CacheStatus::Mismatch { reason } => {
            assert!(reason.contains("app_version"), "names the field: {reason}")
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn a_moved_snapshot_path_is_a_mismatch() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    populate_cache_dir(&cache_dir);
    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    let mut expected = a_stamp();
    expected.snapshot_path = "/somewhere/else".to_string();

    match read_cache_stamp(data_subdir.path(), &cache_dir, &expected) {
        CacheStatus::Mismatch { reason } => {
            assert!(reason.contains("snapshot"), "names the field: {reason}")
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn a_different_platform_is_a_mismatch() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    populate_cache_dir(&cache_dir);
    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    let mut expected = a_stamp();
    expected.platform = platform("8.6");

    match read_cache_stamp(data_subdir.path(), &cache_dir, &expected) {
        CacheStatus::Mismatch { reason } => {
            assert!(reason.contains("platform"), "names the field: {reason}")
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn an_unparseable_stamp_is_a_mismatch_not_a_crash() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    populate_cache_dir(&cache_dir);
    fs::write(cache_stamp_path(data_subdir.path()), b"{not json").expect("a garbled stamp");

    match read_cache_stamp(data_subdir.path(), &cache_dir, &a_stamp()) {
        CacheStatus::Mismatch { .. } => {}
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn a_missing_cache_dir_is_a_mismatch_even_with_a_matching_stamp() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    // Never created — a hand-deleted directory, or one that never existed.
    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    match read_cache_stamp(data_subdir.path(), &cache_dir, &a_stamp()) {
        CacheStatus::Mismatch { reason } => {
            assert!(
                reason.contains("missing or empty"),
                "names the cause: {reason}"
            )
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn an_empty_cache_dir_is_a_mismatch_even_with_a_matching_stamp() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let cache_dir = data_subdir.path().join("cache");
    fs::create_dir_all(&cache_dir).expect("an empty cache dir");
    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    match read_cache_stamp(data_subdir.path(), &cache_dir, &a_stamp()) {
        CacheStatus::Mismatch { reason } => {
            assert!(
                reason.contains("missing or empty"),
                "names the cause: {reason}"
            )
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
}

#[test]
fn a_written_stamp_leaves_no_temp_file_behind() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");

    write_cache_stamp(data_subdir.path(), &a_stamp()).expect("a written stamp");

    assert!(!data_subdir.path().join("cache.json.tmp").exists());
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
fn prepare_dev_launch_hands_off_when_a_sibling_holds_the_serving_lock() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let data_subdir = data_dir.path().join("data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    let holder = spawn_lock_holder(&serving_lock_path(data_dir.path()));

    let locks = prepare_dev_launch(data_dir.path(), &data_subdir, "dev.local.demo", None);

    assert!(
        locks.is_none(),
        "a live sibling holding the serving lock is a hand-off"
    );
    kill_and_wait(holder);
}

#[test]
fn prepare_dev_launch_waits_for_a_dying_sibling_then_succeeds() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let data_subdir = data_dir.path().join("data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    let pid_file = data_dir.path().join("sidecar.pid");
    let mut holder = spawn_lock_holder(&tfsapp_core::process::lock_path(&pid_file));

    let waiting = {
        let data_dir = data_dir.path().to_path_buf();
        let data_subdir = data_subdir.clone();
        thread::spawn(move || prepare_dev_launch(&data_dir, &data_subdir, "dev.local.demo", None))
    };
    thread::sleep(Duration::from_millis(150));
    assert!(
        !waiting.is_finished(),
        "it must still be waiting on the dying sibling"
    );
    holder.kill().unwrap();
    holder.wait().unwrap();

    let locks = waiting.join().expect("the waiter did not panic");
    assert!(
        locks.is_some(),
        "the wait ends and the launch proceeds once the sibling is gone"
    );
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

// --- probe_run_lock (plan 013's rule 3 launch-side refusal, widened by
// plan 047 onto a scan of `runs/`) --------------------------------------------

#[test]
fn probe_run_lock_free_when_nothing_holds_it() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");

    let held = probe_run_lock(data_dir.path(), "dev.local.demo").expect("no I/O error");

    assert_eq!(held, RunLockHeld::Free);
}

#[test]
fn probe_run_lock_held_names_the_alias_from_the_record() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let runs_dir = data_dir.path().join("runs");
    fs::create_dir_all(&runs_dir).expect("a runs dir");
    let entry_path = runs_dir.join("1.lock");
    let _holder = tfsapp_core::process::try_lock_file(&entry_path)
        .unwrap()
        .unwrap();
    fs::write(&entry_path, format_run_entry("mcp-serve", Some(1234))).expect("an entry record");

    let held = probe_run_lock(data_dir.path(), "dev.local.demo").expect("no I/O error");

    let RunLockHeld::Held { active } = held else {
        panic!("expected Held");
    };
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].alias, Some("mcp-serve".to_string()));
}

#[test]
fn probe_run_lock_held_with_no_record_names_nothing() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let runs_dir = data_dir.path().join("runs");
    fs::create_dir_all(&runs_dir).expect("a runs dir");
    let entry_path = runs_dir.join("1.lock");
    // Held, but with nothing ever written to it — the narrow window between
    // `run`'s own lock acquisition and its first write, or an entry this
    // probe races with a concurrent writer on.
    let _holder = tfsapp_core::process::try_lock_file(&entry_path)
        .unwrap()
        .unwrap();

    let held = probe_run_lock(data_dir.path(), "dev.local.demo").expect("no I/O error");

    let RunLockHeld::Held { active } = held else {
        panic!("expected Held");
    };
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].alias, None);
}

#[test]
fn probe_run_lock_held_by_a_live_orphan() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let runs_dir = data_dir.path().join("runs");
    fs::create_dir_all(&runs_dir).expect("a runs dir");
    let entry_path = runs_dir.join("1.lock");
    let identifier = "dev.local.demo";
    let mut orphan = std::process::Command::new("sleep")
        .arg("30")
        .env("TFS_APP_IDENTIFIER", identifier)
        .spawn()
        .unwrap();
    fs::write(
        &entry_path,
        format_run_entry("mcp-serve", Some(orphan.id())),
    )
    .expect("an entry record");

    let held = probe_run_lock(data_dir.path(), identifier).expect("no I/O error");

    let RunLockHeld::Held { active } = held else {
        panic!("expected Held");
    };
    assert_eq!(active.len(), 1);
    assert!(active[0].orphaned);
    orphan.kill().unwrap();
    orphan.wait().unwrap();
}

#[test]
fn data_dir_holder_propagates_an_unprobeable_window_lock() {
    let root = tempfile::tempdir().expect("a temp root");
    let data_dir = root.path().join("not-a-directory");
    fs::write(&data_dir, "not a directory").expect("a file where a data directory would be");

    let error = data_dir_holder(&data_dir, "dev.local.demo")
        .expect_err("an unprobeable sidecar lock must not be treated as free");

    assert!(
        matches!(error.kind(), std::io::ErrorKind::NotADirectory),
        "unexpected error: {error}"
    );
}

// --- dialog_is_warranted --------------------------------------------------

#[test]
fn no_dialog_when_stderr_is_a_terminal() {
    // A developer at a terminal wants the line where they typed the command,
    // not a modal to dismiss.
    assert!(!dialog_is_warranted(true));
}

#[test]
fn a_dialog_when_stderr_is_not_a_terminal() {
    // A `.desktop` launch has no stderr anyone will read — the dialog is the
    // whole answer there.
    assert!(dialog_is_warranted(false));
}

// --- the rollback anchor's database half: snapshot_db / restore_db_snapshot
// -----------------------------------------------------------------------

#[test]
fn a_snapshot_round_trips_with_a_hot_wal_present() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    fs::write(data_subdir.path().join("app.db"), b"main").expect("a db file");
    fs::write(data_subdir.path().join("app.db-wal"), b"wal").expect("a wal file");
    // No `-shm` — a snapshot must handle an absent twin, not just a present
    // one.

    snapshot_db(data_subdir.path()).expect("a snapshot");
    for name in DB_FILE_NAMES {
        fs::remove_file(data_subdir.path().join(name)).ok();
    }
    restore_db_snapshot(data_subdir.path()).expect("a restore");

    assert_eq!(
        fs::read(data_subdir.path().join("app.db")).expect("the main file"),
        b"main"
    );
    assert_eq!(
        fs::read(data_subdir.path().join("app.db-wal")).expect("the wal twin"),
        b"wal"
    );
    assert!(
        !data_subdir.path().join("app.db-shm").exists(),
        "a twin absent at snapshot time must not appear on restore"
    );
}

#[test]
fn a_restore_removes_a_wal_that_appeared_after_the_snapshot() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    fs::write(data_subdir.path().join("app.db"), b"main").expect("a db file");
    // No hot WAL at snapshot time.
    snapshot_db(data_subdir.path()).expect("a snapshot");

    // A WAL appears afterwards — the live file a failed event created along
    // the way, or a hot WAL left by an unrelated crash.
    fs::write(data_subdir.path().join("app.db-wal"), b"stray").expect("a wal file");

    restore_db_snapshot(data_subdir.path()).expect("a restore");

    assert!(
        !data_subdir.path().join("app.db-wal").exists(),
        "restoring a main file must not leave a foreign WAL beside it"
    );
}

#[test]
fn discarding_a_db_snapshot_leaves_the_live_database_untouched() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    fs::write(data_subdir.path().join("app.db"), b"live").expect("a db file");
    snapshot_db(data_subdir.path()).expect("a snapshot");

    discard_db_snapshot(data_subdir.path());

    assert!(!db_snapshot_path(data_subdir.path(), "app.db").exists());
    assert_eq!(
        fs::read(data_subdir.path().join("app.db")).expect("the live file"),
        b"live"
    );
}

#[test]
fn discarding_an_absent_db_snapshot_is_not_an_error() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    discard_db_snapshot(data_subdir.path()); // must not panic
}

#[test]
fn rescue_dumps_reserve_a_planted_candidate_and_preserve_both_byte_streams() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let source = data_subdir.path().join("app.db");
    let first = data_subdir.path().join("app.db.rescue-fixed");
    fs::write(&source, b"first rescue").expect("write the source database");
    fs::write(&first, b"existing rescue").expect("plant the first candidate");
    let second = copy_rescue_dump_at(&source, &first).expect("reserve a suffix");

    assert_eq!(second, data_subdir.path().join("app.db.rescue-fixed-2"));
    assert_eq!(fs::read(&first).unwrap(), b"existing rescue");
    assert_eq!(fs::read(&second).unwrap(), b"first rescue");
}

#[test]
fn concurrent_rescue_dumps_reserve_distinct_paths_without_overwriting() {
    use std::sync::{Arc, Barrier};

    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let source = data_subdir.path().join("app.db");
    let base = data_subdir.path().join("app.db.rescue-fixed");
    fs::write(&source, b"database bytes").expect("write the source database");
    let barrier = Arc::new(Barrier::new(2));
    let first = std::thread::scope(|scope| {
        let child_barrier = Arc::clone(&barrier);
        let child_source = source.clone();
        let child_base = base.clone();
        let left = scope.spawn(move || {
            child_barrier.wait();
            copy_rescue_dump_at(&child_source, &child_base).expect("first concurrent copy")
        });
        barrier.wait();
        let second = copy_rescue_dump_at(&source, &base).expect("second concurrent copy");
        (left.join().unwrap(), second)
    });
    assert_ne!(first.0, first.1);
    assert_eq!(fs::read(first.0).unwrap(), b"database bytes");
    assert_eq!(fs::read(first.1).unwrap(), b"database bytes");
}

#[test]
fn rescue_announcement_names_a_pattern_not_a_reserved_path() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    assert_eq!(
        rescue_dump_pattern(data_subdir.path(), "app.db"),
        data_subdir.path().join("app.db.rescue-<timestamp>[-N]")
    );
}

// --- the rollback anchor's tree half: previous_tree_path -------------------

#[test]
fn the_previous_tree_path_appends_previous_to_the_app_dir() {
    let app_dir = Path::new("/apps/demo");
    assert_eq!(
        previous_tree_path(app_dir),
        Path::new("/apps/demo.previous")
    );
}

// --- the rollback anchor's registry half: rollback.json --------------------

#[test]
fn a_written_rollback_anchor_reads_back() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let anchor = RollbackAnchor {
        app_version: "0.5.0".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        created_at: "2026-08-10T00:00:00Z".to_string(),
    };

    write_rollback_anchor(data_subdir.path(), &anchor).expect("a write");

    assert_eq!(read_rollback_anchor(data_subdir.path()), Some(anchor));
}

#[test]
fn a_missing_rollback_anchor_reads_as_none() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    assert_eq!(read_rollback_anchor(data_subdir.path()), None);
}

#[test]
fn an_unknown_key_does_not_make_a_rollback_anchor_unreadable() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    fs::write(
        rollback_anchor_path(data_subdir.path()),
        r#"{"app_version": "0.5.0", "source_revision": "sha256:deadbeef", "created_at": "2026-08-10T00:00:00Z", "from_a_newer_hub": true}"#,
    )
    .expect("a hand-written anchor");

    assert_eq!(
        read_rollback_anchor(data_subdir.path()),
        Some(RollbackAnchor {
            app_version: "0.5.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            created_at: "2026-08-10T00:00:00Z".to_string(),
        })
    );
}

#[test]
fn writing_a_rollback_anchor_leaves_no_temp_file_behind() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    write_rollback_anchor(
        data_subdir.path(),
        &RollbackAnchor {
            app_version: "0.5.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            created_at: "2026-08-10T00:00:00Z".to_string(),
        },
    )
    .expect("a write");

    assert!(!data_subdir.path().join("rollback.json.tmp").exists());
}

#[test]
fn discarding_the_anchor_record_leaves_it_unreadable() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    write_rollback_anchor(
        data_subdir.path(),
        &RollbackAnchor {
            app_version: "0.5.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            created_at: "2026-08-10T00:00:00Z".to_string(),
        },
    )
    .expect("a write");

    discard_rollback_anchor(data_subdir.path());

    assert_eq!(read_rollback_anchor(data_subdir.path()), None);
}

// --- anchor_state: three halves or Missing ----------------------------------

/// Everything a `Complete` anchor needs, written under `data_subdir` and
/// `app_dir`'s sibling `.previous` tree.
fn write_complete_anchor(data_subdir: &Path, app_dir: &Path) {
    fs::create_dir_all(previous_tree_path(app_dir)).expect("a retained tree");
    fs::write(data_subdir.join("app.db"), b"pre-update").expect("a live db");
    snapshot_db(data_subdir).expect("a db snapshot");
    write_rollback_anchor(
        data_subdir,
        &RollbackAnchor {
            app_version: "0.5.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            created_at: "2026-08-10T00:00:00Z".to_string(),
        },
    )
    .expect("an anchor record");
}

#[test]
fn all_three_halves_present_reads_as_complete() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    write_complete_anchor(data_subdir.path(), &app_dir);

    assert_eq!(
        anchor_state(data_subdir.path(), &app_dir),
        Anchor::Complete {
            app_version: "0.5.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            created_at: "2026-08-10T00:00:00Z".to_string(),
        }
    );
}

#[test]
fn a_missing_retained_tree_reads_as_missing() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    write_complete_anchor(data_subdir.path(), &app_dir);
    fs::remove_dir_all(previous_tree_path(&app_dir)).expect("dropping the retained tree");

    assert_eq!(anchor_state(data_subdir.path(), &app_dir), Anchor::Missing);
}

#[test]
fn a_missing_db_snapshot_reads_as_missing() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    write_complete_anchor(data_subdir.path(), &app_dir);
    fs::remove_file(db_snapshot_path(data_subdir.path(), "app.db")).expect("dropping the snapshot");

    assert_eq!(anchor_state(data_subdir.path(), &app_dir), Anchor::Missing);
}

#[test]
fn a_missing_rollback_json_reads_as_missing() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let apps_root = tempfile::tempdir().expect("a temp apps root");
    let app_dir = apps_root.path().join("demo");
    write_complete_anchor(data_subdir.path(), &app_dir);
    discard_rollback_anchor(data_subdir.path());

    assert_eq!(anchor_state(data_subdir.path(), &app_dir), Anchor::Missing);
}

#[test]
fn nothing_at_all_reads_as_missing() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    let app_dir = Path::new("/apps/never-installed");

    assert_eq!(anchor_state(data_subdir.path(), app_dir), Anchor::Missing);
}

// --- close-guard gating (plan 055 step 3) ------------------------------------
//
// The decision-to-effect rules, tested at the same level as `veto_exit`: the
// mappings are pure, and the pieces that need a runtime go through the mock
// one. What stays beyond every test here — the real dialog, a real hide, a
// real teardown — is step 4's native validation.

use crate::close_guard::{CloseFlow, CloseGuardState, SharedCloseGuards};

fn guard_state_with_frontend_guard(window: &str, id: &str) -> SharedCloseGuards {
    let state = std::sync::Arc::new(CloseGuardState::new());
    let context = state.context(window);
    state
        .frontend_register(window, &context, id)
        .expect("a registered guard");
    state
}

#[test]
fn a_busy_or_confirmed_close_vetoes_the_default_close() {
    // One decision at a time across the app's windows: a repeated click, or
    // a second window closing while a dialog stands, lands on `KeepOpen`.
    // Stacking a second dialog is how two closes both conclude they are the
    // last one.
    assert_eq!(close_action(&CloseFlow::Busy, false), CloseAction::KeepOpen);
    assert_eq!(
        close_action(
            &CloseFlow::Confirm {
                token: "t".to_string(),
                frontend: Default::default(),
                backend: Default::default(),
            },
            false
        ),
        CloseAction::KeepOpen
    );
    assert_eq!(
        close_action(
            &CloseFlow::Confirm {
                token: "t".to_string(),
                frontend: Default::default(),
                backend: Default::default(),
            },
            true
        ),
        CloseAction::KeepOpen
    );
}

#[test]
fn an_allowed_close_keeps_todays_behavior() {
    // Nothing to protect: the last window with a sidecar takes the existing
    // hide-and-teardown path, a secondary window closes by the default.
    assert_eq!(
        close_action(&CloseFlow::Allow, true),
        CloseAction::HideAndTearDown
    );
    assert_eq!(close_action(&CloseFlow::Allow, false), CloseAction::Default);
}

#[test]
fn a_cancelled_close_performs_no_hide_destroy_or_stop() {
    let state = guard_state_with_frontend_guard("main", "editor:42");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };

    // Cancel — the dialog's `false`, which Escape and dismiss also answer.
    assert_eq!(
        close_effect(state.resolve_close(&token, false, false)),
        CloseEffect::Nothing
    );
    // The guard survives the cancellation; the window stays open with it.
    assert!(state.frontend_guards("main").contains("editor:42"));

    // The refusals that were never a person's answer at all: a callback
    // with a token the state never issued, and a decision whose window was
    // destroyed while its dialog stood.
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks again");
    };
    assert_eq!(
        close_effect(state.resolve_close("never-issued", true, false)),
        CloseEffect::Nothing
    );
    state.drop_window("main");
    assert_eq!(
        close_effect(state.resolve_close(&token, true, false)),
        CloseEffect::Nothing
    );
}

#[test]
fn an_approved_secondary_close_keeps_the_backend_alive() {
    let state = guard_state_with_frontend_guard("main", "editor:42");
    state
        .backend_register("export:job-1")
        .expect("a backend guard");
    // `main` is not the last window here, so the backend guards are not at
    // stake and the backend keeps running through this close.
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };

    let effect = close_effect(state.resolve_close(&token, true, false));
    assert_eq!(effect, CloseEffect::CloseWindow);
    // The backend's protection is untouched by closing this window.
    assert!(state.backend_guards().contains("export:job-1"));
}

#[test]
fn an_approved_final_close_tears_down_once() {
    let state = guard_state_with_frontend_guard("main", "editor:42");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("a guarded close asks");
    };

    assert_eq!(
        close_effect(state.resolve_close(&token, true, true)),
        CloseEffect::HideAndTearDown
    );
    // The decision was one-time: a repeated or late answer authorises
    // nothing, so the teardown that already started is the only one.
    assert_eq!(
        close_effect(state.resolve_close(&token, true, true)),
        CloseEffect::Nothing
    );
}

#[test]
fn a_signal_commits_shutdown_and_invalidates_the_pending_approval() {
    // A termination signal reaches `stop_sidecar_and_exit`, whose first step
    // is the one-way shutdown commitment — the pending dialog's late answer
    // goes stale, and mandatory shutdown never waits on a person.
    let state = guard_state_with_frontend_guard("main", "editor:42");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("a guarded close asks");
    };

    state.commit_shutdown();

    assert_eq!(
        close_effect(state.resolve_close(&token, true, true)),
        CloseEffect::Nothing
    );
    assert!(state.is_closing());
}

#[test]
fn the_three_warnings_are_distinct_and_name_no_guard_ids() {
    let set = |values: &[&str]| -> std::collections::BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    };
    let frontend = set(&["editor:42"]);
    let backend = set(&["export:job-1"]);

    let (frontend_title, frontend_body) = close_warning(&frontend, &set(&[]));
    let (backend_title, backend_body) = close_warning(&set(&[]), &backend);
    let (both_title, both_body) = close_warning(&frontend, &backend);

    assert_eq!(frontend_title, "Unsaved changes");
    assert_eq!(backend_title, "Background work");
    assert_eq!(both_title, "Unsaved changes and background work");
    assert_ne!(frontend_body, backend_body);
    assert_ne!(both_body, frontend_body);
    assert_ne!(both_body, backend_body);

    // The guard IDs are app-chosen and may name documents or jobs; the
    // person is told what is at stake, never the identifiers.
    for body in [frontend_body, backend_body, both_body] {
        assert!(!body.contains("editor:42"));
        assert!(!body.contains("export:job-1"));
    }
}

#[test]
fn a_destroyed_window_drops_its_guards_through_the_event_handler() {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

    let app = tauri::test::mock_app();
    let state = guard_state_with_frontend_guard("main", "editor:42");
    app.manage(state.clone());
    let window = WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
        .build()
        .expect("a window")
        .as_ref()
        .window();

    on_window_event(&window, &tauri::WindowEvent::Destroyed);

    assert!(state.frontend_guards("main").is_empty());
}

#[test]
fn close_stops_backend_counts_windows_and_the_sidecar_on_the_mock_runtime() {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

    let app = tauri::test::mock_app();
    let state = std::sync::Arc::new(CloseGuardState::new());
    app.manage(state);

    // No sidecar yet — the shape before `serve` finishes. The window only
    // has to exist for the topology question; nothing is done to it.
    WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
        .build()
        .expect("a window");
    assert!(!close_stops_backend(app.handle(), "main"));

    // The managed sidecar is what makes the last window's close a backend
    // stop. A minimal one: no server, no workers — `stop` on it is never
    // called here, only `try_state` finds it.
    app.manage(std::sync::Mutex::new(crate::sidecar::Sidecar {
        server: None,
        workers: vec![],
        shutting_down: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pid_file: std::path::PathBuf::new(),
        lock: None,
        serving: None,
    }));
    assert!(close_stops_backend(app.handle(), "main"));

    // A second window keeps the backend alive through either close.
    WebviewWindowBuilder::new(&app, "main-2", WebviewUrl::App("index.html".into()))
        .build()
        .expect("another window");
    assert!(!close_stops_backend(app.handle(), "main"));
    assert!(!close_stops_backend(app.handle(), "main-2"));
}

// --- close commitment coordination (plan 055 step 5, audit 016 findings 1–2) --
//
// The production coordinator runs on the event loop against `TauriCloseWorld`;
// these regressions drive the same coordinator functions with a recording
// world, so the interleavings assert *effects and ownership* — which window
// was hidden, which destroyed, how many teardowns — rather than enum
// mappings alone. A destruction is only *posted* by `destroy`, exactly as in
// production, and observed exactly where a scenario stages its `Destroyed`
// event: `observe_destruction`.

use crate::close_guard::GuardError;

/// A bare shared guard state — the shape a launch manages before its first
/// window.
fn shared_close_state() -> SharedCloseGuards {
    std::sync::Arc::new(CloseGuardState::new())
}

struct RecordedDialog {
    window: String,
    token: String,
    frontend: std::collections::BTreeSet<String>,
    backend: std::collections::BTreeSet<String>,
}

struct RecordedWorld {
    labels: Vec<String>,
    has_backend: bool,
    /// The one effect failure the coordinator must recover from: the close
    /// request could not even be posted and the window stays usable.
    fail_request_close: bool,
    /// The same one-shot claim `stop_sidecar_and_exit` performs on
    /// `TEARDOWN_IN_FLIGHT`: a second teardown dispatch records nothing,
    /// because the second arrival returns at the latch.
    teardown_claimed: bool,
    /// The native close requests the coordinator posted, awaiting the
    /// `CloseRequested` dispatch that will consume them — what the runtime
    /// hands back through `on_close_requested` in production.
    pending_requests: std::collections::VecDeque<String>,
    effects: Vec<String>,
    dialogs: Vec<RecordedDialog>,
}

impl RecordedWorld {
    /// Two windows on a shared backend — the shape every audit-016
    /// reproduction starts from.
    fn two_windows() -> Self {
        Self {
            labels: vec!["main".to_string(), "main-2".to_string()],
            has_backend: true,
            fail_request_close: false,
            teardown_claimed: false,
            pending_requests: Default::default(),
            effects: vec![],
            dialogs: vec![],
        }
    }

    /// The last window on a shared backend.
    fn one_window() -> Self {
        let mut world = Self::two_windows();
        world.labels.pop();
        world
    }

    /// Observe the destruction a scenario approved: the label stops counting
    /// and the guard state releases the window's reservation and guards —
    /// what the `Destroyed` event does in production.
    fn observe_destruction(&mut self, state: &SharedCloseGuards, window: &str) {
        self.labels.retain(|label| label != window);
        state.drop_window(window);
    }

    fn effect_count(&self, effect: &str) -> usize {
        self.effects
            .iter()
            .filter(|recorded| *recorded == effect)
            .count()
    }

    /// Consume one posted close request the way the runtime does: hand it
    /// back as a `CloseRequested`, which in production lands in
    /// `handle_close_request` — the same dispatch that proceeds to the
    /// native destruction when the answer is not a veto.
    fn consume_pending_close(&mut self, state: &SharedCloseGuards) -> Option<CloseAction> {
        let window = self.pending_requests.pop_front()?;
        Some(handle_close_request(state, self, &window))
    }
}

impl CloseWorld for RecordedWorld {
    fn window_labels(&self) -> Vec<String> {
        self.labels.clone()
    }

    fn has_backend(&self) -> bool {
        self.has_backend
    }

    fn hide(&mut self, window: &str) {
        self.effects.push(format!("hide:{window}"));
    }

    fn request_close(&mut self, window: &str) -> bool {
        if self.fail_request_close {
            return false;
        }
        self.effects.push(format!("request-close:{window}"));
        self.pending_requests.push_back(window.to_string());
        true
    }

    fn start_teardown(&mut self) {
        if self.teardown_claimed {
            return;
        }
        self.teardown_claimed = true;
        self.effects.push("teardown".to_string());
    }

    fn open_confirmation(
        &mut self,
        window: &str,
        token: &str,
        frontend: &std::collections::BTreeSet<String>,
        backend: &std::collections::BTreeSet<String>,
    ) {
        self.dialogs.push(RecordedDialog {
            window: window.to_string(),
            token: token.to_string(),
            frontend: frontend.clone(),
            backend: backend.clone(),
        });
    }
}

#[test]
fn an_approved_close_not_yet_destroyed_is_never_a_survivor_for_the_next_close() {
    // Audit 016, finding 1: A's close was approved and its destruction only
    // posted; B then closed with a backend guard standing. Counting A as a
    // survivor let B through without a backend warning, and the backend
    // stopped with live, unacknowledged work.
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    state.backend_register("export:1").expect("registration");
    let mut world = RecordedWorld::two_windows();

    // A's close: B survives it, so only the dirty document is at stake.
    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    assert_eq!(world.dialogs[0].window, "main");
    assert!(world.dialogs[0].frontend.contains("editor:42"));
    let a_token = world.dialogs[0].token.clone();

    // The person approves; the close request is posted, not yet consumed by
    // the `CloseRequested` dispatch that will decide it.
    handle_close_answer(&state, &mut world, "main", &a_token, true);
    assert_eq!(world.effects, ["request-close:main"]);

    // B closes before A's `Destroyed` event: A is committed, so this close
    // stops the backend — the backend guard is at stake and must be asked
    // about, never silently outrun.
    assert_eq!(
        handle_close_request(&state, &mut world, "main-2"),
        CloseAction::KeepOpen,
        "the committed close makes B the last surviving window"
    );
    assert_eq!(world.dialogs.len(), 2);
    assert_eq!(world.dialogs[1].window, "main-2");
    assert!(world.dialogs[1].backend.contains("export:1"));
    assert_eq!(world.effect_count("teardown"), 0);

    // Approving B's dialog hides B and tears down exactly once — the
    // teardown owns A's destruction too, so its pending request is never
    // consumed.
    let b_token = world.dialogs[1].token.clone();
    handle_close_answer(&state, &mut world, "main-2", &b_token, true);
    assert_eq!(world.effect_count("hide:main-2"), 1);
    assert_eq!(world.effect_count("teardown"), 1);
    assert_eq!(world.effect_count("request-close:main"), 1);

    // Observing both destructions releases both reservations and guards.
    world.observe_destruction(&state, "main");
    world.observe_destruction(&state, "main-2");
    assert!(state.committed_closing_windows().is_empty());
    assert!(state.frontend_guards("main-2").is_empty());
}

#[test]
fn two_nearly_simultaneous_clean_closes_cannot_both_keep_the_backend() {
    // Audit 016, finding 2's interleaving over the same race as finding 1,
    // with nothing guarded at all: A's committed close is invisible until
    // its `Destroyed` event, so B's close must find itself the last one and
    // take the hide-and-teardown path — and the teardown itself must run
    // once, whatever B's close repeats after commitment.
    let state = shared_close_state();
    let mut world = RecordedWorld::two_windows();

    // A closes cleanly while B survives it: the default close, reserved
    // until its destruction is observed.
    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::Default
    );
    assert_eq!(
        state.committed_closing_windows(),
        ["main".to_string()].into_iter().collect()
    );

    // B closes before A's `Destroyed` event: A is not a survivor, so this
    // is the backend-stopping close — it hides B and tears down.
    assert_eq!(
        handle_close_request(&state, &mut world, "main-2"),
        CloseAction::HideAndTearDown
    );
    assert!(
        state.is_closing(),
        "the unguarded last close commits shutdown"
    );
    assert_eq!(
        state.backend_register("job:new"),
        Err(GuardError::Closing),
        "a registration landing after the commitment is refused, never \
         accepted and ignored by a teardown that revalidates nothing"
    );

    // A repeated close of the committed window dispatches the same effects;
    // the one-shot latch keeps them to a single teardown.
    assert_eq!(
        handle_close_request(&state, &mut world, "main-2"),
        CloseAction::HideAndTearDown
    );
    assert_eq!(world.effect_count("hide:main-2"), 2);
    assert_eq!(world.effect_count("teardown"), 1);
}

#[test]
fn a_backend_registration_can_no_longer_land_between_the_check_and_the_stop() {
    // Audit 016, finding 2, the guarded half: the final approval used to
    // authorise the stop under the mutex and release it without committing
    // `closing`, so a bridge call in between received a success for a guard
    // the teardown then ignored.
    let state = shared_close_state();
    state.backend_register("job:old").expect("registration");
    let mut world = RecordedWorld::one_window();

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    assert!(world.dialogs[0].backend.contains("job:old"));
    let token = world.dialogs[0].token.clone();

    handle_close_answer(&state, &mut world, "main", &token, true);
    assert_eq!(world.effect_count("hide:main"), 1);
    assert_eq!(world.effect_count("teardown"), 1);

    // The approval committed the shutdown in the same transition as its
    // final check: a registration landing now is refused, not ignored.
    assert!(state.is_closing());
    assert_eq!(state.backend_register("job:new"), Err(GuardError::Closing));
}

#[test]
fn a_new_backend_guard_during_confirmation_cannot_be_overlooked() {
    // A bridge call can register work while a GTK dialog stands — the
    // application modality stops clicks, not HTTP. The answer must ask
    // again with a dialog that covers the new guard, never apply the old
    // approval to new stakes.
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let mut world = RecordedWorld::one_window();

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    let first_token = world.dialogs[0].token.clone();

    // The live probe arrives while the dialog stands.
    state.backend_register("export:live").expect("registration");

    handle_close_answer(&state, &mut world, "main", &first_token, true);
    assert_eq!(world.dialogs.len(), 2, "the answer asked again");
    assert!(world.dialogs[1].backend.contains("export:live"));
    assert!(
        world.effects.is_empty(),
        "no hide, no destroy, no teardown came out of the stale approval"
    );

    // The fresh dialog covers both halves; approving it tears down.
    let fresh_token = world.dialogs[1].token.clone();
    handle_close_answer(&state, &mut world, "main", &fresh_token, true);
    assert_eq!(world.effect_count("hide:main"), 1);
    assert_eq!(world.effect_count("teardown"), 1);
}

#[test]
fn a_destruction_that_cannot_even_be_posted_releases_the_close() {
    // The plan's recovery rule: an effect that failed while the window
    // remained usable must not leave every later close stuck behind a
    // dead reservation.
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    state.backend_register("export:1").expect("registration");
    let mut world = RecordedWorld::two_windows();
    world.fail_request_close = true;

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    let token = world.dialogs[0].token.clone();
    handle_close_answer(&state, &mut world, "main", &token, true);

    // The destruction failed and the window stayed usable: the close is
    // not committed after all, and the document registers again.
    assert!(state.committed_closing_windows().is_empty());
    state
        .frontend_register("main", &context, "editor:43")
        .expect("a usable window's document registers again");

    // A counts as a survivor again: B's close does not stop the backend,
    // so the backend guard does not make it warn.
    assert_eq!(
        handle_close_request(&state, &mut world, "main-2"),
        CloseAction::Default
    );
    assert_eq!(world.effect_count("teardown"), 0);
}

#[test]
fn a_window_admitted_before_commitment_changes_the_topology_of_the_answer() {
    // The last window's close opens a dialog; a second instance opens
    // another window while it stands. The answer honours the new topology:
    // the close no longer stops the backend, so no hide, no teardown — and
    // the approval destroys exactly its own window.
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let mut world = RecordedWorld::one_window();

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    let token = world.dialogs[0].token.clone();

    // The second-instance window arrives before the answer commits
    // anything — the arrival path `second_instance_action` admits.
    world.labels.push("main-2".to_string());
    handle_close_answer(&state, &mut world, "main", &token, true);

    assert_eq!(world.effects, ["request-close:main"]);
    assert!(!state.is_closing(), "the backend now survives this close");

    // The posted request lands back as a `CloseRequested`, decided at the
    // same dispatch that proceeds to the native destruction: the document
    // is unchanged, so the approval closes exactly its own window.
    assert_eq!(
        world.consume_pending_close(&state),
        Some(CloseAction::Default)
    );
    world.observe_destruction(&state, "main");
    assert!(state.committed_closing_windows().is_empty());
    assert_eq!(world.labels, ["main-2".to_string()]);
}

#[test]
fn a_cancelled_close_keeps_the_window_usable_and_commits_nothing() {
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let mut world = RecordedWorld::two_windows();

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    let token = world.dialogs[0].token.clone();

    handle_close_answer(&state, &mut world, "main", &token, false);
    assert!(
        world.effects.is_empty(),
        "cancellation performs no hide, no destroy, no teardown"
    );
    assert!(state.committed_closing_windows().is_empty());
    state
        .frontend_register("main", &context, "editor:43")
        .expect("the window is usable and its document registers again");

    // And the refused close can be asked again afresh.
    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    assert_eq!(world.dialogs.len(), 2);
}

#[test]
fn the_teardown_latch_admits_exactly_one_owner() {
    // Audit 016, finding 2's second half: `TEARDOWN_IN_FLIGHT` used to be a
    // plain store, so an approved final close and a signal arriving
    // together could both run the teardown. The conditional take is what
    // keeps the second arrival out.
    let latch = std::sync::atomic::AtomicBool::new(false);
    assert!(claim_teardown(&latch));
    assert!(!claim_teardown(&latch));
    assert!(!claim_teardown(&latch));
}

// --- document-identity binding (plan 055 step 6, audit 016 finding 3) -------

#[test]
fn a_replacement_during_the_dialog_leaves_the_successor_open() {
    // A programmatically scheduled navigation replaces the document while
    // its close dialog stands. Approving the old dialog must close
    // nothing — not the successor, whatever guard IDs it reuses — and must
    // not stack a second native dialog while the first one is still up.
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let mut world = RecordedWorld::two_windows();

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    assert_eq!(world.dialogs.len(), 1);

    // The replacement, then the successor's own registration of the same
    // guard ID under its own identity.
    state.rotate_context("main");
    let fresh = state.context("main");
    state
        .frontend_register("main", &fresh, "editor:42")
        .expect("the successor's own guard");

    let token = world.dialogs[0].token.clone();
    handle_close_answer(&state, &mut world, "main", &token, true);
    assert!(
        world.effects.is_empty(),
        "the stale approval performed no hide, no destroy, no teardown"
    );
    assert_eq!(
        world.dialogs.len(),
        1,
        "the invalidation consumed the answer without stacking a second dialog"
    );

    // The successor keeps its guard, and a new explicit close request
    // starts a decision of its own.
    assert!(state.frontend_guards("main").contains("editor:42"));
    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    assert_eq!(world.dialogs.len(), 2);
}

// --- committed closes (plan 055, simplified step 11) -----------------------

#[test]
fn a_reload_after_approval_keeps_the_close_and_backend_topology_committed() {
    let state = shared_close_state();
    let context = state.context("main");
    state.frontend_register("main", &context, "editor").unwrap();
    state.backend_register("export").unwrap();
    let mut world = RecordedWorld::two_windows();
    handle_close_request(&state, &mut world, "main");
    let token = world.dialogs[0].token.clone();
    handle_close_answer(&state, &mut world, "main", &token, true);
    assert_eq!(world.effects, ["request-close:main"]);

    // Approval committed the window close. A reload does not turn it back
    // into a survivor; closing the remaining window must warn about the job.
    state.rotate_context("main");
    assert_eq!(
        handle_close_request(&state, &mut world, "main-2"),
        CloseAction::KeepOpen
    );
    assert!(world.dialogs[1].backend.contains("export"));

    // The already-approved close proceeds even while that dialog is open.
    assert_eq!(
        world.consume_pending_close(&state),
        Some(CloseAction::Default)
    );
    world.observe_destruction(&state, "main");
    assert_eq!(world.labels, ["main-2"]);
    assert_eq!(world.effect_count("teardown"), 0);
    assert!(state.backend_guards().contains("export"));
    let token = world.dialogs[1].token.clone();
    handle_close_answer(&state, &mut world, "main-2", &token, false);
    assert!(!state.is_closing());
}

#[test]
fn a_queued_close_consumed_unchanged_closes_its_window_once() {
    // The inverse order, document unchanged: the posted request's dispatch
    // authorizes the default close, repeated requests are absorbed rather
    // than stacked, and the destruction observation releases the window's
    // reservation and guards.
    let state = shared_close_state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let mut world = RecordedWorld::two_windows();

    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::KeepOpen
    );
    let token = world.dialogs[0].token.clone();
    handle_close_answer(&state, &mut world, "main", &token, true);
    assert_eq!(world.effects, ["request-close:main"]);

    // The dispatch of the posted request, document unchanged: the default
    // close proceeds — no veto, no dialog, no teardown.
    assert_eq!(
        world.consume_pending_close(&state),
        Some(CloseAction::Default)
    );
    assert_eq!(world.dialogs.len(), 1);
    assert_eq!(world.effect_count("teardown"), 0);

    // A repeated close request while the close is already happening is
    // absorbed, not stacked — and posts nothing new.
    assert_eq!(
        handle_close_request(&state, &mut world, "main"),
        CloseAction::Default
    );
    assert!(world.pending_requests.is_empty());

    // The destruction is observed: the reservation and the guards go, and
    // the surviving window's closes decide on a topology without `main`.
    world.observe_destruction(&state, "main");
    assert!(state.committed_closing_windows().is_empty());
    assert!(state.frontend_guards("main").is_empty());
    assert_eq!(world.labels, ["main-2".to_string()]);
    assert_eq!(world.effect_count("request-close:main"), 1);
}

#[test]
fn a_mismatched_callback_cannot_apply_its_decision_to_another_window() {
    // The dialog callback binds its window and token together, so this is
    // a shape no production callback can build — driven anyway to prove
    // the final application boundary: an answer that somehow names
    // another window with main-2's token may consume main-2's decision,
    // but its queued effect cannot be dispatched onto the named window.
    let state = shared_close_state();
    let context = state.context("main-2");
    state
        .frontend_register("main-2", &context, "editor:42")
        .expect("registration");
    let mut world = RecordedWorld::two_windows();

    assert_eq!(
        handle_close_request(&state, &mut world, "main-2"),
        CloseAction::KeepOpen
    );
    let token = world.dialogs[0].token.clone();

    handle_close_answer(&state, &mut world, "main", &token, true);
    assert!(
        world.effects.is_empty(),
        "no destroy was dispatched onto the named window"
    );
}
