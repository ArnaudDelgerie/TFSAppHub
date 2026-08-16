//! The Messenger worker, and the supervisor that keeps it alive.
//!
//! Ported from the station's `worker.rs` (CONTRACT.md §2/§6). The policy comes
//! across unchanged, down to the constants, because it is the app's guarantee
//! and not the host's: an app that declares `async_worker` gets a consumer on
//! its `async` queue, recycled on its own time and memory limits, respawned with
//! an exponential backoff, and given up on after five consecutive failed starts
//! with the user told once. A hub that supervised differently would make the
//! same manifest mean two things.
//!
//! Two differences, both mechanical rather than behavioural:
//!
//! - the worker's `async_worker` flag comes from the app's **manifest**, read at
//!   launch, where the station reads it back from what `build-app.sh` baked into
//!   `tauri.conf.json`. Same value, one bake fewer;
//! - `run_messenger_setup_transports` runs through the hub's own
//!   `php::Toolchain`, which is what keeps every line of an app's PHP on the
//!   bundled interpreter (see `php.rs`'s header on `PHP_BINARY`).
//!
//! **Every worker belongs to the sidecar's lifetime, not the window's.** A
//! second `open` of the same app attaches a window to the sidecar already
//! running and must not spawn a second set of consumers — which falls out of
//! the process model: the serving lock sends that second `open` down the
//! hand-off path, and the hand-off — not a lock — prevents it. Teardown stops
//! every worker before the server, and each worker's pid is one of the lines
//! after the server's in `sidecar.pid` (CONTRACT.md §6, plan 045), which is
//! what lets the next launch reap all of them if this process dies without
//! tearing anything down.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Child,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use tfsapp_core::{log, sidecar::command_with_env};

/// Provision the Doctrine `async` transport table (CONTRACT.md §2/§6).
///
/// Run once per launch, **before the server spawns**, and that order is
/// load-bearing: an app that declares `async_worker` without the Doctrine
/// Messenger bridge installed has to fail here, cleanly, with no sidecar yet to
/// tear down. Going through the same logged `bin/console` path as a lifecycle
/// command — under the alias `messenger-setup`, into `commands.log` — rather
/// than through a mechanism of its own, so its failure reads exactly like a
/// failing `pre-install` and is inspectable in the same file.
pub fn setup_transports(
    toolchain: &crate::php::Toolchain,
    app_dir: &Path,
    envs: &[(&str, String)],
    log_dir: &Path,
) -> Result<(), crate::php::PhpError> {
    toolchain.console_logged(
        app_dir,
        envs,
        "messenger:setup-transports --no-interaction",
        &log_dir.join("commands.log"),
        "messenger-setup",
    )
}

/// The `messenger:consume` argument vector for one declaration's transports,
/// in the order the manifest gave them (plan 045). Order is Symfony's own
/// priority mechanism — `Worker::run()` rescans from the first transport
/// after every envelope (`vendor/symfony/messenger/Worker.php`) — so a long
/// queued run cannot starve short interactive work on the same consumer, and
/// the hub itself never routes a message. A pure function so the vector is
/// testable without spawning anything.
fn consume_args(transports: &[String]) -> Vec<String> {
    let mut args = vec![
        "php-cli".to_string(),
        "bin/console".to_string(),
        "messenger:consume".to_string(),
    ];
    args.extend(transports.iter().cloned());
    args.push("--time-limit=3600".to_string());
    args.push("--memory-limit=256M".to_string());
    args
}

/// Spawn one Messenger worker, consuming `transports` in order.
///
/// `--time-limit`/`--memory-limit` make it recycle periodically, which is what a
/// long-lived PHP process needs; the supervisor below is what makes the recycle
/// invisible. No `-vvv`: the app's real logs go through Symfony's own logger
/// under `APP_LOG_DIR`, and duplicating them into `sidecar.log` would only make
/// the file useless for what it is actually for.
///
/// No `--env`/`--no-debug`: `bin/console` reads `APP_ENV`/`APP_DEBUG` straight
/// from `envs` when neither flag is given, exactly as the FrankenPHP server
/// started alongside it does — the same values, the same way, in every process.
/// Hardcoding `--env=prod` here (as the station's single-mode `worker.rs` did)
/// would override the injected value and win over it, so a dev session's
/// worker would run its prod kernel while its web server ran dev — silently
/// breaking CONTRACT.md §3's "every process the hub starts on the app's behalf
/// gets the same list" the moment `dev` gave `APP_ENV` a second value to carry.
pub fn spawn_worker(
    frankenphp: &Path,
    app_dir: &Path,
    envs: &[(&str, String)],
    log_dir: &Path,
    transports: &[String],
) -> std::io::Result<Child> {
    let mut command = command_with_env(frankenphp, envs);
    command.args(consume_args(transports));
    command.current_dir(app_dir);
    let (stdout, stderr) = log::sidecar_log_stdio(log_dir)?;
    command.stdout(stdout).stderr(stderr);
    tfsapp_core::process::set_own_process_group(&mut command);
    command.spawn()
}

