use super::*;

// --- healthz_poll_outcome ---------------------------------------------

#[test]
fn healthz_poll_outcome_child_exited_wins_over_deadline_passed() {
    assert_eq!(
        healthz_poll_outcome(true, true),
        HealthzPollOutcome::ChildExited
    );
}

#[test]
fn healthz_poll_outcome_child_exited_before_deadline() {
    assert_eq!(
        healthz_poll_outcome(true, false),
        HealthzPollOutcome::ChildExited
    );
}

#[test]
fn healthz_poll_outcome_deadline_passed_when_child_still_running() {
    assert_eq!(
        healthz_poll_outcome(false, true),
        HealthzPollOutcome::DeadlinePassed
    );
}

#[test]
fn healthz_poll_outcome_keep_waiting_before_deadline_with_a_live_child() {
    assert_eq!(
        healthz_poll_outcome(false, false),
        HealthzPollOutcome::KeepWaiting
    );
}

// --- dead_sidecar_message / healthz_timeout_message ------------------------

use std::os::unix::process::ExitStatusExt;

#[test]
fn dead_sidecar_message_names_exit_code_and_log_in_packaged_mode() {
    let status = ExitStatus::from_raw(3 << 8);
    let log_file = Path::new("/data/log/sidecar.log");

    let message = dead_sidecar_message(status, Some(log_file));

    assert!(message.contains("exited with status 3"));
    assert!(message.contains("/data/log/sidecar.log"));
}

#[test]
fn dead_sidecar_message_omits_log_reference_in_dev_mode() {
    let status = ExitStatus::from_raw(3 << 8);

    let message = dead_sidecar_message(status, None);

    assert!(message.contains("exited with status 3"));
    assert!(!message.contains("sidecar.log"));
}

#[test]
fn dead_sidecar_message_names_a_signal_kill_without_a_status_code() {
    // Raw wait() status for "killed by SIGKILL": no WIFEXITED bit set, so
    // `.code()` is None.
    let status = ExitStatus::from_raw(9);

    let message = dead_sidecar_message(status, None);

    assert!(message.contains("killed by a signal"));
}

#[test]
fn healthz_timeout_message_names_url_timeout_and_log_in_packaged_mode() {
    let log_file = Path::new("/data/log/sidecar.log");

    let message = healthz_timeout_message(
        "http://127.0.0.1:8080/healthz",
        Duration::from_secs(60),
        Some(log_file),
    );

    assert!(message.contains("http://127.0.0.1:8080/healthz"));
    assert!(message.contains("60s"));
    assert!(message.contains("/data/log/sidecar.log"));
}

#[test]
fn healthz_timeout_message_omits_log_reference_in_dev_mode() {
    let message = healthz_timeout_message(
        "http://127.0.0.1:8080/healthz",
        Duration::from_secs(60),
        None,
    );

    assert!(message.contains("http://127.0.0.1:8080/healthz"));
    assert!(!message.contains("sidecar.log"));
}
