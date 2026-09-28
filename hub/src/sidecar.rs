//! Starting one app's FrankenPHP, and owning it until the window closes.
//!
//! The station's `start_packaged_sidecar`, with its `PackagedEnv` replaced by
//! the hub's own `app_env::AppEnvironment` and its baked resources replaced by
//! resolved ones. What it spawns, in what order, and what it writes down is the
//! same, because all three are CONTRACT.md §6.
//!
//! **The Caddyfile is written here, per app, per launch.** The station bundles
//! it as an AppImage resource; the hub has no per-app build step to place one
//! with, so it carries the file in its own binary (`include_str!`) and writes it
//! into the app's data dir on the way up. Rewritten every launch rather than
//! created once, for the same reason the `php` shim is (see `php.rs`): a hub
//! self-update can change what the file should say, and a stale one on disk
//! would be indistinguishable from a current one.
//!
//! **Order is load-bearing and is the station's.** `messenger:setup-transports`
//! runs first, before anything is spawned, so an app that declares a worker
//! without the Doctrine Messenger bridge fails with no sidecar to tear down.
//! Then the server. Then every worker (plan 045: one slot per declaration,
//! flattened by its copy count), each pid becoming its own line of
//! `sidecar.pid` after the server's (§6) so the next launch can reap all of
//! them if this process never gets to.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Child,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use tfsapp_core::{
    log,
    sidecar::{command_with_env, path_to_string},
};

use crate::{app_env::AppEnvironment, php::Toolchain, worker};

/// The station's `Caddyfile.desktop`, carried in the binary — see the module
/// header, and `hub/Caddyfile.desktop` for what it says and why it is a copy.
const CADDYFILE: &str = include_str!("../Caddyfile.desktop");

/// The one FrankenPHP this process owns, and everything needed to stop it.
///
/// Dropping it stops it. That is not a convenience: it is what makes a panic
/// anywhere in this process leave no orphan behind, and it is the same
/// arrangement the station relies on.
pub struct Sidecar {
    pub server: Option<Child>,
    /// One slot per declared worker (plan 045 step 4), flattened from
    /// `manifest.workers` by each declaration's copy count. Each slot is
    /// behind its own lock because its own supervisor thread replaces it on
    /// every recycle, independently of its siblings.
    pub workers: Vec<Arc<Mutex<Option<Child>>>>,
    /// Tells the supervisor to stop respawning. Set **first** in [`Sidecar::stop`],
    /// before the worker is killed, so the supervisor can never race a teardown
    /// with a respawn.
    pub shutting_down: Arc<AtomicBool>,
    pub pid_file: PathBuf,
    /// The liveness lock (CONTRACT.md §6), acquired before `Builder` by
    /// `lifecycle::prepare_launch` and moved in here. **Never dropped by any
    /// code in this struct** — see [`Sidecar::stop`] for why: the operating
    /// system releasing it at process death is the only release that is
    /// genuinely simultaneous with the bus names going away. So this field is
    /// genuinely never read after construction, which is the point rather
    /// than an oversight — held purely so its lifetime is this struct's.
    #[allow(dead_code)]
    pub lock: Option<fs::File>,
    /// The serving lock ([`crate::lifecycle::serving_lock_path`]), acquired
    /// alongside `lock` by the same launch and moved in here by the same
    /// route, for the same reason: one place decides what this process
    /// claims. Unlike `lock` it *is* released early, by
    /// `lifecycle::stop_sidecar_and_exit` — taking it out of this struct
    /// before `stop` runs — because answering "will hand you a window" has to
    /// stop the instant teardown begins, not once the process actually exits.
    pub serving: Option<fs::File>,
}

impl Sidecar {
    /// Stop everything, in the one order that leaves nothing behind.
    ///
    /// Every worker before the server: a worker talks to the app's database
    /// through the same files the server holds open, and stopping the server
    /// first would leave a consumer running against a backend that has gone.
    ///
    /// **It assumes this process's webview windows are already gone** — an
    /// ordering constraint `lifecycle::stop_sidecar_and_exit` owns and this
    /// function cannot check (plan 014). The server's stop is graceful: Caddy
    /// drains its connections, the always-mounted Mercure hub makes one of them
    /// a stream, and a stream never drains — so called with a live webview
    /// still attached, the SIGTERM below is ignored for as long as that window
    /// exists and the server dies by SIGKILL every time. Every caller reaches
    /// this through `stop_sidecar`, and the ordinary path
    /// (`stop_sidecar_and_exit`) destroys the windows first.
    ///
    /// `fatal_post_setup_error` is the one caller that cannot: it still has a
    /// dialog to show, and it needs a window to show it over. It pays Caddy's
    /// grace period for that, which is bounded (`Caddyfile.desktop`,
    /// `CONTRACT.md` §4) and is one of the reasons that bound exists.
    pub fn stop(&mut self) {
        // Before the kill, never after: a supervisor that learns of the
        // shutdown only afterwards respawns the worker we just stopped.
        self.shutting_down.store(true, Ordering::SeqCst);

        for worker in &self.workers {
            if let Ok(mut worker) = worker.lock() {
                if let Some(mut child) = worker.take() {
                    if worker_needs_termination(&mut child) {
                        println!("Stopping the Messenger worker pid {}", child.id());
                        tfsapp_core::process::terminate(child.id());
                        let _ = child.wait();
                    }
                }
            }
        }
        if let Some(mut child) = self.server.take() {
            println!("Stopping FrankenPHP pid {}", child.id());
            tfsapp_core::process::terminate(child.id());
            let _ = child.wait();
        }

        let _ = fs::remove_file(&self.pid_file);

        // No `self.lock.take()` here — deliberately. Every way this process
        // ends, ordinary or crashed, bottoms out in `std::process::exit`
        // (Tauri's own `run` loop does it after `app.exit()`, and the fatal
        // paths call it directly), which does not run the destructors of
        // what is still on the heap. An explicit drop here would therefore be
        // worthless on exactly the paths that matter. The liveness lock is
        // released the one way that is real regardless of how this process
        // ends: the OS closing every fd when it dies. See the field's own
        // doc comment.
    }
}