/// Flatten `workers` into one entry per copy — a declaration with `count: 3`
/// becomes three identical slots — in declaration order, each copy's
/// transports before the next declaration's first (plan 045 step 4). A pure
/// function so the slot list a launch will spawn is assertable without
/// spawning anything.
pub fn flatten_worker_slots(workers: &[crate::manifest::WorkerDeclaration]) -> Vec<Vec<String>> {
    workers
        .iter()
        .flat_map(|declaration| {
            std::iter::repeat_n(declaration.transports.clone(), declaration.count as usize)
        })
        .collect()
}

/// A worker that lived at least this long is judged healthy: its exit is a
/// recycle (a time or memory limit reached, or a clean `SIGTERM`) rather than a
/// failed start, and it resets the failure count.
///
/// Judged by uptime and deliberately not by exit code: a recycle and a crash
/// `try_wait` identically, but only a crash happens fast.
pub const WORKER_MIN_HEALTHY_UPTIME: Duration = Duration::from_secs(10);

/// How many consecutive short-lived exits are tolerated before the worker is
/// given up on for the rest of this launch.
pub const WORKER_MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// The supervisor's per-exit decision, a pure function of what is already known
/// so the backoff and give-up policy is testable without a process to kill.
#[derive(Debug, PartialEq)]
pub enum SupervisorDecision {
    RestartAfter {
        delay: Duration,
        consecutive_failures: u32,
    },
    GiveUp {
        consecutive_failures: u32,
    },
}

/// A healthy exit always wins: it restarts immediately and clears the tally,
/// however many failures preceded it. A short-lived one backs off
/// `2^(failures - 1)` seconds until the tally reaches
/// [`WORKER_MAX_CONSECUTIVE_FAILURES`], which is itself the give-up condition —
/// so the delay computed for that count is never actually waited out.
pub fn supervisor_decision(consecutive_failures: u32, uptime: Duration) -> SupervisorDecision {
    if uptime >= WORKER_MIN_HEALTHY_UPTIME {
        return SupervisorDecision::RestartAfter {
            delay: Duration::ZERO,
            consecutive_failures: 0,
        };
    }
    if consecutive_failures >= WORKER_MAX_CONSECUTIVE_FAILURES {
        return SupervisorDecision::GiveUp {
            consecutive_failures,
        };
    }
    SupervisorDecision::RestartAfter {
        delay: Duration::from_secs(1 << (consecutive_failures - 1)),
        consecutive_failures,
    }
}

/// Sleep `delay`, waking every 200 ms to check for shutdown.
///
/// `false` means shutdown was observed and the caller must not respawn. Without
/// the slicing, a shutdown arriving during the longest backoff would wait out
/// the whole eight seconds and then start a worker nobody wants.
pub fn sleep_backoff_or_shutdown(delay: Duration, shutting_down: &AtomicBool) -> bool {
    let slice = Duration::from_millis(200);
    let deadline = Instant::now() + delay;
    loop {
        if shutting_down.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return !shutting_down.load(Ordering::SeqCst);
        }
        thread::sleep(slice.min(remaining));
    }
}

/// The shared record behind every `sidecar.pid` write once a launch has more
/// than one worker slot (plan 045 step 4).
///
/// A supervisor that rewrote the file from its own knowledge alone would
/// erase its siblings' pids the moment it recycled or gave up, and the next
/// launch would not reap them — so every write instead goes through this
/// table, which holds every slot's current pid and always rewrites the whole
/// file: the server pid, then one line per still-live worker, in slot order.
pub struct WorkerPidTable {
    server_pid: u32,
    pid_file: PathBuf,
    pids: Mutex<Vec<Option<u32>>>,
}

impl WorkerPidTable {
    pub fn new(server_pid: u32, pid_file: PathBuf, slots: usize) -> Self {
        Self {
            server_pid,
            pid_file,
            pids: Mutex::new(vec![None; slots]),
        }
    }

    /// Record `slot`'s pid (or clear it, with `None`, on a give-up) and
    /// rewrite the file whole.
    pub fn set(&self, slot: usize, pid: Option<u32>) -> std::io::Result<()> {
        let mut pids = self.pids.lock().expect("worker pid table mutex");
        pids[slot] = pid;
        let mut contents = format!("{}\n", self.server_pid);
        for pid in pids.iter().flatten() {
            contents.push_str(&format!("{pid}\n"));
        }
        fs::write(&self.pid_file, contents)
    }
}

