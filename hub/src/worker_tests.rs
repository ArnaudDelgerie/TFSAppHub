use std::{
    process::Command,
    sync::{atomic::AtomicBool, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use super::{
    arbitrate_respawn, sleep_backoff_or_shutdown, supervisor_decision, RespawnArbitration,
    SupervisorDecision, WORKER_MIN_HEALTHY_UPTIME,
};

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
