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
//! (`update.rs`) which owns that event — it snapshots the database, replaces
//! the tree, runs `pre-update`/`post-update` and records the new version at
//! its success point, reverting all three together on any failure.
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
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
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

/// The rollback anchor's three halves (CONTRACT.md §6 / the per-app update and
/// rollback plan): the retained source tree, the pre-update database snapshot
/// and `rollback.json`, and the primitives that produce, restore and consume
/// them. Ported from the station's `lifecycle.rs`/`rollback.rs`, adapted to
/// take a `data_subdir` rather than reading a global — this hub has no single
/// installed app to assume one for.
///
/// The three SQLite files an update event's DB snapshot/restore covers: the
/// main file and its WAL/SHM twins, which may not exist (no hot WAL left by a
/// crash, or — updating an installation that never created `app.db` — no DB at
/// all yet).
pub const DB_FILE_NAMES: [&str; 3] = ["app.db", "app.db-wal", "app.db-shm"];

/// `<data_subdir>/<name>.pre-update` — the snapshot twin of `data_subdir`'s
/// `name`, written by [`snapshot_db`] before an update event's `pre-update`
/// hook runs. Consumed by [`restore_db_snapshot`] on a failed update; left in
/// place as the rollback anchor's database half on a succeeded one.
pub fn db_snapshot_path(data_subdir: &Path, name: &str) -> PathBuf {
    data_subdir.join(format!("{name}.pre-update"))
}

/// Snapshot `app.db` (+ any `-wal`/`-shm` twins) before an update event's
/// `pre-update` hook runs — the sidecar isn't started yet, so the files are
/// quiescent. A source file that doesn't exist (no hot WAL from a previous
/// crash, or no DB at all for a fresh installation) snapshots "absence": any
/// leftover twin from an earlier attempt is removed rather than left stale, so
/// [`restore_db_snapshot`] correctly deletes a partially created file on
/// restore instead of resurrecting an unrelated older one.
pub fn snapshot_db(data_subdir: &Path) -> io::Result<()> {
    for name in DB_FILE_NAMES {
        let source = data_subdir.join(name);
        let snapshot = db_snapshot_path(data_subdir, name);
        if source.is_file() {
            fs::copy(&source, &snapshot)?;
        } else {
            let _ = fs::remove_file(&snapshot);
        }
    }
    Ok(())
}

/// Put `app.db` (+ `-wal`/`-shm`) back to the state [`snapshot_db`] captured: a
/// present twin is copied back over the live file; an absent one (the source
/// didn't exist when snapshotted) deletes the live file, so a failed event
/// that created one along the way is fully reverted — and a `-wal`/`-shm` a
/// restored main file did not itself bring back is not left beside it to
/// silently keep committed transactions from the version that was reverted.
pub fn restore_db_snapshot(data_subdir: &Path) -> io::Result<()> {
    for name in DB_FILE_NAMES {
        let target = data_subdir.join(name);
        let snapshot = db_snapshot_path(data_subdir, name);
        if snapshot.is_file() {
            fs::copy(&snapshot, &target)?;
        } else {
            let _ = fs::remove_file(&target);
        }
    }
    Ok(())
}

/// Delete the pre-update snapshot files, without touching the live database —
/// `rollback <id>`'s own consumption of the anchor's database half, once
/// [`restore_db_snapshot`] has already put its contents back as the live
/// database. Best-effort: a file already gone is not an error.
pub fn discard_db_snapshot(data_subdir: &Path) {
    for name in DB_FILE_NAMES {
        let _ = fs::remove_file(db_snapshot_path(data_subdir, name));
    }
}

/// `<data_subdir>/<name>.rescue` — where `rollback <id>` copies the *current*
/// (post-update) database before overwriting it with the restored pre-update
/// snapshot: a manual-recovery artefact at the new schema, never auto-restored
/// — moving the data aside, not losing it. A second consecutive rollback
/// overwrites the previous rescue dump.
pub fn rescue_dump_path(data_subdir: &Path, name: &str) -> PathBuf {
    data_subdir.join(format!("{name}.rescue"))
}

