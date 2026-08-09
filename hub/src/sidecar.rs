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
//! runs first, before anything is spawned, so an app that declares
//! `async_worker` without the Doctrine Messenger bridge fails with no sidecar to
//! tear down. Then the server. Then the worker, whose pid becomes the second
//! line of `sidecar.pid` (§6) so the next launch can reap it if this process
//! never gets to.

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
    /// The Messenger worker, when the app declares `async_worker`. Behind a lock
    /// because the supervisor thread replaces it on every recycle.
    pub worker: Arc<Mutex<Option<Child>>>,
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
    /// Worker before server: the worker talks to the app's database through the
    /// same files the server holds open, and stopping the server first would
    /// leave a consumer running against a backend that has gone.
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

        if let Ok(mut worker) = self.worker.lock() {
            if let Some(mut child) = worker.take() {
                println!("Stopping the Messenger worker pid {}", child.id());
                tfsapp_core::process::terminate(child.id());
                let _ = child.wait();
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
pub fn start(
    toolchain: &Toolchain,
    app_dir: &Path,
    environment: &AppEnvironment,
    manifest: &crate::manifest::Manifest,
    lock: Option<fs::File>,
    serving: Option<fs::File>,
    app: &tauri::AppHandle,
) -> Result<(Sidecar, String), Box<dyn std::error::Error>> {
    let async_worker = manifest.async_worker;
    let actions = &manifest.actions;
    let url = format!("http://127.0.0.1:{}", environment.port);
    let pid_file = environment.data_dir.join("sidecar.pid");

    // First, and before anything is spawned: an app declaring `async_worker`
    // without the Doctrine Messenger bridge has to fail here, with nothing yet
    // to tear down.
    if async_worker {
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
    if actions.secrets.bridge || actions.update.bridge {
        let bridge = crate::bridge::start(
            environment.secret_store.clone(),
            actions.secrets.keys.clone(),
            crate::bridge::BridgeGroups {
                secrets: actions.secrets.bridge,
                update: actions.update.bridge,
            },
        )
        .map_err(|error| format!("Cannot start the actions bridge: {error}"))?;
        envs.push((
            "TFS_BRIDGE_URL",
            format!("http://127.0.0.1:{}", bridge.port),
        ));
        envs.push(("TFS_BRIDGE_TOKEN", bridge.token));
    }

    // The app's own stdout and stderr have nowhere to go when it was launched
    // from a desktop entry, so they are redirected rather than inherited. The
    // file is appended to, never truncated: several launches, plus a recycled
    // worker, have to stay comparable in one file. `open`'s own detached
    // child gets the same treatment, for the same reason, into `hub.log`
    // beside this file — see `open::prepare_hub_log` (plan 015).
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

    let server_pid = server.id();
    let shutting_down = Arc::new(AtomicBool::new(false));

    // Only the worker is gated on `async_worker`. The Mercure hub is always
    // mounted: it is a Caddy directive, not a process, and costs nothing at
    // rest — which is what lets an app use it without declaring anything.
    let worker = if async_worker {
        let child =
            worker::spawn_worker(&toolchain.frankenphp, app_dir, &envs, &environment.log_dir)
                .map_err(|error| format!("Cannot start the Messenger worker: {error}"))?;
        let worker_spawned_at = Instant::now();

        // Both pids, server first, worker second — the order CONTRACT.md §6
        // fixes, and the order the next launch's reap reads them back in.
        fs::write(&pid_file, format!("{server_pid}\n{}\n", child.id()))?;

        let worker = Arc::new(Mutex::new(Some(child)));
        worker::spawn_worker_supervisor(worker::WorkerSupervisorConfig {
            worker: Arc::clone(&worker),
            shutting_down: Arc::clone(&shutting_down),
            frankenphp: toolchain.frankenphp.clone(),
            app_dir: app_dir.to_path_buf(),
            envs: envs.clone(),
            pid_file: pid_file.clone(),
            server_pid,
            log_dir: environment.log_dir.clone(),
            worker_spawned_at,
            app: app.clone(),
        });
        worker
    } else {
        fs::write(&pid_file, format!("{server_pid}\n"))?;
        Arc::new(Mutex::new(None))
    };

    Ok((
        Sidecar {
            server: Some(server),
            worker,
            shutting_down,
            pid_file,
            lock,
            serving,
        },
        url,
    ))
}
