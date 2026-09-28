use std::{
    fs,
    path::Path,
    process::Command,
    sync::{atomic::AtomicBool, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use super::{
    arbitrate_give_up, arbitrate_respawn, claim_dialog, consume_args, flatten_worker_slots,
    record_worker_pid, sleep_backoff_or_shutdown, spawn_worker, supervisor_decision, worker_log,
    RespawnArbitration, SupervisorDecision, WorkerPidTable, WORKER_MIN_HEALTHY_UPTIME,
};
use crate::manifest::WorkerDeclaration;

// --- messenger:consume argument vector (plan 045) ---------------------------

#[test]
fn one_transport_is_appended_after_messenger_consume() {
    assert_eq!(
        consume_args(&["async".to_string()]),
        vec![
            "php-cli",
            "bin/console",
            "messenger:consume",
            "async",
            "--time-limit=3600",
            "--memory-limit=256M",
        ]
    );
}

#[test]
fn several_transports_are_appended_in_declared_order() {
    // Order is the priority: `Worker::run()` rescans from the first transport
    // after every envelope, so declaration order is what keeps a long queued
    // run from starving short interactive work on the same consumer.
    assert_eq!(
        consume_args(&[
            "courant".to_string(),
            "planifie".to_string(),
            "fond".to_string(),
        ]),
        vec![
            "php-cli",
            "bin/console",
            "messenger:consume",
            "courant",
            "planifie",
            "fond",
            "--time-limit=3600",
            "--memory-limit=256M",
        ]
    );
}

// --- flattening declarations into slots (plan 045 step 4) -------------------

#[test]
fn flattening_expands_each_declaration_by_its_count() {
    let workers = vec![
        WorkerDeclaration {
            transports: vec!["courant".to_string()],
            count: 2,
        },
        WorkerDeclaration {
            transports: vec!["fond".to_string()],
            count: 1,
        },
    ];

    assert_eq!(
        flatten_worker_slots(&workers),
        vec![
            vec!["courant".to_string()],
            vec!["courant".to_string()],
            vec!["fond".to_string()],
        ]
    );
}

#[test]
fn flattening_no_declarations_is_no_slots() {
    assert!(flatten_worker_slots(&[]).is_empty());
}

// --- per-slot log files (plan 046) -------------------------------------------

#[test]
fn worker_log_names_the_slot_one_based() {
    let dir = tempfile::tempdir().expect("a temp dir");
    assert_eq!(worker_log(dir.path(), 0), dir.path().join("worker-1.log"));
    assert_eq!(worker_log(dir.path(), 2), dir.path().join("worker-3.log"));
}

#[test]
fn spawn_worker_writes_to_its_own_slot_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut child = spawn_worker(
        Path::new("/bin/echo"),
        dir.path(),
        &[],
        dir.path(),
        &["async".to_string()],
        0,
    )
    .expect("spawn the throwaway worker");
    child.wait().expect("the throwaway worker exits");

    let contents = fs::read_to_string(worker_log(dir.path(), 0)).expect("read worker-1.log");
    assert!(contents.contains("messenger:consume"));
}

#[test]
fn two_slots_never_write_to_one_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut first = spawn_worker(
        Path::new("/bin/echo"),
        dir.path(),
        &[],
        dir.path(),
        &["async".to_string()],
        0,
    )
    .expect("spawn slot 0's throwaway worker");
    first.wait().expect("slot 0 exits");

    let mut second = spawn_worker(
        Path::new("/bin/echo"),
        dir.path(),
        &[],
        dir.path(),
        &["scheduler_default".to_string()],
        1,
    )
    .expect("spawn slot 1's throwaway worker");
    second.wait().expect("slot 1 exits");

    let first_log = fs::read_to_string(worker_log(dir.path(), 0)).expect("read worker-1.log");
    let second_log = fs::read_to_string(worker_log(dir.path(), 1)).expect("read worker-2.log");
    assert!(first_log.contains("async"));
    assert!(!first_log.contains("scheduler_default"));
    assert!(second_log.contains("scheduler_default"));
    assert!(!second_log.contains("async"));
}

// --- the shared pid table -----------------------------------------------------

