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
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tfsapp_core::ports::DataConfig;

use crate::close_guard::{CloseFlow, Resolution, SharedCloseGuards};
use crate::manifest::RunAlias;

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

/// `<data_subdir>/cache.json` — plan 024's cache stamp: what `cache/`/`build/`
/// were last compiled against. Beside `config.json`, following the same
/// temp-file-plus-`rename` write and the same "absent is not an error" read
/// shape as [`read_data_version`]/[`write_data_version`].
pub fn cache_stamp_path(data_subdir: &Path) -> PathBuf {
    data_subdir.join("cache.json")
}

/// The three dimensions that can invalidate a compiled Symfony container
/// (plan 024's Overview): the app's own version, the absolute path of the
/// installed snapshot the container was compiled from — a stable real path
/// for an installed app, unlike the station's random `/tmp/.mount_*` — and
/// the `Platform` fingerprint (`registry.rs`) the PHP that compiled it ran
/// under. A rollback restoring an older tree changes the first, a hub
/// self-update moving PHP changes the third; nothing else does.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CacheStamp {
    pub app_version: String,
    pub snapshot_path: String,
    pub platform: crate::registry::Platform,
}

/// [`read_cache_stamp`]'s answer: never a bare bool, so the caller can log
/// *why* it is about to rebuild rather than rebuilding silently — a launch
/// that silently rebuilds is the failure mode plan 024 exists to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheStatus {
    /// The stamp matches `expected` and `cache/` actually holds something —
    /// the compiled container is safe to reuse.
    Matches,
    /// A stamp exists but does not vouch for what is (or is not) on disk —
    /// named, so `resolve` can say why it is about to wipe.
    Mismatch { reason: String },
    /// No stamp has ever been written for this data dir — the state of every
    /// app installed before this plan, and of the very first launch after a
    /// successful install/update that stamped it (in which case the caller
    /// never reaches this branch mismatched, since the stamp is written
    /// before the launch that would read it).
    Absent,
}

