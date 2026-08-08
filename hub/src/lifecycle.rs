//! The guards a launch has to pass before an app gets a window.
//!
//! Ported from the station's `lifecycle.rs`, which is where these were written
//! and measured. Three of them come across whole — the version decision against
//! `data/config.json`, the static-port conflict guard, and the sidecar liveness
//! lock with its crash-orphan reap — and one is deliberately reshaped, which is
//! the interesting part.
//!
//! **What is the same.** All of it is CONTRACT.md §6, and §6 is about the *data
//! dir*, which the two hosts share by construction: the same `identifier`
//! resolves to the same directory whether a packaged AppImage or the hub opened
//! it. A guard that read differently on one host would let the two corrupt each
//! other's data, so these are not "ported code", they are the same rules read
//! from the same file.
//!
//! **Everything runs before `tauri::Builder` exists.** Not a style choice: the
//! refusals below show a blocking native dialog through `rfd`, and once Tauri
//! has claimed GTK a raw `rfd` dialog deadlocks rather than appears. The station
//! carries the same constraint and states it the same way; the hub inherits it
//! unchanged because it inherits the reason.
//!
//! **What is reshaped, and why it is a case-1 difference.** On the station, a
//! launch that finds a *newer* binary than the data dir records **is** the update
//! event: someone dropped a newer AppImage over the same data dir, and a launch
//! is the only moment that can notice. The hub has an explicit `update <id>`
//! which owns that event — it replaces the snapshot, runs `pre-update`/
//! `post-update` and records the new version at its success point (plan 011).
//! So the same observation means something different here: an installed snapshot
//! newer than the record is not an update in progress, it is an update that
//! never completed, or a tree edited under the hub. Opening it would run the app
//! against a database its migrations never touched, which is worse than not
//! opening. It is refused, naming the command that resolves it.
//!
//! The event *itself* is not gone, it moved: the hub's `install` is the install
//! event (see `install.rs`, and CONTRACT.md §6, which states the lifecycle as
//! an ordering guarantee rather than as a launch), and it is what writes the
//! record these guards read.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use tfsapp_core::ports::DataConfig;

/// Which lifecycle event (CONTRACT.md §6), if any, a launch represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// No `data/config.json` yet: nothing has recorded a version in this data
    /// dir.
    Install,
    /// Recorded version < the installed app's: on the station a
    /// manually-installed upgrade, in the hub an inconsistency — see the module
    /// header.
    Update,
    /// Recorded version == the installed app's: an ordinary launch.
    None,
}

/// Why [`lifecycle_decision`] could not resolve to an event.
#[derive(Debug)]
pub enum LifecycleDecisionError {
    InvalidVersion(semver::Error),
    Downgrade {
        recorded: semver::Version,
        current: semver::Version,
    },
}

/// Decide the lifecycle event from the recorded version string
/// (`data/config.json`'s `version`, `None` when the file does not exist yet)
/// against the installed app's own `app_version`. No I/O, no dialog.
///
/// Ported unchanged from the station, down to the `Downgrade` variant being an
/// error rather than an event: running an app against data written by a newer
/// version of itself has no defined behaviour, so there is nothing for the
/// caller to do with it but refuse.
pub fn lifecycle_decision(
    recorded: Option<&str>,
    current: &semver::Version,
) -> Result<LifecycleEvent, LifecycleDecisionError> {
    let Some(recorded) = recorded else {
        return Ok(LifecycleEvent::Install);
    };
    let recorded_version =
        semver::Version::parse(recorded).map_err(LifecycleDecisionError::InvalidVersion)?;

    if recorded_version == *current {
        return Ok(LifecycleEvent::None);
    }
    if recorded_version < *current {
        return Ok(LifecycleEvent::Update);
    }
    Err(LifecycleDecisionError::Downgrade {
        recorded: recorded_version,
        current: current.clone(),
    })
}

/// `<data_subdir>/config.json` — the data dir's own record (CONTRACT.md §6),
/// written by both hosts and read by both.
pub fn data_config_path(data_subdir: &Path) -> PathBuf {
    data_subdir.join("config.json")
}

