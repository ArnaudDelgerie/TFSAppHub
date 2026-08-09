use std::{
    fs,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use super::{
    acquire_launch_locks, decide_launch, lifecycle_decision, prepare_dev_launch, probe_run_lock,
    read_data_version, serving_lock_path, write_data_version, LaunchDecision, LaunchLockError,
    LifecycleDecisionError, LifecycleError, LifecycleEvent, RunLockHeld,
};

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
fn probe_run_lock_free_when_nothing_holds_it() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");

    let held = probe_run_lock(data_dir.path()).expect("no I/O error");

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

    let held = probe_run_lock(data_dir.path()).expect("no I/O error");

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

    let held = probe_run_lock(data_dir.path()).expect("no I/O error");

    assert_eq!(held, RunLockHeld::Held { alias: None });
}