/// Whether teardown still owns a live worker it must signal.
///
/// The supervisor may already have reaped the child while it remains in the
/// shared slot during its backoff. `Child` caches that exit status, so a later
/// `try_wait` returns `Ok(Some(_))`; signalling its numeric pid then could hit
/// an unrelated process after pid reuse. A polling error remains conservative:
/// an unpollable child is treated as live, matching the supervisor's posture.
fn worker_needs_termination(child: &mut Child) -> bool {
    !matches!(child.try_wait(), Ok(Some(_)))
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The running backend, as everything downstream needs to see it.
///
/// Managed as Tauri state so the single-instance relaunch closure can open a
/// second window on the same backend long after the launch that resolved it has
/// returned.
pub struct Launch {
    pub url: String,
    pub product_name: String,
    /// The app's declared splash colours, carried alongside `product_name` so
    /// a crash on a second-instance window (plan 058) can render the hub's
    /// crash page in the same palette the first window's splash used —
    /// `main.rs`'s `setup` closure has `spec.manifest` in scope directly, but
    /// this relaunch path only has whatever was managed here.
    pub splash_bg: Option<String>,
    pub splash_text: Option<String>,
}

/// Write the Caddyfile into `data_dir` and answer with its path.
pub fn write_caddyfile(data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = data_dir.join("Caddyfile");
    fs::write(&path, CADDYFILE)?;
    Ok(path)
}

/// Spawn the app's backend, and answer with it and the URL it serves.
///
/// `lock` and `serving` are the liveness and serving locks this launch
/// already holds; both are stored, never acquired here, so there is exactly
/// one place in the codebase — `lifecycle::acquire_launch_locks` — that
/// decides what this process claims.
#[allow(clippy::too_many_arguments)]
pub fn start(
    toolchain: &Toolchain,
    app_dir: &Path,
    environment: &AppEnvironment,
    manifest: &crate::manifest::Manifest,
    update_context: &crate::update_check::Context,
    close_guards: &crate::close_guard::SharedCloseGuards,
    lock: Option<fs::File>,
    serving: Option<fs::File>,
    app: &tauri::AppHandle,
) -> Result<(Sidecar, String), Box<dyn std::error::Error>> {
    // One slot per declared worker, flattened from the manifest's
    // declarations by their copy count (plan 045 step 4).
    let worker_slots = worker::flatten_worker_slots(&manifest.workers);
    let actions = &manifest.actions;
    let url = format!("http://127.0.0.1:{}", environment.port);
    let pid_file = environment.data_dir.join("sidecar.pid");

    // First, and before anything is spawned: an app declaring a worker
    // without the Doctrine Messenger bridge has to fail here, with nothing yet
    // to tear down.
    if !worker_slots.is_empty() {
        worker::setup_transports(toolchain, app_dir, &environment.vars, &environment.log_dir)?;
    }

    let caddyfile = write_caddyfile(&environment.data_dir)?;

    // §3's variables plus the `php` shim. The server and the worker are PHP
    // processes that can shell out to PHP themselves, and under
    // `frankenphp php-cli` the constant every PHP tool uses to re-invoke its own
    // interpreter is empty — so without these two they would land on whatever
    // `php` the machine happens to have, or on none at all, which is precisely
    // the machine the hub exists to serve. Measured, not assumed: see
    // ARCHITECTURE.md's "The `PHP_BINARY` shim".
    let mut envs = environment.vars.clone();
    envs.extend(toolchain.shim_env());

    // Only when some group actually declares `bridge: true`: no thread, no port
    // bound, and no `TFS_BRIDGE_*` in the app's environment otherwise. §3 makes
    // those two variables conditional on a bridge *running*, and
    // present-but-dead would be worse than absent — an app would open a
    // connection to nothing.
    if actions.secrets.bridge || actions.update.bridge || actions.close_guard.bridge {
        let bridge = crate::bridge::start(
            environment.secret_store.clone(),
            actions.secrets.keys.clone(),
            crate::bridge::BridgeGroups {
                secrets: actions.secrets.bridge,
                update: actions.update.bridge,
                close_guard: actions.close_guard.bridge,
            },
            update_context.clone(),
            close_guards.clone(),
        )
        .map_err(|error| format!("Cannot start the actions bridge: {error}"))?;
        envs.push((
            "TFS_BRIDGE_URL",
            format!("http://127.0.0.1:{}", bridge.port),
        ));
        envs.push(("TFS_BRIDGE_TOKEN", bridge.token));
    }

    // The server's own stdout and stderr have nowhere to go when it was
    // launched from a desktop entry, so they are redirected rather than
    // inherited. The file is appended to, never truncated: several launches
    // have to stay comparable in one file. `open`'s own detached child gets
    // the same treatment, for the same reason, into `hub.log` beside this
    // file — see `open::prepare_hub_log` (plan 015).
    let (stdout, stderr) = log::sidecar_log_stdio(&environment.log_dir).map_err(|error| {
        format!(
            "Cannot open {}: {error}",
            environment.log_dir.join("sidecar.log").display()
        )
    })?;

    let mut command = command_with_env(&toolchain.frankenphp, &envs);
    command
        .args(["run", "--config", &path_to_string(&caddyfile)])
        .current_dir(app_dir)
        .stdout(stdout)
        .stderr(stderr);
    // Its own process group, so its whole descendant tree — the PHP workers
    // FrankenPHP forks among them — is reachable by one signal at teardown.
    tfsapp_core::process::set_own_process_group(&mut command);
    let server = command.spawn().map_err(|error| {
        format!(
            "Cannot start FrankenPHP {}: {error}",
            toolchain.frankenphp.display()
        )
    })?;

    println!("{} is listening at {url}", app_dir.display());

    // From `spawn()` onward the server is never loose: every fallible piece
    // of startup below runs against this value, so an `Err` drops it and
    // `Sidecar::stop` reaps both the server and any worker already adopted.
    let sidecar = Sidecar {
        server: Some(server),
        workers: worker_slots
            .iter()
            .map(|_| Arc::new(Mutex::new(None)))
            .collect(),
        shutting_down: Arc::new(AtomicBool::new(false)),
        pid_file,
        lock,
        serving,
    };
    let server_pid = sidecar
        .server
        .as_ref()
        .expect("a freshly constructed Sidecar owns its server")
        .id();

    // The pid file starts with the server alone. If spawning the worker or
    // updating this file fails, `sidecar` drops and removes this partial record
    // while reaping the server it has owned since `spawn()` returned.
    tfsapp_core::process::write_pid_file(&sidecar.pid_file, &format!("{server_pid}\n"))?;

    // Only the workers are gated on a declaration. The Mercure hub is always
    // mounted: it is a Caddy directive, not a process, and costs nothing at
    // rest — which is what lets an app use it without declaring anything.
    if !worker_slots.is_empty() {
        // Shared across every slot: the table so no supervisor's rewrite can
        // erase a sibling's pid, the latch so a launch with several workers
        // still shows the give-up dialog at most once.
        let pid_table = Arc::new(worker::WorkerPidTable::new(
            server_pid,
            sidecar.pid_file.clone(),
            worker_slots.len(),
        ));
        let dialog_shown = Arc::new(AtomicBool::new(false));

        for (slot, transports) in worker_slots.into_iter().enumerate() {
            let child = worker::spawn_worker(
                &toolchain.frankenphp,
                app_dir,
                &envs,
                &environment.log_dir,
                &transports,
                slot,
            )
            .map_err(|error| format!("Cannot start Messenger worker {}: {error}", slot + 1))?;
            let worker_spawned_at = Instant::now();

            // Adopt before the fallible pid-table rewrite. From this point a
            // failed write drops `sidecar`, which owns and stops every worker
            // already adopted, this one included.
            let worker_pid = child.id();
            *sidecar.workers[slot]
                .lock()
                .expect("the worker slot is uncontended before its supervisor starts") =
                Some(child);

            pid_table.set(slot, Some(worker_pid))?;

            worker::spawn_worker_supervisor(worker::WorkerSupervisorConfig {
                worker: Arc::clone(&sidecar.workers[slot]),
                shutting_down: Arc::clone(&sidecar.shutting_down),
                frankenphp: toolchain.frankenphp.clone(),
                app_dir: app_dir.to_path_buf(),
                envs: envs.clone(),
                transports,
                pid_table: Arc::clone(&pid_table),
                slot,
                log_dir: environment.log_dir.clone(),
                worker_spawned_at,
                app: app.clone(),
                dialog_shown: Arc::clone(&dialog_shown),
            });
        }
    }

    Ok((sidecar, url))
}

#[cfg(test)]
#[path = "sidecar_tests.rs"]
mod tests;