/// `apps/<id>.previous` — the rollback anchor's tree half: the outgoing
/// `apps/<id>` renamed rather than copied (the per-app update and rollback
/// plan's Overview — a rename costs one generation of the tree, never a copy
/// pass over it). Cannot collide with a real app directory:
/// `install::is_usable_id` rejects a dot in an `id`, so no `id` can ever
/// resolve to this name itself.
pub fn previous_tree_path(app_dir: &Path) -> PathBuf {
    let mut previous = app_dir.as_os_str().to_os_string();
    previous.push(".previous");
    PathBuf::from(previous)
}

/// `<data_subdir>/rollback.json` — the rollback anchor's registry half: what
/// `rollback <id>` needs to restore the registry entry that the retained tree
/// and database snapshot cannot answer on their own. `source_revision` is
/// hashed over the *source* (`source::EXCLUDED_FROM_HASH`), while the retained
/// tree under `apps/` was copied with `install`'s own excluded paths/names —
/// recomputing it from the tree would produce a different string that means
/// nothing, so it is recorded instead.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RollbackAnchor {
    pub app_version: String,
    pub source_revision: String,
    /// RFC 3339, UTC — same convention as the registry's own timestamps
    /// ([`crate::registry::now_timestamp`]).
    pub created_at: String,
}

/// `<data_subdir>/rollback.json`'s path.
pub fn rollback_anchor_path(data_subdir: &Path) -> PathBuf {
    data_subdir.join("rollback.json")
}

/// Write `rollback.json`, atomic temp-file-plus-`rename` like
/// [`write_data_version`] — a crash or power loss mid-write can never leave a
/// truncated anchor record behind.
pub fn write_rollback_anchor(
    data_subdir: &Path,
    anchor: &RollbackAnchor,
) -> Result<(), LifecycleError> {
    let path = rollback_anchor_path(data_subdir);
    let json = serde_json::to_string_pretty(anchor).map_err(|error| {
        LifecycleError::MalformedDataConfig {
            path: path.clone(),
            detail: error.to_string(),
        }
    })?;

    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| LifecycleError::Io { path, source }
    };
    let temporary = data_subdir.join("rollback.json.tmp");
    fs::write(&temporary, json).map_err(io_error(&temporary))?;
    fs::rename(&temporary, &path).map_err(io_error(&path))
}

/// Read `rollback.json`, tolerantly: absent or unparseable both read as
/// `None` rather than an error — either one means the same thing to a caller
/// (nothing usable to roll back to), and an unknown key never makes an
/// otherwise-valid anchor unusable, since `serde` ignores fields this struct
/// does not declare by default.
pub fn read_rollback_anchor(data_subdir: &Path) -> Option<RollbackAnchor> {
    fs::read_to_string(rollback_anchor_path(data_subdir))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
}

/// Discard `rollback.json` without touching the retained tree or database
/// snapshot. Best-effort: an already-missing file is not an error.
pub fn discard_rollback_anchor(data_subdir: &Path) {
    let _ = fs::remove_file(rollback_anchor_path(data_subdir));
}

/// Whether `id`'s rollback anchor is actually usable — all three halves
/// (the retained tree, the retained database snapshot's main file, and
/// `rollback.json`), or nothing (the plan's Overview: "three halves or no
/// anchor"). Pure-ish: reads three paths and no more, so a missing tree, a
/// missing snapshot and a missing `rollback.json` are each a unit test with no
/// process involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    Complete {
        app_version: String,
        source_revision: String,
        created_at: String,
    },
    Missing,
}

pub fn anchor_state(data_subdir: &Path, app_dir: &Path) -> Anchor {
    let has_tree = previous_tree_path(app_dir).is_dir();
    let has_snapshot = db_snapshot_path(data_subdir, "app.db").is_file();

    match (has_tree, has_snapshot, read_rollback_anchor(data_subdir)) {
        (true, true, Some(record)) => Anchor::Complete {
            app_version: record.app_version,
            source_revision: record.source_revision,
            created_at: record.created_at,
        },
        _ => Anchor::Missing,
    }
}