/// The version `data/config.json` records, or `None` when there is no file yet.
///
/// A file that exists but does not parse is an error rather than a `None`:
/// treating it as "no record" would silently re-run an install event over a data
/// dir that already holds someone's database.
pub fn read_data_version(data_subdir: &Path) -> Result<Option<String>, LifecycleError> {
    let path = data_config_path(data_subdir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(LifecycleError::Io { path, source }),
    };

    serde_json::from_str::<DataConfig>(&contents)
        .map(|config| Some(config.version))
        .map_err(|error| LifecycleError::MalformedDataConfig {
            path,
            detail: error.to_string(),
        })
}

/// Record `version` in `data/config.json`, preserving any `port_override`
/// (CONTRACT.md §6).
///
/// Same-directory temp file plus `rename`, the standard POSIX atomic write:
/// `rename(2)` is atomic within one filesystem, so a crash or power loss can
/// only ever leave the old or the new contents in full — never a truncated
/// record that the next launch would read as a corrupt data dir.
///
/// `port_override` is read back and carried over rather than dropped: it is the
/// user's own per-installation escape hatch for a static `app_port` already
/// taken on their machine, and nothing here has any business forgetting it.
pub fn write_data_version(data_subdir: &Path, version: &str) -> Result<(), LifecycleError> {
    let path = data_config_path(data_subdir);
    let port_override = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<DataConfig>(&contents).ok())
        .and_then(|config| config.port_override);

    let config = DataConfig {
        version: version.to_string(),
        port_override,
    };
    let json = serde_json::to_string_pretty(&config).map_err(|error| {
        LifecycleError::MalformedDataConfig {
            path: path.clone(),
            detail: error.to_string(),
        }
    })?;

    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| LifecycleError::Io { path, source }
    };
    let temporary = data_subdir.join("config.json.tmp");
    fs::write(&temporary, json).map_err(io_error(&temporary))?;
    fs::rename(&temporary, &path).map_err(io_error(&path))
}

/// How long an arriving launch waits for a dying sibling's liveness lock to
/// free up (`LaunchDecision::Wait`) before refusing. Named next to what it
/// tracks: plan 011 step 1 measured the two-stage SIGTERM-then-SIGKILL
/// teardown at 3.15s with no `async_worker` and 6.28–6.40s with one — two
/// sequential `terminate()` calls, each escalating past its own 3s budget on
/// the machine it was measured on — so this is that worst case plus margin
/// for scheduling jitter, not a number picked by feel.
const SERVING_WAIT_BUDGET: Duration = Duration::from_secs(10);

/// `<data_dir>/serving.lock` (CONTRACT.md §6) — beside `sidecar.pid` and
/// `sidecar.pid.lock`, so an installed launch and a dev session get theirs by
/// the same identifier-keyed rule that gives them everything else.
///
/// Answers a different question than the liveness lock: "is there an
/// instance willing to be handed a window right now", not "does this process
/// still own this data dir". Held means exactly that — hand this launch's
/// argv to whoever holds it and it will answer with a window. See the plan's
/// Overview for why the two used to be one signal and had to become two.
pub fn serving_lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("serving.lock")
}

/// What an arriving launch is told, from probing the serving lock and the
/// liveness lock, in that order (see the plan's Overview). Pure and total
/// over its two inputs, so every case is a unit test with no process,
/// display or session bus involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchDecision {
    /// A live sibling holds the serving lock: run no guard, touch nothing,
    /// and let `tauri-plugin-single-instance` hand this launch's argv to it —
    /// the ordinary second-`open`-of-a-running-app case, and it pays nothing.
    HandOff,
    /// The serving lock is free but the liveness lock is held: a sibling has
    /// begun shutting down. Wait, bounded, for it to finish, then launch as
    /// though nothing had been there.
    Wait,
    /// Both locks are free: launch immediately.
    Launch,
}

/// Decide [`LaunchDecision`] from the two probes. `serving_held` alone
/// decides `HandOff` — a live sibling holding the serving lock can hand a
/// window over immediately whether or not it is also mid liveness-lock
/// housekeeping — so `liveness_held` is only consulted once `serving_held` is
/// false.
pub fn decide_launch(serving_held: bool, liveness_held: bool) -> LaunchDecision {
    if serving_held {
        LaunchDecision::HandOff
    } else if liveness_held {
        LaunchDecision::Wait
    } else {
        LaunchDecision::Launch
    }
}