#[test]
fn the_pid_table_writes_the_server_then_every_live_worker_in_slot_order() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file.clone(), 3);

    // Set out of slot order: the file must still read back in slot order.
    table.set(0, Some(201)).expect("write slot 0");
    table.set(2, Some(203)).expect("write slot 2");
    table.set(1, Some(202)).expect("write slot 1");

    assert_eq!(
        fs::read_to_string(&pid_file).expect("read the pid file"),
        "100\n201\n202\n203\n"
    );
    assert!(!dir.path().join("sidecar.pid.tmp").exists());
}

#[test]
fn failed_pid_table_rewrite_preserves_the_previous_full_list() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file.clone(), 2);
    table.set(0, Some(201)).unwrap();
    table.set(1, Some(202)).unwrap();
    fs::create_dir(dir.path().join("sidecar.pid.tmp")).unwrap();

    assert!(table.set(1, Some(303)).is_err());
    assert_eq!(fs::read_to_string(&pid_file).unwrap(), "100\n201\n202\n");
}

#[test]
fn failed_supervisor_pid_record_is_written_to_its_worker_log() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file, 1);
    fs::create_dir(dir.path().join("sidecar.pid.tmp")).unwrap();
    let log_path = worker_log(dir.path(), 0);

    record_worker_pid(&table, 0, Some(201), &log_path);

    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("Messenger worker 1"));
    assert!(log.contains("sidecar.pid"));
}

#[test]
fn respawning_a_middle_slot_leaves_its_siblings_pids_untouched() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file.clone(), 3);
    table.set(0, Some(201)).expect("write slot 0");
    table.set(1, Some(202)).expect("write slot 1");
    table.set(2, Some(203)).expect("write slot 2");

    table.set(1, Some(9202)).expect("respawn slot 1");

    assert_eq!(
        fs::read_to_string(&pid_file).expect("read the pid file"),
        "100\n201\n9202\n203\n"
    );
}

#[test]
fn giving_up_on_one_slot_clears_only_that_pid() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file.clone(), 3);
    table.set(0, Some(201)).expect("write slot 0");
    table.set(1, Some(202)).expect("write slot 1");
    table.set(2, Some(203)).expect("write slot 2");

    table.set(1, None).expect("give up on slot 1");

    assert_eq!(
        fs::read_to_string(&pid_file).expect("read the pid file"),
        "100\n201\n203\n"
    );
}

#[test]
fn a_give_up_racing_teardown_leaves_no_stale_pid_and_skips_the_dialog() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file.clone(), 1);
    table
        .set(0, Some(201))
        .expect("record the failing worker's last pid");

    // Teardown removes the file first ...
    fs::remove_file(&pid_file).expect("simulate teardown's own removal");
    let shutting_down = AtomicBool::new(true);

    // ... and only then does the give-up's write land.
    let show_dialog =
        arbitrate_give_up(&table, 0, &shutting_down, &dir.path().join("worker-1.log"));

    assert!(
        !show_dialog,
        "a give-up racing teardown must not raise a dialog"
    );
    assert_eq!(
        fs::read_to_string(&pid_file).expect("read the resurrected pid file"),
        "100\n",
        "the late write must still leave the file without this slot's pid"
    );
}

#[test]
fn a_give_up_without_teardown_still_shows_the_dialog() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let pid_file = dir.path().join("sidecar.pid");
    let table = WorkerPidTable::new(100, pid_file.clone(), 1);
    table
        .set(0, Some(201))
        .expect("record the failing worker's last pid");
    let shutting_down = AtomicBool::new(false);

    assert!(arbitrate_give_up(
        &table,
        0,
        &shutting_down,
        &dir.path().join("worker-1.log")
    ));
    assert_eq!(
        fs::read_to_string(&pid_file).expect("read the pid file"),
        "100\n"
    );
}

// --- the give-up dialog latch -------------------------------------------------

#[test]
fn the_dialog_latch_admits_only_the_first_claim() {
    let dialog_shown = AtomicBool::new(false);
    assert!(
        claim_dialog(&dialog_shown),
        "the first give-up must claim the dialog"
    );
    assert!(
        !claim_dialog(&dialog_shown),
        "a second give-up must not claim it again"
    );
}