/// How long an arriving launch waits for a dying sibling's liveness lock to
/// free up (`LaunchDecision::Wait`) before refusing.
///
/// **It is not sized against the ordinary teardown, and that is deliberate.**
/// A teardown now takes 0.28–0.35s on an app with `async_worker` and
/// 0.07–0.13s without (plan 014 step 4, ten runs each), so this budget is
/// essentially never reached. What it has to cover is the *slow* teardown,
/// because the two outcomes are not symmetric: waiting longer than necessary
/// costs a spinner, while refusing too early costs a launch that would have
/// succeeded a moment later — and the refusal is a dialog the user has to
/// dismiss and retry.
///
/// So it is the sum of the bounded waits a teardown can spend, each of them a
/// constant in this tree rather than a measurement: 2s for the webview windows
/// to go (`WINDOW_DESTROY_BUDGET`), then up to 3s for the worker and up to 3s
/// for the server (`tfsapp_core::process::terminate`'s own escalation, twice,
/// in sequence). Eight seconds of ceiling, plus margin for scheduling jitter.
/// Past that, "it may be stuck" is the true statement rather than an
/// impatient one.
///
/// The version of this comment before plan 014 cited plan 011's measured
/// 3.15s/6.28–6.40s and claimed not to be a number picked by feel. Those
/// numbers were two SIGKILL escalations firing on every single teardown — a
/// defect, measured — so the claim was false in the one way a comment must not
/// be. Both halves of the defect are gone; the number happens to land in the
/// same place, for a reason that can now be checked against the code.
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

    // Rule 3's launch-side refusal (plan 013, CONTRACT.md §6's "Running a
    // declared command"): a `run` command holding `run.lock` for this app
    // owns its data dir just as much as a live window does, so a window must
    // not open over it — a `bin/console` command opening the app's SQLite
    // while a window is being spawned into it is the same hazard `run.rs`'s
    // own rule 3 exists to prevent, read the other way round. Placed here,
    // after `acquire_launch_locks` has already decided to launch rather than
    // hand off: a launch that hands off to a live sibling
    // (`LaunchDecision::HandOff`, `locks.as_ref()?` above) must never reach
    // this check, since it is the ordinary "second window on an app that is
    // already up" case and a `concurrent` alias legitimately running beside
    // that window would otherwise be blocked by it.
    check_run_lock(id, data_dir);

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
/// Nor does the rule 3 `run.lock` probe (plan 013) reach here: a dev
/// session's `run.lock` would live under its own project's `var/`, not under
/// `data_dir`, and `run <id> <alias>` takes a hub-local `id` a dev session
/// never has — nothing in the hub ever writes one for it.
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

/// What [`probe_run_lock`] found: `Free` means a launch may proceed.
/// `Held { alias }` means `run.lock` is held — `alias` is whatever its record
/// could tell us, from the alias name it was written with down to `None` when
/// the lock is held but its record could not be read (a race with `run`'s own
/// acquisition-time write, or a legacy/corrupt file) — the caller still
/// refuses either way, just with a shorter message when there is nothing to
/// name.
///
/// `pub(crate)`, not private: plan 016's `install::check_data_dir_available`
/// reads this same lock before writing into a data directory, and it must be
/// the one reader of what "held" means — never a second probe with its own,
/// possibly diverging, idea of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunLockHeld {
    Free,
    Held { alias: Option<String> },
}