/// Everything the supervisor loop needs, grouped so its call site builds one
/// named-field value rather than a positional list where two swapped `PathBuf`s
/// would compile in silence. Every field is owned: they all cross a
/// `thread::spawn`.
pub struct WorkerSupervisorConfig {
    pub worker: Arc<Mutex<Option<Child>>>,
    pub shutting_down: Arc<AtomicBool>,
    pub frankenphp: PathBuf,
    pub app_dir: PathBuf,
    pub envs: Vec<(&'static str, String)>,
    pub transports: Vec<String>,
    /// The shared table behind this slot's `sidecar.pid` line, and this
    /// slot's index into it.
    pub pid_table: Arc<WorkerPidTable>,
    pub slot: usize,
    pub log_dir: PathBuf,
    pub worker_spawned_at: Instant,
    pub app: tauri::AppHandle,
    /// Shared across every slot's supervisor so at most one give-up ever
    /// shows the "background processing unavailable" dialog for one launch.
    pub dialog_shown: Arc<AtomicBool>,
}

/// The result of offering a freshly spawned worker to the shared sidecar slot.
///
/// Until the slot accepts it, the supervisor owns the child and must reap it.
/// Keeping that ownership in the return value makes a failed hand-off unable to
/// silently turn into an orphan.
enum RespawnArbitration {
    Adopted { worker_pid: u32 },
    CancelledByShutdown(Child),
    CannotAdopt(Child),
}

/// Put a respawned worker under sidecar ownership unless teardown won the race.
///
/// `stop()` sets `shutting_down` before taking this same mutex. Consequently,
/// while the mutex is held, either teardown will later find the child in the
/// slot, or this re-check observes shutdown and `Option::take` gives the child
/// back to the supervisor as its sole killer. The mutex and `take` therefore
/// serialize the hand-off and make exactly one side responsible for reaping it.
fn arbitrate_respawn(
    worker: &Mutex<Option<Child>>,
    shutting_down: &AtomicBool,
    child: Child,
) -> RespawnArbitration {
    let Ok(mut guard) = worker.lock() else {
        return RespawnArbitration::CannotAdopt(child);
    };

    let worker_pid = child.id();
    *guard = Some(child);
    if shutting_down.load(Ordering::SeqCst) {
        return RespawnArbitration::CancelledByShutdown(
            guard.take().expect("worker was inserted immediately above"),
        );
    }

    RespawnArbitration::Adopted { worker_pid }
}

/// Watch the worker and respawn it under [`supervisor_decision`]'s policy.
///
/// Without this the worker would stop consuming after its first time or memory
/// limit and the app's async jobs would silently never complete — silence being
/// the whole problem, which is also why giving up is announced.
pub fn spawn_worker_supervisor(config: WorkerSupervisorConfig) {
    let WorkerSupervisorConfig {
        worker,
        shutting_down,
        frankenphp,
        app_dir,
        envs,
        transports,
        pid_table,
        slot,
        log_dir,
        worker_spawned_at,
        app,
        dialog_shown,
    } = config;
    let sidecar_log = log_dir.join("sidecar.log");

    thread::spawn(move || {
        let mut spawned_at = worker_spawned_at;
        let mut consecutive_failures: u32 = 0;

        'watch: loop {
            if shutting_down.load(Ordering::SeqCst) {
                return;
            }

            // `try_wait`, never `wait`: holding the lock while blocking would
            // stop teardown from ever taking it to kill the worker.
            let status = {
                let Ok(mut guard) = worker.lock() else {
                    return;
                };
                match guard.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(status) => status,
                        Err(error) => {
                            // An OS-level polling error is not evidence the
                            // worker died, so it stays alive here — but the
                            // oddity is made visible rather than spun on
                            // forever in silence.
                            eprintln!(
                                "tfsapp-hub: cannot poll the Messenger worker pid {}: {error} \
                                 (treating it as still running)",
                                child.id()
                            );
                            None
                        }
                    },
                    None => return,
                }
            };

            let Some(status) = status else {
                thread::sleep(Duration::from_millis(1000));
                continue 'watch;
            };

            if shutting_down.load(Ordering::SeqCst) {
                return;
            }

            let mut uptime = spawned_at.elapsed();
            let exit_line = format!("Messenger worker exited ({status}) after {uptime:.1?}");
            println!("{exit_line}");
            log::append_log(&sidecar_log, &exit_line);

            // Looping here rather than returning out of the thread: a failed
            // *respawn* is itself a zero-uptime failed start, and it earns the
            // same backoff as a worker that started and died fast.
            loop {
                let (delay, attempt) = match supervisor_decision(consecutive_failures + 1, uptime) {
                    SupervisorDecision::GiveUp {
                        consecutive_failures,
                    } => {
                        let line = format!(
                            "Messenger worker: giving up after {consecutive_failures} \
                             consecutive failed starts"
                        );
                        eprintln!("{line}");
                        log::append_log(&sidecar_log, &line);
                        // Only this slot's pid leaves the table; a sibling
                        // still running keeps its own line.
                        let _ = pid_table.set(slot, None);
                        if claim_dialog(&dialog_shown) {
                            gave_up_dialog(&app, &sidecar_log, &transports);
                        }
                        return;
                    }
                    SupervisorDecision::RestartAfter {
                        delay,
                        consecutive_failures,
                    } => (delay, consecutive_failures),
                };
                consecutive_failures = attempt;

                // `attempt == 0` is the healthy recycle, where uptime alone
                // decided the restart. Logging "attempt 0" there would read as
                // alarming for something entirely routine.
                if attempt > 0 {
                    let line = format!(
                        "Messenger worker: restarting after {delay:.1?} backoff (attempt {attempt})"
                    );
                    println!("{line}");
                    log::append_log(&sidecar_log, &line);
                }

                if !sleep_backoff_or_shutdown(delay, &shutting_down) {
                    return;
                }

                match spawn_worker(&frankenphp, &app_dir, &envs, &log_dir, &transports) {
                    Ok(child) => match arbitrate_respawn(&worker, &shutting_down, child) {
                        RespawnArbitration::Adopted { worker_pid } => {
                            // The child is in the slot before this durable
                            // record exists, so teardown can always take it.
                            // If shutdown begins just after the arbitration,
                            // clearing this slot again below covers the race
                            // where this write lands after teardown already
                            // read the table for its own last rewrite.
                            let _ = pid_table.set(slot, Some(worker_pid));
                            if shutting_down.load(Ordering::SeqCst) {
                                let _ = pid_table.set(slot, None);
                                return;
                            }

                            let line = format!("Restarted Messenger worker pid {worker_pid}");
                            println!("{line}");
                            log::append_log(&sidecar_log, &line);
                            spawned_at = Instant::now();
                            continue 'watch;
                        }
                        RespawnArbitration::CancelledByShutdown(mut child) => {
                            let line = "Messenger worker respawn cancelled by shutdown";
                            println!("{line}");
                            log::append_log(&sidecar_log, line);
                            tfsapp_core::process::terminate(child.id());
                            let _ = child.wait();
                            return;
                        }
                        RespawnArbitration::CannotAdopt(mut child) => {
                            let line = "Cannot supervise respawned Messenger worker; stopping it";
                            eprintln!("{line}");
                            log::append_log(&sidecar_log, line);
                            tfsapp_core::process::terminate(child.id());
                            let _ = child.wait();
                            return;
                        }
                    },
                    Err(error) => {
                        let line = format!("Cannot restart the Messenger worker: {error}");
                        eprintln!("{line}");
                        log::append_log(&sidecar_log, &line);
                        uptime = Duration::ZERO;
                    }
                }
            }
        }
    });
}

