use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use super::{
    acquire_launch_locks, anchor_state, cache_stamp_path, db_snapshot_path, decide_launch,
    dialog_is_warranted, discard_db_snapshot, discard_rollback_anchor, lifecycle_decision,
    prepare_dev_launch, previous_tree_path, probe_run_lock, read_cache_stamp, read_data_version,
    read_rollback_anchor, rescue_dump_path, restore_db_snapshot, rollback_anchor_path,
    run_lock_held_decision, serving_lock_path, snapshot_db, veto_exit, write_cache_stamp,
    write_data_version, write_rollback_anchor, Anchor, CacheStamp, CacheStatus, LaunchDecision,
    LaunchLockError, LifecycleDecisionError, LifecycleError, LifecycleEvent, RollbackAnchor,
    RunLockHeld, DB_FILE_NAMES,
};
use crate::{registry::Platform, run::OrphanedRun};

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

// --- probe_run_lock (plan 013's rule 3 launch-side refusal) ------------------

#[test]
fn a_live_orphan_is_held_with_its_alias() {
    let held = run_lock_held_decision(
        false,
        None,
        OrphanedRun::ActiveOrphan {
            alias: "migrate".to_string(),
            pid: 42,
        },
    );

    assert_eq!(
        held,
        RunLockHeld::Held {
            alias: Some("migrate".to_string())
        }
    );
}

#[test]
fn a_stale_free_lock_allows_the_data_dir() {
    assert_eq!(
        run_lock_held_decision(false, None, OrphanedRun::Stale),
        RunLockHeld::Free
    );
}

#[test]
fn probe_run_lock_free_when_nothing_holds_it() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");

    let held = probe_run_lock(data_dir.path(), "dev.local.demo").expect("no I/O error");

    assert_eq!(held, RunLockHeld::Free);
}

#[test]
fn probe_run_lock_held_names_the_alias_from_the_record() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let run_lock_path = data_dir.path().join("run.lock");
    let _holder = tfsapp_core::process::try_lock_file(&run_lock_path)
        .unwrap()
        .unwrap();
    fs::write(&run_lock_path, "mcp-serve\n1234").expect("a run.lock record");

    let held = probe_run_lock(data_dir.path(), "dev.local.demo").expect("no I/O error");

    assert_eq!(
        held,
        RunLockHeld::Held {
            alias: Some("mcp-serve".to_string())
        }
    );
}

#[test]
fn probe_run_lock_held_with_no_record_names_nothing() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let run_lock_path = data_dir.path().join("run.lock");
    // Held, but with nothing ever written to it — the narrow window between
    // `run`'s own lock acquisition and its first write, or a lock file this
    // probe races with a concurrent writer on.
    let _holder = tfsapp_core::process::try_lock_file(&run_lock_path)
        .unwrap()
        .unwrap();

    let held = probe_run_lock(data_dir.path(), "dev.local.demo").expect("no I/O error");

    assert_eq!(held, RunLockHeld::Held { alias: None });
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
fn the_rescue_dump_path_names_a_rescue_twin_beside_the_live_file() {
    let data_subdir = Path::new("/tmp/does-not-need-to-exist");
    assert_eq!(
        rescue_dump_path(data_subdir, "app.db"),
        data_subdir.join("app.db.rescue")
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