/// Probe `<data_dir>/run.lock` (plan 013, CONTRACT.md §6): the pure,
/// Result-returning half of [`check_run_lock`], kept apart from it exactly as
/// [`acquire_launch_locks`] is kept apart from [`prepare_launch`] — so a held
/// lock, a free one, and an unreadable record are each a unit test with no
/// process willing to exit under it.
///
/// Never retains the lock — probe and drop, the same "never retain, just
/// observe" pattern [`tfsapp_core::process::is_owner_live`] uses for the
/// liveness lock.
pub(crate) fn probe_run_lock(data_dir: &Path) -> std::io::Result<RunLockHeld> {
    let run_lock_path = data_dir.join("run.lock");
    if tfsapp_core::process::try_lock_file(&run_lock_path)?.is_some() {
        return Ok(RunLockHeld::Free);
    }
    let alias = fs::read_to_string(&run_lock_path)
        .ok()
        .and_then(|contents| crate::run::parse_run_lock(&contents))
        .map(|record| record.alias);
    Ok(RunLockHeld::Held { alias })
}

/// Who is already using a data directory — a live app window, or an active
/// `run` command. Returned by [`data_dir_holder`]; `None` there means free.
///
/// Shared, not install-specific despite the name it carries over from plan
/// 016: `export`/`import` (plan 022) are further writers into a data
/// directory and refuse the same two ways something is already using it, so
/// there is one definition of "who holds this data directory" rather than a
/// second one that could drift from the first.
#[derive(Debug)]
pub enum DataDirHolder {
    /// A live app window holds the sidecar liveness lock (CONTRACT.md §6).
    Window,
    /// An active `run` command holds `run.lock` (rule 3). `alias` is
    /// whatever [`probe_run_lock`] could read from the record — `None` in
    /// the narrow window between `run`'s own lock acquisition and its first
    /// write.
    RunCommand { alias: Option<String> },
}

/// Probe whether something already holds `data_dir` — a live window, then an
/// active `run` command, in that order — returning who, if anyone.
///
/// The window check comes first because it is the more common case and the
/// cheaper probe; both are read-only and retain nothing, exactly like
/// [`probe_run_lock`] itself. `data_dir` not existing is out of scope here —
/// each caller already knows what "nothing to check yet" means for its own
/// command (an install has nothing to refuse; an export has nothing to
/// read), so that is decided before this is ever called.
pub fn data_dir_holder(data_dir: &Path) -> io::Result<Option<DataDirHolder>> {
    let pid_file = data_dir.join("sidecar.pid");
    if tfsapp_core::process::is_owner_live(&pid_file).unwrap_or(false) {
        return Ok(Some(DataDirHolder::Window));
    }

    match probe_run_lock(data_dir)? {
        RunLockHeld::Free => Ok(None),
        RunLockHeld::Held { alias } => Ok(Some(DataDirHolder::RunCommand { alias })),
    }
}