/// Everything a launch that got past [`acquire_launch_locks`] holds for its
/// whole lifetime: the sidecar liveness lock (CONTRACT.md §6) and the serving
/// lock ([`serving_lock_path`]).
#[derive(Debug)]
pub struct LaunchLocks {
    pub liveness: fs::File,
    pub serving: fs::File,
}

/// Why [`acquire_launch_locks`] could not hand back a decision to launch.
#[derive(Debug)]
pub enum LaunchLockError {
    /// The wait for a dying sibling's liveness lock outlived the budget it
    /// was given.
    Timeout(Duration),
    /// A probe itself failed — kept distinct from `Timeout` so a caller names
    /// what actually went wrong rather than blaming a stuck sibling for it.
    Io(std::io::Error),
}

impl std::fmt::Display for LaunchLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(budget) => write!(
                formatter,
                "a previous instance of this app did not finish shutting down within {}s. It \
                 may be stuck; try again in a moment.",
                budget.as_secs()
            ),
            Self::Io(error) => {
                write!(
                    formatter,
                    "cannot tell whether this app is already running: {error}"
                )
            }
        }
    }
}

/// The guard pair [`prepare_launch`] and [`prepare_dev_launch`] share: probe
/// the serving lock, hand off at once if a live sibling holds it (`Ok(None)`,
/// exactly what `cleanup_previous_sidecar` alone answered before this plan);
/// otherwise reap and take the liveness lock — waiting up to `wait_budget` if
/// a sibling was mid-teardown — then take the serving lock for this launch's
/// own lifetime and answer `Ok(Some(..))`.
///
/// `wait_budget` is a parameter rather than reading [`SERVING_WAIT_BUDGET`]
/// directly so a test can shrink it and prove `Timeout` without spending the
/// real budget; both callers below pass the real constant.
///
/// Kept free of [`fatal_startup_error`] on purpose, unlike its two callers:
/// this is the part worth testing without a process willing to exit under it,
/// so a timeout comes back as [`LaunchLockError::Timeout`] for the caller to
/// turn into a dialog.
fn acquire_launch_locks(
    pid_file: &Path,
    data_dir: &Path,
    identifier: &str,
    wait_budget: Duration,
) -> Result<Option<LaunchLocks>, LaunchLockError> {
    let serving_path = serving_lock_path(data_dir);
    let serving_held = tfsapp_core::process::try_lock_file(&serving_path)
        .map_err(LaunchLockError::Io)?
        .is_none();
    let liveness_held =
        tfsapp_core::process::is_owner_live(pid_file).map_err(LaunchLockError::Io)?;

    let wait = match decide_launch(serving_held, liveness_held) {
        LaunchDecision::HandOff => return Ok(None),
        LaunchDecision::Wait => {
            println!(
                "tfsapp-hub: a previous instance of this app is still shutting down, waiting up \
                 to {}s for it to finish...",
                wait_budget.as_secs()
            );
            wait_budget
        }
        LaunchDecision::Launch => Duration::ZERO,
    };

    let Some(liveness) = tfsapp_core::process::cleanup_previous_sidecar(pid_file, identifier, wait)
    else {
        return Err(LaunchLockError::Timeout(wait_budget));
    };
    let serving = tfsapp_core::process::try_lock_file(&serving_path)
        .map_err(LaunchLockError::Io)?
        .expect(
            "the serving lock was just observed free and nothing else in this launch's own \
             process claims it — a lock taken out from under it here would be a different bug",
        );

    Ok(Some(LaunchLocks { liveness, serving }))
}