/// Whether `cache_dir` has anything in it at all. A directory that does not
/// exist reads the same as an empty one — both mean "nothing to reuse" — so
/// callers get one answer instead of having to fold two `Result`/`bool`s
/// themselves.
fn cache_dir_has_entries(cache_dir: &Path) -> bool {
    fs::read_dir(cache_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// Compare `data_subdir`'s cache stamp against `expected`, the stamp this
/// launch would write if it rebuilt right now.
///
/// A hand-deleted `cache/` overrides an otherwise-matching stamp
/// (`cache_dir_has_entries` is checked last, precisely so every other branch
/// gets to explain its own mismatch first): a stamp is a claim about what was
/// built, not a promise that it is still on disk, and this function must
/// never tell a caller to reuse a container that is not there.
pub fn read_cache_stamp(
    data_subdir: &Path,
    cache_dir: &Path,
    expected: &CacheStamp,
) -> CacheStatus {
    let path = cache_stamp_path(data_subdir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return CacheStatus::Absent,
        Err(error) => {
            return CacheStatus::Mismatch {
                reason: format!("cannot read {}: {error}", path.display()),
            }
        }
    };
    let stamp: CacheStamp = match serde_json::from_str(&contents) {
        Ok(stamp) => stamp,
        Err(error) => {
            return CacheStatus::Mismatch {
                reason: format!("cannot parse {}: {error}", path.display()),
            }
        }
    };
    if stamp.app_version != expected.app_version {
        return CacheStatus::Mismatch {
            reason: format!(
                "app_version changed ({} -> {})",
                stamp.app_version, expected.app_version
            ),
        };
    }
    if stamp.snapshot_path != expected.snapshot_path {
        return CacheStatus::Mismatch {
            reason: format!(
                "the installed snapshot moved ({} -> {})",
                stamp.snapshot_path, expected.snapshot_path
            ),
        };
    }
    if stamp.platform != expected.platform {
        return CacheStatus::Mismatch {
            reason: format!(
                "the platform changed ({} -> {})",
                stamp.platform, expected.platform
            ),
        };
    }
    if !cache_dir_has_entries(cache_dir) {
        return CacheStatus::Mismatch {
            reason: format!(
                "{} is missing or empty despite a matching stamp",
                cache_dir.display()
            ),
        };
    }
    CacheStatus::Matches
}

/// Write `data_subdir`'s cache stamp, atomic temp-file-plus-`rename` like
/// [`write_data_version`]. Callers own CONTRACT.md §6's "never written
/// speculatively" rule — this only ever runs after the warm-up it describes
/// has actually succeeded.
pub fn write_cache_stamp(data_subdir: &Path, stamp: &CacheStamp) -> Result<(), LifecycleError> {
    let path = cache_stamp_path(data_subdir);
    let json = serde_json::to_string_pretty(stamp).map_err(|error| {
        LifecycleError::MalformedDataConfig {
            path: path.clone(),
            detail: error.to_string(),
        }
    })?;

    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| LifecycleError::Io { path, source }
    };
    let temporary = data_subdir.join("cache.json.tmp");
    fs::write(&temporary, json).map_err(io_error(&temporary))?;
    fs::rename(&temporary, &path).map_err(io_error(&path))
}

/// Discard `data_subdir`'s cache stamp — an already-missing file is not an
/// error, same as [`discard_db_snapshot`]/[`discard_rollback_anchor`].
///
/// `update.rs`'s own revert path calls this alongside its database and tree
/// restores: a stamp written for the version an update was moving *to* must
/// not survive next to a tree reverted back to the version it was moving
/// *from*. Reaching for the field of a mismatched dimension covers most such
/// cases already (`read_cache_stamp`'s `app_version` check), but an update
/// that resyncs the same version onto a different tree is a case where
/// nothing else would catch it — so the revert clears it outright rather than
/// leaning on a comparison that happens to save it most of the time.
pub fn discard_cache_stamp(data_subdir: &Path) -> io::Result<()> {
    match fs::remove_file(cache_stamp_path(data_subdir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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
#[allow(dead_code)] // Transaction-private snapshots supersede this public-anchor helper.
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

/// An I/O failure while atomically creating a rescue copy.
#[derive(Debug)]
pub struct RescueDumpError {
    pub path: PathBuf,
    pub source: io::Error,
}

/// The unreserved rescue filename pattern named before confirmation.
pub fn rescue_dump_pattern(data_subdir: &Path, name: &str) -> PathBuf {
    data_subdir.join(format!("{name}.rescue-<timestamp>[-N]"))
}

fn rescue_dump_base(data_subdir: &Path, name: &str) -> PathBuf {
    let format =
        time::format_description::parse_borrowed::<2>("[year][month][day]T[hour][minute][second]Z")
            .expect("the fixed rescue timestamp format is valid");
    let timestamp = time::OffsetDateTime::now_utc()
        .format(&format)
        .expect("UTC always fits the fixed rescue timestamp format");
    data_subdir.join(format!("{name}.rescue-{timestamp}"))
}

/// Copy one current SQLite member to a newly-created rescue file. The
/// exclusive create is the reservation: a concurrently claimed candidate is
/// retried with its numeric suffix, never overwritten after an `exists` check.
pub fn copy_rescue_dump(data_subdir: &Path, name: &str) -> Result<PathBuf, RescueDumpError> {
    let source_path = data_subdir.join(name);
    let base = rescue_dump_base(data_subdir, name);
    copy_rescue_dump_at(&source_path, &base)
}

fn copy_rescue_dump_at(source_path: &Path, base: &Path) -> Result<PathBuf, RescueDumpError> {
    for suffix in 1_u32.. {
        let candidate = match suffix {
            1 => base.to_path_buf(),
            _ => base.with_file_name(format!(
                "{}-{suffix}",
                base.file_name().unwrap().to_string_lossy()
            )),
        };
        let mut rescue = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(RescueDumpError {
                    path: candidate,
                    source,
                })
            }
        };
        let mut source = fs::File::open(source_path).map_err(|source| RescueDumpError {
            path: source_path.to_path_buf(),
            source,
        })?;
        io::copy(&mut source, &mut rescue).map_err(|source| RescueDumpError {
            path: candidate.clone(),
            source,
        })?;
        return Ok(candidate);
    }
    unreachable!("a u32 suffix range never ends")
}

/// Rename `data_dir`/`name` aside to `<name>.rescue-<timestamp>`, the same
/// naming and collision-avoidance rule [`copy_rescue_dump`] uses — a rename
/// rather than a copy, for a caller whose rescued thing is a directory (or
/// otherwise too large to double), where `import`'s own reason to copy
/// rather than move the database (`remove_live_db_files` unlinks it right
/// after) does not apply (plan 049 / decision 006).
///
/// The exclusive check here is not atomic against a concurrent claimant the
/// way [`copy_rescue_dump`]'s `create_new` is — `rename(2)` has no equivalent
/// reservation for a target that must not already exist — but the caller
/// holds the maintenance lease for the whole pipeline, so a to-the-second
/// collision between two of *its own* rescues is the only case this loop
/// exists to survive.
pub fn move_rescue_dump_dir(data_dir: &Path, name: &str) -> Result<PathBuf, RescueDumpError> {
    let source_path = data_dir.join(name);
    let base = rescue_dump_base(data_dir, name);
    move_rescue_dump_dir_at(&source_path, &base)
}

fn move_rescue_dump_dir_at(source_path: &Path, base: &Path) -> Result<PathBuf, RescueDumpError> {
    for suffix in 1_u32.. {
        let candidate = match suffix {
            1 => base.to_path_buf(),
            _ => base.with_file_name(format!(
                "{}-{suffix}",
                base.file_name().unwrap().to_string_lossy()
            )),
        };
        if candidate.exists() {
            continue;
        }
        return fs::rename(source_path, &candidate)
            .map(|()| candidate.clone())
            .map_err(|source| RescueDumpError {
                path: candidate,
                source,
            });
    }
    unreachable!("a u32 suffix range never ends")
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
    run_aliases: &BTreeMap<String, RunAlias>,
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

    // Rule 3's launch-side refusal (plan 013, widened by plan 047,
    // `../decision/005-concurrency-belongs-to-the-alias.md` point 3,
    // CONTRACT.md §6's "Running a declared command"): a `run` command
    // holding a `runs/` entry for this app owns its data dir just as much as
    // a live window does, so a window must not open over a non-`concurrent`
    // one, or over any of them when this launch has an `event` to run — the
    // launch path is the only one that can show progress. Placed here, after
    // `acquire_launch_locks` has already decided to launch rather than hand
    // off: a launch that hands off to a live sibling (`LaunchDecision::HandOff`,
    // `locks.as_ref()?` above) must never reach this check, since it is the
    // ordinary "second window on an app that is already up" case and a
    // `concurrent` alias legitimately running beside that window would
    // otherwise be blocked by it.
    check_run_lock(
        id,
        data_dir,
        identifier,
        run_aliases,
        event != LifecycleEvent::None,
    );

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
/// Nor does the rule 3 `runs/` probe (plan 013, widened by plan 047) reach
/// here: a dev session's `runs/` would live under its own project's `var/`,
/// not under `data_dir`, and `run <id> <alias>` takes a hub-local `id` a dev
/// session never has — nothing in the hub ever writes one for it.
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
/// `Held { active }` means [`crate::run::scan_runs`] found at least one live
/// launcher or active orphan — never empty when this variant is constructed.
///
/// `pub(crate)`, not private: plan 016's `install::check_data_dir_available`
/// reads this same scan before writing into a data directory, and it must be
/// the one reader of what "held" means — never a second probe with its own,
/// possibly diverging, idea of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunLockHeld {
    Free,
    Held {
        active: Vec<crate::run::ActiveRunEntry>,
    },
}

/// Probe `<data_dir>/runs/` (plan 013's rule 3, widened by plan 047,
/// CONTRACT.md §6): the pure*-ish, `Result`-returning half of
/// [`check_run_lock`], kept apart from it exactly as [`acquire_launch_locks`]
/// is kept apart from [`prepare_launch`] — so a held entry, a free directory,
/// and an unreadable record are each a unit test with no process willing to
/// exit under it. (*[`crate::run::scan_runs`] also unlinks stale entries as
/// it reads them — the one side effect this probe is not otherwise free of.)
pub(crate) fn probe_run_lock(data_dir: &Path, identifier: &str) -> std::io::Result<RunLockHeld> {
    let active = crate::run::scan_runs(data_dir, identifier)?;
    Ok(if active.is_empty() {
        RunLockHeld::Free
    } else {
        RunLockHeld::Held { active }
    })
}

/// Who is already using a data directory — a live app window, or one or more
/// active `run` commands. Returned by [`data_dir_holder`]; `None` there means
/// free.
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
    /// One or more active `run` commands hold `runs/` entries (rule 3).
    /// `active` is whatever [`probe_run_lock`] found — never empty.
    RunCommand {
        active: Vec<crate::run::ActiveRunEntry>,
    },
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
pub fn data_dir_holder(data_dir: &Path, identifier: &str) -> io::Result<Option<DataDirHolder>> {
    let pid_file = data_dir.join("sidecar.pid");
    if tfsapp_core::process::is_owner_live(&pid_file)? {
        return Ok(Some(DataDirHolder::Window));
    }

    match probe_run_lock(data_dir, identifier)? {
        RunLockHeld::Free => Ok(None),
        RunLockHeld::Held { active } => Ok(Some(DataDirHolder::RunCommand { active })),
    }
}

/// Rule 3's launch-side gate (plan 013, widened by plan 047,
/// `../decision/005-concurrency-belongs-to-the-alias.md` point 3,
/// CONTRACT.md §6): resolve the scan against the manifest exactly as
/// `run::start`'s own guard does ([`crate::run::resolve_active_runs`]), then
/// let [`crate::run::launch_verdict`] decide — a launch with an `event` to
/// run refuses over any active command, one with nothing to run refuses only
/// over a non-`concurrent` one, naming it and pointing at the way to release
/// it.
fn check_run_lock(
    id: &str,
    data_dir: &Path,
    identifier: &str,
    run_aliases: &BTreeMap<String, RunAlias>,
    has_lifecycle_event: bool,
) {
    let active = match crate::run::scan_runs(data_dir, identifier) {
        Ok(active) => active,
        Err(error) => fatal_startup_error(&format!(
            "cannot probe {}: {error}",
            data_dir.join("runs").display()
        )),
    };
    let active_runs = crate::run::resolve_active_runs(active, run_aliases);
    if let crate::run::LaunchVerdict::Refuse { blocker } =
        crate::run::launch_verdict(has_lifecycle_event, &active_runs)
    {
        let pid = blocker
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        fatal_startup_error(&format!(
            "{id} cannot open a window while its \"{}\" run command is active (pid {pid}) — stop \
             it first with `tfsapp-hub run --stop {id}`.",
            blocker.alias
        ));
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
        use tauri::Manager;

        // Commit shutdown first, exactly as a signal does: a fatal error
        // invalidates any pending close approval, refuses new guards, and
        // never lets a dialog's late answer authorise an effect against a
        // launch that is already failing. The dialog and the exit code below
        // stay this path's own — error reporting and exit behavior are
        // unchanged.
        if let Some(state) = app.try_state::<SharedCloseGuards>() {
            state.commit_shutdown();
        }
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
fn stop_sidecar<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
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
fn release_serving_claim<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
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

/// Claim the one teardown this process ever runs. A conditional take, not a
/// plain store: an approved final close, a termination signal and a repeated
/// close after commitment can all reach [`stop_sidecar_and_exit`] — the
/// state commitment they share is idempotent, but the teardown itself must
/// run exactly once, so the first arrival to flip this latch owns it and
/// every other one returns at the claim.
fn claim_teardown(latch: &std::sync::atomic::AtomicBool) -> bool {
    latch
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

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
fn destroy_windows<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
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
fn stop_sidecar_and_exit<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Manager;

    // Ownership before anything: exactly one caller — an approved final
    // close, a termination signal, a repeated close after commitment — runs
    // the teardown; every other arrival returns at this line. The state
    // commitment below is deliberately separate from this claim: a close
    // precommitting shutdown inside `resolve_close` must not suppress its
    // own cleanup by finding the commitment already set.
    if !claim_teardown(&TEARDOWN_IN_FLIGHT) {
        return;
    }

    // Shutdown commitment — idempotent, and already done when an approved
    // final close or another teardown path got here first — ahead of every
    // teardown step and on every path that reaches here. From this line,
    // guard operations answer `closing` and a confirmation dialog still open
    // when a signal arrived goes stale: its late answer can authorise
    // nothing. A mandatory shutdown never waits on a person.
    if let Some(state) = app.try_state::<SharedCloseGuards>() {
        state.commit_shutdown();
    }
    release_serving_claim(app);
    destroy_windows(app);
    stop_sidecar(app);
    app.exit(0);
}

/// The window and process facts the close coordinator needs, and the effects
/// it applies — the injectable boundary between the close-decision machinery
/// and the runtime. Production answers with [`TauriCloseWorld`], backed by
/// the `AppHandle`; the coordinator's regressions drive it with a recording
/// world, so interleavings assert *effects and ownership*, not enum
/// mappings, with the destruction observed exactly when the test says so.
///
/// Approved secondary closes post a native close request. The state already
/// reserves that window, so the resulting `CloseRequested` proceeds without
/// another dialog, even if the document reloaded after approval.
pub trait CloseWorld {
    /// Every window this process currently has, including ones whose close
    /// is committed but whose destruction is not observed yet.
    fn window_labels(&self) -> Vec<String>;
    /// Whether a shared backend exists that a last-window close would stop.
    fn has_backend(&self) -> bool;
    /// Hide a window whose close was approved as the backend-stopping one.
    fn hide(&mut self, window: &str);
    /// Post the approved window's native close. `CloseRequested` observes
    /// the existing commitment. Return `false` if posting failed, so the
    /// window's reservation can be released.
    fn request_close(&mut self, window: &str) -> bool;
    /// Start the orderly teardown off the calling thread.
    fn start_teardown(&mut self);
    /// Open the one native confirmation for a close that needs asking.
    fn open_confirmation(
        &mut self,
        window: &str,
        token: &str,
        frontend: &BTreeSet<String>,
        backend: &BTreeSet<String>,
    );
}

/// The topology rule every close decision runs on: closing `window` stops
/// the shared backend when one exists to stop and no other window survives
/// this close — and a window whose close is committed but whose destruction
/// is not observed yet is *not* a survivor. That last clause is what makes
/// two nearly-simultaneous closes unable to both conclude that the other
/// window will preserve the backend (audit 016, finding 1).
fn close_stops_backend_among(
    labels: &[String],
    committed: &BTreeSet<String>,
    has_backend: bool,
    window: &str,
) -> bool {
    has_backend
        && !labels
            .iter()
            .any(|label| label != window && !committed.contains(label))
}

/// [`close_stops_backend_among`] over a [`CloseWorld`] and the guard state.
fn close_stops_backend_in<W: CloseWorld>(
    world: &W,
    state: &crate::close_guard::SharedCloseGuards,
    window: &str,
) -> bool {
    close_stops_backend_among(
        &world.window_labels(),
        &state.committed_closing_windows(),
        world.has_backend(),
        window,
    )
}

/// The close-request half of the coordinator: decide, commit, and dispatch
/// the first effects — vetoed closes open their confirmation, unguarded
/// backend-stopping closes hide and start the teardown, everything else is
/// left to the default close. Runs on the event loop in production, so the
/// topology it reads is serialized with every other window event.
fn handle_close_request<W: CloseWorld>(
    state: &crate::close_guard::SharedCloseGuards,
    world: &mut W,
    window: &str,
) -> CloseAction {
    let stops = close_stops_backend_in(world, state, window);
    let flow = state.begin_close(window, stops);
    let action = close_action(&flow, stops);
    if let CloseFlow::Confirm {
        token,
        frontend,
        backend,
    } = &flow
    {
        world.open_confirmation(window, token, frontend, backend);
    }
    if action == CloseAction::HideAndTearDown {
        world.hide(window);
        world.start_teardown();
    }
    action
}

/// The dialog-answer half of the coordinator, on the event loop: revalidate
/// the topology, the guards and the document incarnation, commit the close
/// or refuse it, and dispatch the effect — post the approved secondary
/// window's native close request (releasing the reservation when even the
/// request could not be posted), hide and tear down an approved final one,
/// or ask again when the warning no longer described what was at stake.
///
/// Document replacement invalidates a pending answer. After commitment,
/// closing the window is final; a later document load does not cancel it.
fn handle_close_answer<W: CloseWorld>(
    state: &crate::close_guard::SharedCloseGuards,
    world: &mut W,
    window: &str,
    token: &str,
    approved: bool,
) {
    let stops = close_stops_backend_in(world, state, window);
    match close_effect(state.resolve_close(token, approved, stops)) {
        CloseEffect::Nothing => {}
        CloseEffect::CloseWindow => post_approved_close(state, world, window),
        CloseEffect::HideAndTearDown => {
            world.hide(window);
            world.start_teardown();
        }
        CloseEffect::AskAgain => {
            // The topology is re-read after the failed recheck: the answer
            // landed on the event loop, and whatever changed since the
            // question was asked is the reason it is being asked again.
            let stops_now = close_stops_backend_in(world, state, window);
            match state.begin_close(window, stops_now) {
                // The person already approved this close, and everything the
                // warning covered has since vanished: it closes cleanly, with
                // the effect the current topology calls for.
                CloseFlow::Allow => {
                    if stops_now {
                        world.hide(window);
                        world.start_teardown();
                    } else {
                        post_approved_close(state, world, window);
                    }
                }
                // Another window's decision took the one slot while this
                // dialog stood. This close stays open with its guards; the
                // person can close it again once that dialog resolves.
                CloseFlow::Busy => {}
                CloseFlow::Confirm {
                    token,
                    frontend,
                    backend,
                } => world.open_confirmation(window, &token, &frontend, &backend),
            }
        }
    }
}

/// Recover the reservation if posting failed and the window remains usable.
fn post_approved_close<W: CloseWorld>(
    state: &crate::close_guard::SharedCloseGuards,
    world: &mut W,
    window: &str,
) {
    if state.committed_closing_windows().contains(window) && !world.request_close(window) {
        state.release_close(window);
    }
}

/// The window-close handler.
///
/// Guards get the first say, then today's behavior: on a close that would
/// stop the shared backend the default close is vetoed — otherwise it races
/// the teardown below — the window is hidden at once so the user sees their
/// click land, and the sidecar is stopped off the GTK main thread.
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
/// to the app, not to any one of its windows — and the guards of another,
/// surviving window never make a clean secondary window prompt. This handler
/// runs on the event loop, so the topology every decision reads is serialized
/// with the other windows' close requests, their destruction, and a
/// second-instance arrival.
pub fn on_window_event<R: tauri::Runtime>(window: &tauri::Window<R>, event: &tauri::WindowEvent) {
    use tauri::Manager;

    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
        let app = window.app_handle();
        match app.try_state::<SharedCloseGuards>() {
            Some(state) => {
                let mut world = TauriCloseWorld { app };
                let action = handle_close_request(&state, &mut world, window.label());
                if action != CloseAction::Default {
                    api.prevent_close();
                }
            }
            // A launch always manages a guard state before its first window,
            // so this arm is the shape no real launch has — the pre-055
            // behavior, kept for the same reason `begin_close` needs a
            // guard-free answer: teardown must never depend on state being
            // present.
            None => {
                if close_stops_backend(app, window.label()) {
                    api.prevent_close();
                    let _ = window.hide();
                    let app = app.clone();
                    std::thread::spawn(move || stop_sidecar_and_exit(&app));
                }
            }
        }
    }

    // Focus recency for the open-files target selector: the most recently
    // focused eligible window is the one a file-bearing arrival is delivered
    // to, and this is the only place the hub learns focus order. Recorded for
    // every window, whatever the app declared — focus is a window fact, and
    // the state stays inert until an enqueue happens.
    if let tauri::WindowEvent::Focused(true) = event {
        if let Some(state) = window
            .app_handle()
            .try_state::<crate::open_files::SharedOpenFiles>()
        {
            state.note_focused(window.label());
        }
    }

    // A destroyed window's frontend guards cannot survive it: the document
    // they belong to is gone, and a later window reusing the label
    // (`main`-`N` gap-filling) is a new owner by the plan's own rule. The
    // backend namespace is untouched — it belongs to the app instance, not to
    // a window. This runs for every destruction path, including teardown's
    // own `destroy()`, and it is also what releases the window's close
    // reservation: the destruction has now been observed.
    if let tauri::WindowEvent::Destroyed = event {
        let app = window.app_handle();
        if let Some(state) = app.try_state::<crate::close_guard::SharedCloseGuards>() {
            state.drop_window(window.label());
        }

        // Its unacknowledged file requests transfer to a surviving eligible
        // window, which is then notified; with no eligible survivor — or a
        // committed shutdown, where no window will consume anything again —
        // they die with the window. Never a committed-close window: its
        // namespace closed when its close committed.
        if let Some(open_files) = app.try_state::<crate::open_files::SharedOpenFiles>() {
            let shutting_down = app
                .try_state::<crate::close_guard::SharedCloseGuards>()
                .is_some_and(|guards| guards.is_closing());
            let survivor = if shutting_down {
                None
            } else {
                crate::open_files::select_target_window(app, Some(window.label()))
            };
            match survivor {
                Some(survivor) => {
                    let moved = open_files.reassign(window.label(), &survivor);
                    if moved > 0 {
                        crate::open_files::notify(app, &survivor);
                        crate::open_files::present_target_window(app, &survivor);
                    }
                }
                None => open_files.drop_window(window.label()),
            }
        }
    }
}

/// The [`CloseWorld`] production impl, backed by the app handle.
struct TauriCloseWorld<'a, R: tauri::Runtime> {
    app: &'a tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> CloseWorld for TauriCloseWorld<'_, R> {
    fn window_labels(&self) -> Vec<String> {
        use tauri::Manager;
        self.app.webview_windows().into_keys().collect()
    }

    fn has_backend(&self) -> bool {
        use tauri::Manager;
        self.app
            .try_state::<std::sync::Mutex<crate::sidecar::Sidecar>>()
            .is_some()
    }

    fn hide(&mut self, window: &str) {
        use tauri::Manager;
        if let Some(webview) = self.app.get_webview_window(window) {
            let _ = webview.hide();
        }
    }

    fn request_close(&mut self, window: &str) -> bool {
        use tauri::Manager;
        // An absent window is not a failed request: it is already gone,
        // and its `Destroyed` event has already released the reservation.
        match self.app.get_webview_window(window) {
            // The handle targets this native window; `begin_close` lets its
            // committed close proceed without rechecking document identity.
            Some(webview) => webview.close().is_ok(),
            None => true,
        }
    }

    fn start_teardown(&mut self) {
        let app = self.app.clone();
        std::thread::spawn(move || stop_sidecar_and_exit(&app));
    }

    fn open_confirmation(
        &mut self,
        window: &str,
        token: &str,
        frontend: &BTreeSet<String>,
        backend: &BTreeSet<String>,
    ) {
        open_close_confirmation(
            self.app,
            window,
            token.to_string(),
            frontend.clone(),
            backend.clone(),
        );
    }
}

/// Whether closing `window` right now would stop the shared backend: it is
/// the last window this process has, and there is a sidecar to stop. Asked
/// fresh at every decision point — the closing window is still open at this
/// point, so its own label is excluded — because a window opened or closed
/// between two moments is honoured, never assumed away. A window whose close
/// is committed but whose destruction is not observed yet is counted as
/// closing, never as a survivor.
fn close_stops_backend<R: tauri::Runtime>(app: &tauri::AppHandle<R>, window: &str) -> bool {
    use tauri::Manager;

    let committed = app
        .try_state::<SharedCloseGuards>()
        .map(|state| state.committed_closing_windows())
        .unwrap_or_default();
    close_stops_backend_among(
        &app.webview_windows().into_keys().collect::<Vec<_>>(),
        &committed,
        app.try_state::<std::sync::Mutex<crate::sidecar::Sidecar>>()
            .is_some(),
        window,
    )
}

/// What `on_window_event` does about the default close, before any dialog is
/// opened. Pure, so the race rules are unit tests rather than live windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Let GTK close the window as it asked to.
    Default,
    /// Veto the close and keep the window open — a decision is already
    /// pending for another window, or a confirmation dialog is now open for
    /// this one. Repeated clicks land here, which is what keeps them from
    /// stacking dialogs.
    KeepOpen,
    /// The last window with a sidecar: hide it and tear down off-thread.
    HideAndTearDown,
}

/// The mapping from a guard decision to what happens to the default close.
fn close_action(flow: &CloseFlow, last_window_with_sidecar: bool) -> CloseAction {
    match flow {
        // `Busy`: one decision at a time across the app's windows, or two
        // nearly-simultaneous closes would both conclude they are the last.
        CloseFlow::Busy | CloseFlow::Confirm { .. } => CloseAction::KeepOpen,
        CloseFlow::Allow => {
            if last_window_with_sidecar {
                CloseAction::HideAndTearDown
            } else {
                CloseAction::Default
            }
        }
    }
}

/// What a confirmation dialog's answer actually does. The dialog's side
/// effects (hide, destroy, teardown) run in [`apply_close_answer`]; this is
/// the decision half, pure so the effect rules are unit tests too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseEffect {
    /// Nothing happens: refused, dismissed, stale, or the window it asked
    /// about is already gone. The window stays open, the guards stay
    /// standing, the backend keeps serving.
    Nothing,
    /// Close this window once; the backend and its workers keep running.
    CloseWindow,
    /// Hide this window and start the existing orderly teardown off-thread.
    HideAndTearDown,
    /// The warning no longer described what is at stake — a new relevant guard
    /// appeared, or this close became the backend-stopping one: ask again
    /// before anything closes.
    AskAgain,
}

/// The mapping from a resolved decision to its effect.
fn close_effect(resolution: Resolution) -> CloseEffect {
    match resolution {
        // `stop_backend` is the topology at answer time, so an approval can
        // never stop a backend that a newly-opened window now keeps alive,
        // and a secondary close can never take the backend down with it.
        Resolution::Approved { stop_backend: true } => CloseEffect::HideAndTearDown,
        Resolution::Approved {
            stop_backend: false,
        } => CloseEffect::CloseWindow,
        Resolution::Cancelled | Resolution::Stale | Resolution::Invalidated => CloseEffect::Nothing,
        Resolution::NeedsFreshDecision => CloseEffect::AskAgain,
    }
}

/// The confirmation dialog's title and body — one distinct text per situation
/// the contract names, so a person is told what is actually at stake. The
/// guard IDs themselves never appear: they are app-chosen and may name
/// documents or jobs.
fn close_warning(
    frontend: &BTreeSet<String>,
    backend: &BTreeSet<String>,
) -> (&'static str, String) {
    match (frontend.is_empty(), backend.is_empty()) {
        (false, true) => (
            "Unsaved changes",
            "This window has work that has not been saved yet.\n\nClose it anyway?".to_string(),
        ),
        (true, false) => (
            "Background work",
            "Background work is still running in this app. Closing this \
             window will stop the app and that work.\n\nClose it anyway?"
                .to_string(),
        ),
        (false, false) => (
            "Unsaved changes and background work",
            "This window has work that has not been saved yet, and \
             background work is still running in this app. Closing this \
             window will stop the app and that work.\n\nClose it anyway?"
                .to_string(),
        ),
        // `begin_close` never opens a decision with nothing at stake; if it
        // ever did, asking is still the safe answer.
        (true, true) => (
            "Unsaved changes",
            "This window has work that has not been saved yet.\n\nClose it anyway?".to_string(),
        ),
    }
}

/// Open the native confirmation for one close decision, parented to the
/// window whose close is at stake. Returns immediately: `show` with a
/// callback, never `blocking_show`, because the caller is the GTK main
/// thread and blocking here would deadlock before the dialog could ever
/// paint — the same constraint `fatal_post_setup_error` documents.
///
/// The window whose close the decision covers is still open — the close was
/// vetoed, not performed — so it can parent the dialog. If it is somehow gone
/// already, the answer will be `Invalidated` and there is nothing left to
/// ask about.
fn open_close_confirmation<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    window: &str,
    token: String,
    frontend: BTreeSet<String>,
    backend: BTreeSet<String>,
) {
    use tauri::Manager;
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let Some(webview) = app.get_webview_window(window) else {
        // No parent, no dialog — and no slot left occupied behind a decision
        // nobody can answer: the window's destruction already marked the
        // pending decision destroyed, so this resolve frees it with the
        // honest `Invalidated` (or finds a signal already did the same).
        if let Some(state) = app.try_state::<SharedCloseGuards>() {
            let _ = state.resolve_close(&token, false, false);
        }
        return;
    };
    let (title, message) = close_warning(&frontend, &backend);
    let app = app.clone();
    let window = window.to_string();
    app.dialog()
        .message(message)
        .title(title)
        .parent(&webview)
        .kind(MessageDialogKind::Warning)
        // Cancel is the safe default: Escape and the window-manager's
        // dismiss answer `false`, and `false` never closes anything.
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Close".to_string(),
            "Cancel".to_string(),
        ))
        .show(move |approved| apply_close_answer(&app, &window, &token, approved));
}