/// Rule 3's launch-side refusal (plan 013, CONTRACT.md §6): refuse to open a
/// window while a `run` command holds `run.lock` for this app, naming the
/// active alias when [`probe_run_lock`] found one and pointing at the way to
/// release it either way.
fn check_run_lock(id: &str, data_dir: &Path) {
    match probe_run_lock(data_dir) {
        Ok(RunLockHeld::Free) => {}
        Ok(RunLockHeld::Held { alias: Some(alias) }) => fatal_startup_error(&format!(
            "{id} cannot open a window while its \"{alias}\" run command is active — stop it \
             first with `tfsapp-hub run --stop {id}`."
        )),
        Ok(RunLockHeld::Held { alias: None }) => fatal_startup_error(&format!(
            "{id} cannot open a window while a run command is active — stop it first with \
             `tfsapp-hub run --stop {id}`."
        )),
        Err(error) => fatal_startup_error(&format!(
            "cannot probe {}: {error}",
            data_dir.join("run.lock").display()
        )),
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

/// Whether a failed launch's message should also raise a native dialog, given
/// whether stderr is a terminal.
///
/// Pure, and the one thing worth testing directly: get this backwards and
/// either a developer's typo pops a modal to dismiss, or a `.desktop` launch's
/// failure lands nowhere anyone will see it. A terminal user and a
/// `.desktop` launch do not overlap in practice — one has a place for the
/// line to land, the other has none — but that is an empirical fact about
/// how the hub is invoked, not a guarantee, which is why it is checked here
/// rather than assumed.
fn dialog_is_warranted(stderr_is_terminal: bool) -> bool {
    !stderr_is_terminal
}

/// A failed launch's single reporter, used on both sides of `open`'s re-exec
/// (plan 015): `tfsapp-hub: {message}` on stderr always, and the same blocking
/// native dialog `fatal_startup_error` already used, but only when stderr is
/// not a terminal ([`std::io::IsTerminal`]) — see [`dialog_is_warranted`] for
/// the decision itself.
///
/// **Safe on both sides of the re-exec, for the same reason on each.**
/// `rfd` is sound exactly while `tauri::Builder` has not claimed GTK yet
/// (`fatal_startup_error`'s own doc comment). The child calls this before
/// `Builder` is ever built, same as before this plan; the parent never builds
/// one at all, so it is sound there for the whole of its life. Two different
/// situations that both cash out the same way — worth stating so the next
/// reader does not have to re-derive it from the warning in the other
/// comment.
///
/// Does not exit: `open::run`'s parent-side caller has an exit code of its
/// own to return, which is `main`'s to give back and the CLI tests read.
pub fn report_launch_failure(message: &str) {
    use std::io::IsTerminal;

    if dialog_is_warranted(std::io::stderr().is_terminal()) {
        rfd::MessageDialog::new()
            .set_title("TFSApp Hub: startup error")
            .set_description(message)
            .set_level(rfd::MessageLevel::Error)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
    eprintln!("tfsapp-hub: {message}");
}

/// Every fatal pre-`Builder` startup error's single exit: [`report_launch_failure`],
/// then exit 1.
pub fn fatal_startup_error(message: &str) -> ! {
    report_launch_failure(message);
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
/// Both ahead of [`destroy_windows`] and [`stop_sidecar`], deliberately:
/// "stop claiming to serve" and "have finished tearing down" are still two
/// different moments, and an arriving launch has no reason to wait out the
/// second just because this process has not finished dying yet. Plan 014 made
/// the gap small — a third of a second rather than six — but not zero, and it
/// is not a latency target: `CONTRACT.md` §5 needs a launch that arrives
/// inside it to wait rather than attach, at any width. It is still taken on
/// every close-then-immediate-reopen (plan 014 step 4, twenty out of twenty).
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

/// How long [`destroy_windows`] waits for the event loop to confirm this
/// process's windows are actually gone. Generous, because it is never spent:
/// destroying a window is a message to an event loop that is idle by then, and
/// the wait ends the moment it has been processed. It exists so a wedged main
/// thread costs the teardown a bounded delay rather than a hang.
const WINDOW_DESTROY_BUDGET: Duration = Duration::from_secs(2);

/// Set for the whole of [`stop_sidecar_and_exit`], read by [`on_run_event`].
///
/// Process-global rather than carried in state because it describes the
/// process itself — there is exactly one teardown, and the question it answers
/// ("did *we* ask for this exit?") is asked from the event loop, which has no
/// route to anything the teardown thread owns.
static TEARDOWN_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether an `ExitRequested` must be vetoed. Pure, so the one case that
/// matters is a unit test rather than a live window.
///
/// `code` is `None` when the event loop raised it itself — every window gone —
/// and `Some` when this process asked, through `app.exit`. During a teardown
/// the first is [`destroy_windows`]'s own doing and must not end the process:
/// FrankenPHP has not been signalled yet at that point, and letting the event
/// loop exit here would orphan it. Outside a teardown it is left alone, so a
/// splash closed before the sidecar ever existed still ends the process exactly
/// as it does today rather than hanging with nothing on screen.
pub fn veto_exit(code: Option<i32>, teardown_in_flight: bool) -> bool {
    code.is_none() && teardown_in_flight
}

/// The event-loop callback, and the only reason this app needs one: see
/// [`veto_exit`].
pub fn on_run_event(_app: &tauri::AppHandle, event: tauri::RunEvent) {
    if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
        if veto_exit(
            code,
            TEARDOWN_IN_FLIGHT.load(std::sync::atomic::Ordering::SeqCst),
        ) {
            api.prevent_exit();
        }
    }
}

/// Destroy every webview window this process owns, and wait until the event
/// loop confirms they are gone.
///
/// **This is what makes FrankenPHP's shutdown graceful** (plan 014). Caddy's
/// stop drains its connections, the Mercure hub is always mounted, a
/// subscription is a stream, and a stream never drains — so as long as this
/// process's own WebView still holds a connection to the app's port, the
/// server waits for a client that is never going to let go, and dies by
/// SIGKILL every time. `on_window_event` only *hides* the window, deliberately,
/// so the client is alive and connected for the entire teardown unless
/// something takes it away.
///
/// `destroy()`, never `close()`: `close()` fires `CloseRequested`, which lands
/// straight back in [`on_window_event`] and would spawn a second teardown on
/// top of this one. `destroy()` closes without emitting anything (confirmed in
/// `tauri-runtime-wry`: `WindowMessage::Destroy` goes to `on_window_close`,
/// where `WindowMessage::Close` goes through `on_close_requested`).
///
/// It runs on the teardown thread, not the main one — plan 011's constraint,
/// unchanged. `destroy()` only posts a message to the event loop, so the work
/// itself lands on the main thread and this waits for it, which is the shape
/// that plan allows.
fn destroy_windows(app: &tauri::AppHandle) {
    use tauri::Manager;

    for window in app.webview_windows().into_values() {
        let _ = window.destroy();
    }

    let deadline = std::time::Instant::now() + WINDOW_DESTROY_BUDGET;
    while std::time::Instant::now() < deadline {
        if app.webview_windows().is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Not fatal: the server is asked to stop either way, and the escalation is
    // there for exactly this. Said out loud because it means the next line in
    // the log — FrankenPHP taking its full budget — has a cause worth knowing.
    eprintln!(
        "tfsapp-hub: a window did not go away within {}s; the backend may take longer to stop",
        WINDOW_DESTROY_BUDGET.as_secs()
    );
}

/// Stop the sidecar and exit — the one shutdown body, shared by the last window
/// closing and by a signal, so a `SIGTERM` tears the app down exactly the way
/// the user closing it does.
///
/// The order is the whole of plan 014's step 2. The serving claim goes first,
/// so nothing is routed here any more; then the client, so the server has
/// nothing left to drain; only then is the server asked to stop. Between the
/// second and the third, this process has no window and is not yet dead —
/// which is why [`on_run_event`] has to veto the event loop's own attempt to
/// exit in that gap.
fn stop_sidecar_and_exit(app: &tauri::AppHandle) {
    TEARDOWN_IN_FLIGHT.store(true, std::sync::atomic::Ordering::SeqCst);
    release_serving_claim(app);
    destroy_windows(app);
    stop_sidecar(app);
    app.exit(0);
}

/// The window-close handler.
///
/// On the *last* window's close request the default synchronous close is vetoed
/// — otherwise it races the teardown below — the window is hidden at once so the
/// user sees their click land, and the sidecar is stopped off the GTK main
/// thread.
///
/// **That last part matters even now that teardown is fast.** It signals
/// processes and waits on them, and none of those waits has a ceiling worth
/// putting on the thread that paints: a wedged FrankenPHP still costs the
/// SIGTERM-then-SIGKILL escalation, and a client holding a stream still costs
/// Caddy's grace period. On the main thread any of those would freeze a window
/// that is still on screen. The reason survives the number that used to
/// motivate it — plan 014 removed a six-second floor, not the argument for
/// where this work runs.
///
/// The hiding is why teardown has to destroy this window itself: hidden is not
/// closed, and a hidden webview keeps its connection to the backend the
/// teardown is about to stop. See [`destroy_windows`].
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