// The station's own table, ported unchanged. The policy is the app's guarantee
// (CONTRACT.md §2/§6) rather than the host's, so the two hosts have to decide
// identically or the same manifest would mean two things.

#[test]
fn a_long_lived_exit_is_a_recycle_and_restarts_at_once() {
    assert_eq!(
        supervisor_decision(3, WORKER_MIN_HEALTHY_UPTIME),
        SupervisorDecision::RestartAfter {
            delay: Duration::ZERO,
            consecutive_failures: 0,
        }
    );
}

#[test]
fn successive_short_exits_back_off_by_doubling() {
    let short = Duration::from_secs(1);

    for (failures, seconds) in [(1, 1), (2, 2), (3, 4), (4, 8)] {
        assert_eq!(
            supervisor_decision(failures, short),
            SupervisorDecision::RestartAfter {
                delay: Duration::from_secs(seconds),
                consecutive_failures: failures,
            }
        );
    }
}

#[test]
fn the_fifth_consecutive_short_exit_gives_up() {
    // Reaching the cap *is* the give-up condition, so the delay that would have
    // been computed for it is never waited out.
    assert_eq!(
        supervisor_decision(5, Duration::from_secs(1)),
        SupervisorDecision::GiveUp {
            consecutive_failures: 5
        }
    );
}

#[test]
fn one_healthy_run_clears_every_failure_before_it() {
    // Judged by uptime alone: a worker that finally stayed up is working, and
    // the count of how badly it started is no longer evidence of anything.
    assert_eq!(
        supervisor_decision(4, WORKER_MIN_HEALTHY_UPTIME + Duration::from_secs(60)),
        SupervisorDecision::RestartAfter {
            delay: Duration::ZERO,
            consecutive_failures: 0,
        }
    );
}

#[test]
fn a_backoff_observes_a_shutdown_instead_of_waiting_it_out() {
    let shutting_down = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutting_down);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let started = Instant::now();
    // The longest backoff the policy ever waits. Without the slicing, a
    // shutdown arriving now would be answered eight seconds later, by spawning
    // a worker nobody wants.
    let respawn = sleep_backoff_or_shutdown(Duration::from_secs(8), &shutting_down);

    assert!(!respawn, "shutdown must cancel the respawn");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown must be observed during the backoff, not after it"
    );
}

#[test]
fn a_backoff_that_no_one_interrupts_runs_its_course() {
    let shutting_down = Arc::new(AtomicBool::new(false));

    assert!(sleep_backoff_or_shutdown(
        Duration::from_millis(300),
        &shutting_down
    ));
}

#[test]
fn a_respawn_is_adopted_while_teardown_is_not_requested() {
    let worker = Mutex::new(None);
    let shutting_down = AtomicBool::new(false);
    let mut command = Command::new("sleep");
    command.arg("60");
    tfsapp_core::process::set_own_process_group(&mut command);
    let child = command.spawn().expect("spawn throwaway worker");
    let worker_pid = child.id();

    assert!(matches!(
        arbitrate_respawn(&worker, &shutting_down, child),
        RespawnArbitration::Adopted { worker_pid: adopted_pid } if adopted_pid == worker_pid
    ));
    let mut child = worker
        .lock()
        .expect("worker mutex")
        .take()
        .expect("worker is adopted");
    tfsapp_core::process::terminate(child.id());
    let _ = child.wait();
}

#[test]
fn a_respawn_cancelled_by_teardown_remains_the_supervisors_child() {
    let worker = Mutex::new(None);
    let shutting_down = AtomicBool::new(true);
    let mut command = Command::new("sleep");
    command.arg("60");
    tfsapp_core::process::set_own_process_group(&mut command);
    let child = command.spawn().expect("spawn throwaway worker");
    let worker_pid = child.id();

    let RespawnArbitration::CancelledByShutdown(mut child) =
        arbitrate_respawn(&worker, &shutting_down, child)
    else {
        panic!("shutdown must leave the child with the supervisor");
    };
    assert!(worker.lock().expect("worker mutex").is_none());
    tfsapp_core::process::terminate(child.id());
    let _ = child.wait();
    assert!(
        !tfsapp_core::process::process_exists(worker_pid),
        "the supervisor must reap the cancelled respawn"
    );
}