/// Run every guard, and answer with the locks (CONTRACT.md §6) this launch is
/// to hold for its whole lifetime.
///
/// `None` means a live sibling already holds the serving lock: this launch is
/// the second one of the same app, it ran no guard, touched nothing, and has
/// one job left — hand its argv to the running instance and go away, which is
/// `tauri-plugin-single-instance`'s from here. A sibling caught mid-teardown
/// is not this case — see [`acquire_launch_locks`] and the plan's Overview.
///
/// The lifecycle *event* is deliberately not returned. On the station it is,
/// because `setup` runs the event's hooks off it; here the hooks belong to the
/// hub's own `install`/`update` commands, so every use of the event is made
/// inside this function — refuse, or stamp a missing record — and handing a
/// caller a value with nothing to do would only invite one to be invented.
///
/// The reap comes first, and that ordering is load-bearing rather than
/// arbitrary: a crashed launch of this same app leaves a FrankenPHP holding its
/// static port, so reaping before the port guard frees the port the guard is
/// about to test. The other way round, the guard dead-ends on a conflict this
/// app's own corpse caused.
///
/// Every refusal below exits the process through [`fatal_startup_error`] rather
/// than returning an error, because there is no caller in a position to do
/// anything else: this runs in the app's own process, launched possibly from a
/// desktop entry where stderr goes nowhere, and the only useful outcome is a
/// dialog the user can read.
pub fn prepare_launch(
    id: &str,
    data_dir: &Path,
    data_subdir: &Path,
    identifier: &str,
    app_version: &str,
    app_port: Option<u16>,
) -> Option<LaunchLocks> {
    let locks = match acquire_launch_locks(
        &data_dir.join("sidecar.pid"),
        data_dir,
        identifier,
        SERVING_WAIT_BUDGET,
    ) {
        Ok(locks) => locks,
        Err(error) => fatal_startup_error(&error.to_string()),
    };
    locks.as_ref()?;

    let event = check_version(id, data_subdir, app_version);
    // A data dir with no record at all, under an app the hub installed: the
    // install event already ran, at install time, so there is nothing to run
    // here and only a record to catch up on. Stamping it is what makes the next
    // launch quiet — and what gives a *packaged* AppImage of this same app a
    // version to compare its own guard against, since the two share this file.
    if event == LifecycleEvent::Install {
        if let Err(error) = write_data_version(data_subdir, app_version) {
            fatal_startup_error(&error.to_string());
        }
    }
    check_port(app_port, data_subdir);

    locks
}

/// The dev variant of [`prepare_launch`]: the same guard pair, the same port
/// guard, but never the version guard — a dev session was never installed, so
/// there is no `data/config.json` to compare `app_version` against, and none
/// is written (plan 009 step 4). Lifecycle hooks stay out of both:
/// `pre-install`/`post-install`/`pre-update`/`post-update` belong to
/// `install`/`update`, and a dev launch is neither.
///
/// `id` and `data_subdir` from [`prepare_launch`] have no dev counterpart to
/// pass, since there is no version guard here to name an app to or a
/// `config.json` to write.
pub fn prepare_dev_launch(
    data_dir: &Path,
    data_subdir: &Path,
    identifier: &str,
    app_port: Option<u16>,
) -> Option<LaunchLocks> {
    let locks = match acquire_launch_locks(
        &data_dir.join("sidecar.pid"),
        data_dir,
        identifier,
        SERVING_WAIT_BUDGET,
    ) {
        Ok(locks) => locks,
        Err(error) => fatal_startup_error(&error.to_string()),
    };
    locks.as_ref()?;

    check_port(app_port, data_subdir);

    locks
}

/// The version guard: decide the event, and refuse the three outcomes an app
/// cannot be opened under.
fn check_version(id: &str, data_subdir: &Path, app_version: &str) -> LifecycleEvent {
    let config_file = data_config_path(data_subdir);

    let recorded = match read_data_version(data_subdir) {
        Ok(recorded) => recorded,
        Err(error) => fatal_startup_error(&error.to_string()),
    };
    let current = match semver::Version::parse(app_version) {
        Ok(current) => current,
        // The installer refuses a non-semver `app_version` (CONTRACT.md §2), so
        // reaching this means the installed snapshot's manifest was edited since.
        Err(error) => fatal_startup_error(&format!(
            "This app declares version {app_version:?}, which is not canonical semver \
             ({error}). It is what decides whether this data dir is up to date, so it \
             cannot be compared — reinstall the app."
        )),
    };

    // Bound rather than matched inline: the arms below move `recorded`, and the
    // borrow `as_deref()` takes would otherwise outlive the call it was made for.
    let decision = lifecycle_decision(recorded.as_deref(), &current);
    match decision {
        // The hub's own `update <id>` owns the update event and records its
        // version at the event's success point, so an installed snapshot newer
        // than the record is never an update in progress — it is one that never
        // finished, or a tree edited under the hub. See the module header for
        // why this is a refusal here and an event on the station.
        Ok(LifecycleEvent::Update) => fatal_startup_error(&format!(
            "This app's installed source is version {current}, but its data dir was last \
             written by version {}. Its update never completed, so its migrations may not \
             have run — opening it could corrupt data. Run `tfsapp-hub update {id}` to \
             finish it.",
            recorded.unwrap_or_default()
        )),
        Ok(event) => event,
        Err(LifecycleDecisionError::InvalidVersion(error)) => fatal_startup_error(&format!(
            "Invalid version in {}: {error}",
            config_file.display()
        )),
        Err(LifecycleDecisionError::Downgrade { recorded, current }) => {
            fatal_startup_error(&format!(
                "This installation's data was written by app version {recorded}, but the \
                 installed app is version {current}. Downgrading is not automated — edit {} \
                 to resolve.",
                config_file.display()
            ))
        }
    }
}

