use std::{
    path::Path,
    process::{Child, ExitStatus},
    thread,
    time::{Duration, Instant},
};

/// `wait_for_healthz`'s per-poll decision (plan 031), factored out as a pure
/// function of what's already known so it's unit-testable without a real
/// child process or HTTP call (see `health_tests.rs`). A child that has
/// already exited always wins — checked first, so a crash is never
/// mis-diagnosed as "still starting" and blamed on the health endpoint —
/// then a passed deadline, then keep polling.
#[derive(Debug, PartialEq)]
pub enum HealthzPollOutcome {
    ChildExited,
    DeadlinePassed,
    KeepWaiting,
}

pub fn healthz_poll_outcome(child_exited: bool, deadline_passed: bool) -> HealthzPollOutcome {
    if child_exited {
        HealthzPollOutcome::ChildExited
    } else if deadline_passed {
        HealthzPollOutcome::DeadlinePassed
    } else {
        HealthzPollOutcome::KeepWaiting
    }
}

/// The message for a sidecar that died before ever answering `/healthz`:
/// names its exit status (or that it was killed by a signal) and, in
/// packaged mode, `sidecar.log` — the sidecar's own captured stdout/stderr
/// (see `sidecar_log_stdio`) — so the cause is diagnosable instead of just
/// "backend did not become healthy". `sidecar_log` is `None` in dev, which
/// has no such file (it inherits the developer's terminal instead).
pub fn dead_sidecar_message(status: ExitStatus, sidecar_log: Option<&Path>) -> String {
    let detail = match status.code() {
        Some(code) => format!("exited with status {code}"),
        None => "was killed by a signal".to_string(),
    };
    match sidecar_log {
        Some(log_file) => format!(
            "The backend process {detail} before becoming healthy. See {} for its output.",
            log_file.display()
        ),
        None => format!("The backend process {detail} before becoming healthy."),
    }
}

/// The message for a sidecar that is merely slow: still running once the
/// deadline passes. Names the URL, the timeout and — packaged mode only,
/// same rationale as `dead_sidecar_message` — `sidecar.log`.
pub fn healthz_timeout_message(url: &str, timeout: Duration, sidecar_log: Option<&Path>) -> String {
    match sidecar_log {
        Some(log_file) => format!(
            "Backend did not become healthy at {url} within {}s. See {} for its output.",
            timeout.as_secs(),
            log_file.display()
        ),
        None => format!(
            "Backend did not become healthy at {url} within {}s.",
            timeout.as_secs()
        ),
    }
}

/// Poll `<base_url>/healthz` until it returns 200. Plan 031: `child` — the
/// spawned FrankenPHP server — is polled with `try_wait()` on every
/// iteration so a sidecar that has already died aborts the wait immediately
/// instead of
/// burning the whole timeout and then blaming the health endpoint for what
/// was actually a dead process; the timeout itself is 60s (was 20s), long
/// enough for a bigger project's from-scratch compiled-container rebuild
/// (every packaged launch wipes and rebuilds `cache/`/`build/` by design —
/// see `resolve_packaged_env`), not tunable, since the real fix for a
/// mis-diagnosed slow start is detecting a dead sidecar, not a bigger
/// number. `sidecar_log` is threaded straight into both failure messages —
/// see `dead_sidecar_message`/`healthz_timeout_message`.
pub fn wait_for_healthz(
    base_url: &str,
    child: &mut Child,
    sidecar_log: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{base_url}/healthz");
    let timeout = Duration::from_secs(60);
    let deadline = Instant::now() + timeout;
    loop {
        let exit_status = child.try_wait().ok().flatten();
        match healthz_poll_outcome(exit_status.is_some(), Instant::now() >= deadline) {
            HealthzPollOutcome::ChildExited => {
                let status = exit_status.expect("ChildExited implies exit_status is Some");
                return Err(dead_sidecar_message(status, sidecar_log).into());
            }
            HealthzPollOutcome::DeadlinePassed => {
                return Err(healthz_timeout_message(&url, timeout, sidecar_log).into());
            }
            HealthzPollOutcome::KeepWaiting => {
                if let Ok(response) = ureq::get(&url).call() {
                    if response.status() == 200 {
                        return Ok(());
                    }
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