/// Apply a confirmation dialog's answer — the one close-guard function the
/// dialog thread calls. It performs no coordination itself: it hops the
/// answer to the event loop, where [`handle_close_answer`] reads the final
/// topology serialized with every other window event — a neighbouring
/// `CloseRequested`, a `Destroyed`, a second-instance window — commits the
/// close, and dispatches the effect. Nothing here blocks on GTK.
fn apply_close_answer<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    window: &str,
    token: &str,
    approved: bool,
) {
    use tauri::Manager;

    let window = window.to_string();
    let token = token.to_string();
    let app_after_schedule = app.clone();
    let scheduled = {
        let app = app.clone();
        let token = token.clone();
        app.clone().run_on_main_thread(move || {
            let Some(state) = app.try_state::<SharedCloseGuards>() else {
                return;
            };
            let mut world = TauriCloseWorld { app: &app };
            handle_close_answer(&state, &mut world, &window, &token, approved);
        })
    };
    if scheduled.is_err() {
        // The event loop is gone — this process is on its way out — but the
        // one pending slot must not stay occupied behind a dead callback:
        // resolve as refused, which frees it without committing anything.
        if let Some(state) = app_after_schedule.try_state::<SharedCloseGuards>() {
            let _ = state.resolve_close(&token, false, false);
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