/// The static-port guard (CONTRACT.md §6).
///
/// A no-op for an app that pins no port, which is the default and the common
/// case: a dynamic port is picked free at launch and can neither claim nor lose
/// a number. For one that does pin, the number is bound and released here, before
/// FrankenPHP is asked to use it, so the failure is a message naming
/// `port_override` rather than a sidecar dying with a bind error nobody sees.
fn check_port(app_port: Option<u16>, data_subdir: &Path) {
    let port = match tfsapp_core::ports::resolve_packaged_port(app_port, data_subdir) {
        Ok(Some(port)) => port,
        Ok(None) => return,
        Err(error) => fatal_startup_error(&format!("Cannot settle the app's port: {error}")),
    };

    if let Err(error) = tfsapp_core::ports::check_packaged_port(app_port, port, data_subdir) {
        fatal_startup_error(&error.to_string());
    }
}

/// Every fatal pre-`Builder` startup error's single exit: a blocking native
/// dialog, then the same text on stderr, then exit 1.
///
/// Both, because there are two audiences and they never overlap. A user who
/// double-clicked a desktop entry has no stderr — without the dialog the app
/// just fails to open, silently, which is the worst outcome this whole module
/// exists to avoid. A developer at a terminal wants the line where they typed
/// the command, not a modal to dismiss.
///
/// Safe here, and only here: `rfd` drives GTK itself, so this is sound exactly
/// while `tauri::Builder` has not claimed it yet. Anything fatal after that
/// point needs the app's own dialog plugin, off the main thread.
pub fn fatal_startup_error(message: &str) -> ! {
    rfd::MessageDialog::new()
        .set_title("TFSApp Hub: startup error")
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    eprintln!("tfsapp-hub: {message}");
    std::process::exit(1);
}

/// Every fatal error after `.setup()` has been reached: stop the sidecar, show
/// the app's own dialog, exit.
///
/// **It must run on its own thread, never on the calling one.** The dialog
/// plugin dispatches the real dialog to the main thread and blocks the caller
/// waiting for the answer. Called from `.setup()` — which *is* the main thread —
/// that queued work could never run: the event loop cannot reach the iteration
/// that would dispatch it while the callback waiting on its result is still on
/// the stack. The dialog would silently never render and the process would hang
/// for ever. The station reproduced exactly that live before fixing it the same
/// way, which is why every call site here spawns and returns immediately.
pub fn fatal_post_setup_error(app: tauri::AppHandle, message: String) {
    use tauri_plugin_dialog::DialogExt;

    std::thread::spawn(move || {
        stop_sidecar(&app);
        app.dialog()
            .message(&message)
            .title("TFSApp Hub: startup error")
            .kind(tauri_plugin_dialog::MessageDialogKind::Error)
            .blocking_show();
        eprintln!("tfsapp-hub: {message}");
        std::process::exit(1);
    });
}

/// Stop the managed sidecar, if this process ever got as far as having one.
fn stop_sidecar(app: &tauri::AppHandle) {
    use tauri::Manager;

    if let Some(sidecar) = app.try_state::<std::sync::Mutex<crate::sidecar::Sidecar>>() {
        if let Ok(mut sidecar) = sidecar.lock() {
            sidecar.stop();
        }
    }
}

