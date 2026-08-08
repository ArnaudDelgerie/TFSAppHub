use std::{
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::{Duration, Instant},
};

use super::{
    sleep_backoff_or_shutdown, supervisor_decision, SupervisorDecision, WORKER_MIN_HEALTHY_UPTIME,
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