/// Claim the right to show the give-up dialog: `true` for the first slot to
/// call this on a given `dialog_shown`, `false` for every one after it. A
/// launch with several workers can only give up on more than one, and the
/// user is told once, not once per slot — kept as its own pure-ish function
/// (its only side effect is the flag itself) so the latch is testable without
/// a `tauri::AppHandle`.
fn claim_dialog(dialog_shown: &AtomicBool) -> bool {
    dialog_shown
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Tell the user once that background processing has stopped, naming the
/// transports the give-up leaves unconsumed.
///
/// Non-blocking, unlike a startup failure's dialog: the server is still healthy
/// and the window still usable, so nothing here should block either this thread
/// or the user's work. Relaunching is the only retry, which is what the message
/// has to convey without saying "restart" as if it were a bug report.
fn gave_up_dialog(app: &tauri::AppHandle, sidecar_log: &Path, transports: &[String]) {
    use tauri_plugin_dialog::DialogExt;

    app.dialog()
        .message(format!(
            "The background worker consuming {} could not stay running and has been stopped \
             for this session. The app itself is unaffected, but jobs that rely on it will not \
             complete until you open it again. See {} for details.",
            transports.join(", "),
            sidecar_log.display()
        ))
        .title("Background processing unavailable")
        .kind(tauri_plugin_dialog::MessageDialogKind::Warning)
        .show(|_| {});
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