/// Release everything this process claims to be *serving*, before anything
/// downstream is signalled: the serving lock, so an arriving launch's probe
/// (`acquire_launch_locks`) stops finding a live instance to hand off to, and
/// the single-instance D-Bus name, so `tauri-plugin-single-instance` stops
/// routing new launches here at all.
///
/// Both ahead of [`stop_sidecar`], deliberately: "stop claiming to serve" and
/// "have finished tearing down" are seconds apart under the
/// SIGTERM-then-SIGKILL escalation `SERVING_WAIT_BUDGET` is sized against,
/// and an arriving launch has no reason to wait out either just because this
/// process has not finished dying yet.
fn release_serving_claim(app: &tauri::AppHandle) {
    use tauri::Manager;

    if let Some(sidecar) = app.try_state::<std::sync::Mutex<crate::sidecar::Sidecar>>() {
        if let Ok(mut sidecar) = sidecar.lock() {
            sidecar.serving.take();
        }
    }
    // Blocking (a real D-Bus call), which is exactly why every caller of
    // `stop_sidecar_and_exit` runs it off the GTK main thread already — see
    // that function's own doc comment. The window closing this teardown is
    // hidden by the time either caller gets here, so nothing on screen is
    // waiting on it.
    tauri_plugin_single_instance::destroy(app);
}

/// Stop the sidecar and exit — the one shutdown body, shared by the last window
/// closing and by a signal, so a `SIGTERM` tears the app down exactly the way
/// the user closing it does.
fn stop_sidecar_and_exit(app: &tauri::AppHandle) {
    release_serving_claim(app);
    stop_sidecar(app);
    app.exit(0);
}

/// The window-close handler.
///
/// On the *last* window's close request the default synchronous close is vetoed
/// — otherwise it races the teardown below — the window is hidden at once so the
/// user sees their click land, and the sidecar is stopped off the GTK main
/// thread. That last part matters: teardown escalates SIGTERM to SIGKILL over up
/// to three seconds, and doing that on the main thread would freeze a window
/// that is still on screen.
///
/// A window closing while others remain closes only itself: the backend belongs
/// to the app, not to any one of its windows.
pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    use tauri::Manager;

    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        // The closing window is still open at this point, so its own label is
        // excluded — the question is whether anything *else* would be left.
        let others_remain = window
            .app_handle()
            .webview_windows()
            .into_keys()
            .any(|label| label != window.label());

        if !others_remain
            && window
                .try_state::<std::sync::Mutex<crate::sidecar::Sidecar>>()
                .is_some()
        {
            api.prevent_close();
            let _ = window.hide();
            let app = window.app_handle().clone();
            std::thread::spawn(move || stop_sidecar_and_exit(&app));
        }
    }
}

/// Turn a `SIGINT`/`SIGTERM` into the same orderly shutdown as closing the last
/// window.
///
/// Without it the default disposition applies: this process dies on the spot,
/// `Sidecar::stop` never runs, and FrankenPHP — plus the Messenger worker —
/// survives as an orphan holding the app's database open. The safety nets still
/// hold (the OS releases the liveness lock, so the next launch reaps it), but
/// nothing reclaims those processes in the meantime.
///
/// A handler may only make async-signal-safe calls, so it writes one byte to a
/// pipe and the reaction happens on an ordinary thread — `core`'s self-pipe
/// machinery, unchanged.
pub fn install_shutdown_on_signal(app: &tauri::AppHandle) {
    let read_fd = match tfsapp_core::process::install_signal_forwarding() {
        Ok(fd) => fd,
        Err(error) => {
            // Not fatal: the app runs exactly as it would have, orphans and
            // all, rather than refusing to open over a failed pipe.
            eprintln!("tfsapp-hub: cannot install signal handling: {error}");
            return;
        }
    };
    let app = app.clone();
    tfsapp_core::process::spawn_on_signal(read_fd, move || {
        println!("Received a termination signal, stopping the backend");
        stop_sidecar_and_exit(&app);
    });
}

#[derive(Debug)]
pub enum LifecycleError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    MalformedDataConfig {
        path: PathBuf,
        detail: String,
    },
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::MalformedDataConfig { path, detail } => write!(
                formatter,
                "cannot read {}: {detail}. It records which version of the app wrote this \
                 data dir (CONTRACT.md §6); the hub will not run an app against data it \
                 cannot date.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
