//! `tfsapp-hub rollback <id> [--yes]` — undo the last successful update (the
//! per-app update and rollback plan, `000-index.md`).
//!
//! The mirror image of `update <id>`'s `Apply` action, and deliberately not
//! symmetric with it: an update reverts itself on failure and leaves the
//! anchor untouched on success, because there is a next launch to protect
//! either way. A rollback has no "next attempt" of its own to protect — it
//! *is* the undo — so, on success, it consumes the anchor rather than
//! rotating it (the plan's Overview: a rollback is one step back, and a
//! `.previous` naming the version just left would invite a "rollback
//! forward" this plan does not define).
//!
//! It is, however, resumable rather than revertible (plan 064): a durable
//! [`RollbackMarker`] is written after the rescue dump and before the first
//! mutation, and every step after it tolerates being run again, so a
//! process killed partway through is finished — never rewound — by simply
//! running `rollback <id>` again. The tree a rollback replaces is already
//! deleted at that point; there is nothing to go back to.
//!
//! The one thing it does not throw away is the database it is about to
//! replace: [`lifecycle::copy_rescue_dump`] carries it forward as a
//! manual-recovery artefact, named on screen, so "I rolled back too eagerly"
//! never costs data that was never wrong.

use std::{fmt, fs, io, path::Path, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    install::{self, InstallError},
    lifecycle::{self, Anchor, LifecycleError},
    lifecycle_gate::{self, GateError},
    paths::{Paths, PathsError},
    prompt,
    registry::{self, RegistryEntry, RegistryError, Source},
    update::restore_tree,
    update_transaction::{self, JournalError},
};

/// The rollback marker's own format version, checked by [`read_marker`] the
/// way the update journal checks its own.
pub(crate) const MARKER_FORMAT_VERSION: u32 = 1;
const MARKER_FILE: &str = "rollback-transaction.json";

/// The durable record a rollback writes after its rescue dump and before
/// its first mutation — the one thing that turns "a kill between two steps
/// leaves a half-rolled-back installation nobody can name" into "the next
/// `rollback <id>` finds the marker and finishes". Every step after the
/// marker tolerates being run again, so a resume never rewinds: the tree it
/// replaces is already gone by then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RollbackMarker {
    pub format_version: u32,
    /// The version the rollback restores — `rollback.json`'s own
    /// `app_version`, recorded here because the anchor is partly consumed by
    /// design by the time a resume reads this marker.
    pub target_version: String,
    /// The source revision the registry entry goes back to.
    pub source_revision: String,
    /// The source the registry entry goes back to — the anchor's own, copied
    /// here for the same reason as `target_version`. Optional with a serde
    /// default so a marker left by the previous binary still reads, and so an
    /// anchor that predates source tracking leaves the recorded source alone;
    /// `MARKER_FORMAT_VERSION` stays 1 for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// `app.db`'s own rescue dump, the path named on screen — `None` when
    /// there was no live database to save.
    pub rescue_path: Option<PathBuf>,
}

pub(crate) fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_FILE)
}

pub(crate) fn write_marker(data_dir: &Path, marker: &RollbackMarker) -> Result<(), JournalError> {
    update_transaction::write_record(data_dir, MARKER_FILE, marker)
}

/// Read the marker, rejecting an unknown `format_version` as malformed — the
/// same stance as the update journal's own read.
pub(crate) fn read_marker(data_dir: &Path) -> Result<Option<RollbackMarker>, JournalError> {
    let Some(marker): Option<RollbackMarker> =
        update_transaction::read_record(data_dir, MARKER_FILE)?
    else {
        return Ok(None);
    };
    if marker.format_version != MARKER_FORMAT_VERSION {
        return Err(JournalError::Malformed {
            path: marker_path(data_dir),
            detail: format!(
                "unsupported transaction format version {}",
                marker.format_version
            ),
        });
    }
    Ok(Some(marker))
}

fn remove_marker(data_dir: &Path) -> io::Result<()> {
    fs::remove_file(marker_path(data_dir))?;
    fs::File::open(data_dir)?.sync_all()
}

/// The deterministic kill stand-in, shared with `update`'s own stop points so
/// the whole hub has one thread-local for the technique.
#[cfg(test)]
fn stop_at(point: &'static str) -> io::Result<()> {
    if crate::update::test_stop::hit(point) {
        return Err(io::Error::other(format!("test stop point: {point}")));
    }
    Ok(())
}

#[cfg(not(test))]
fn stop_at(_point: &'static str) -> io::Result<()> {
    Ok(())
}

/// The whole command: roll `id` back, or say why not. Returns the process's
/// exit code.
pub fn run(id: &str, assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match rollback(&paths, id, assume_yes) {
        Ok(true) => EXIT_OK,
        // Declining is not a failure of the command, but nothing changed
        // either — a script reading 0 would conclude it did.
        Ok(false) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The pipeline, in order (the plan's step 5): load the registry entry, read
/// its anchor, guard the data directory, confirm, rescue-dump the current
/// database, restore the snapshot, swap the trees back, restore the version
/// record and the registry entry, rewrite the desktop entry, then consume
/// the anchor. `false` means the user declined.
///
/// Takes its `Paths` rather than resolving them, matching `update`'s own
/// pipeline — what lets it run against a throwaway root in a test.
fn rollback(paths: &Paths, id: &str, assume_yes: bool) -> Result<bool, RollbackError> {
    // Discover the identifier, then acquire before trusting any mutable
    // registry or anchor state.
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| RollbackError::NotInstalled { id: id.to_string() })?
        .clone();
    let _maintenance = lifecycle_gate::acquire_maintenance(paths, &entry.identifier, "rollback")?;
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| RollbackError::NotInstalled { id: id.to_string() })?
        .clone();

    let app_dir = paths.app_dir(id)?;
    let data_dir = paths.app_data_dir(&entry.identifier)?;
    let data_subdir = data_dir.join("data");

    // A marker means an earlier rollback of this same app was killed partway
    // through. There is nothing to go back to — the tree a rollback replaces
    // is already deleted — so the only way out is forward: finish it.
    if let Some(marker) = read_marker(&data_dir).map_err(marker_error)? {
        install::check_data_dir_available(id, &entry.identifier, &data_dir)?;
        println!(
            "Resuming the interrupted rollback of {id} to {}.",
            marker.target_version
        );
        finish(
            paths,
            id,
            &entry,
            &app_dir,
            &data_dir,
            &data_subdir,
            &marker,
        )?;
        report_success(id, &marker.target_version, marker.rescue_path.as_deref());
        return Ok(true);
    }

    let (target_version, source_revision, source) =
        match lifecycle::anchor_state(&data_subdir, &app_dir) {
            Anchor::Complete {
                app_version,
                source_revision,
                source,
                ..
            } => (app_version, source_revision, source),
            Anchor::Missing => {
                return Err(RollbackError::NoAnchor {
                    id: id.to_string(),
                    missing: missing_halves(&data_subdir, &app_dir),
                })
            }
        };

    install::check_data_dir_available(id, &entry.identifier, &data_dir)?;

    announce(id, &entry, &target_version, source.as_ref());
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(false);
    }

    let rescue_path = rescue_dump(&data_subdir)?;

    // The marker: written after the rescue dump and before the first
    // mutation. A kill before this point has changed nothing live; from here
    // on, a kill is answered by running this command again.
    let marker = RollbackMarker {
        format_version: MARKER_FORMAT_VERSION,
        target_version: target_version.clone(),
        source_revision: source_revision.clone(),
        source,
        rescue_path: rescue_path.clone(),
    };
    write_marker(&data_dir, &marker).map_err(marker_error)?;
    stop_at("rollback_marked").map_err(|source| RollbackError::Unfinished {
        id: id.to_string(),
        detail: source.to_string(),
    })?;

    finish(
        paths,
        id,
        &entry,
        &app_dir,
        &data_dir,
        &data_subdir,
        &marker,
    )?;
    report_success(id, &target_version, rescue_path.as_deref());

    Ok(true)
}

fn report_success(id: &str, target_version: &str, rescue_path: Option<&Path>) {
    if let Some(rescue_path) = rescue_path {
        println!(
            "The database from before the rollback was saved to {}.",
            rescue_path.display()
        );
    }
    println!("Rolled back {id} to {target_version}.");
}

fn marker_error(source: JournalError) -> RollbackError {
    RollbackError::Io {
        path: source_marker_path(&source),
        source: io::Error::other(source.to_string()),
    }
}

/// The path the `JournalError` names, or the marker's own path when it does
/// not carry one — purely so the `Io` shape the rest of this module uses can
/// print something actionable.
fn source_marker_path(source: &JournalError) -> PathBuf {
    match source {
        JournalError::Io { path, .. } | JournalError::Malformed { path, .. } => path.clone(),
    }
}

/// Everything after the marker, in an order where every step tolerates being
/// run again — the first run and a resume after a kill at any boundary both
/// end in the same state. Never rewinds: the caller has already checked the
/// marker is present, and the tree a rollback replaces is gone by then.
///
/// Any failure is [`RollbackError::Unfinished`]: the marker stays, and
/// running `rollback <id>` again finishes.
fn finish(
    paths: &Paths,
    id: &str,
    entry: &RegistryEntry,
    app_dir: &Path,
    data_dir: &Path,
    data_subdir: &Path,
    marker: &RollbackMarker,
) -> Result<(), RollbackError> {
    let unfinished = |detail: String| RollbackError::Unfinished {
        id: id.to_string(),
        detail,
    };
    let step = |what: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source: io::Error| RollbackError::Unfinished {
            id: id.to_string(),
            detail: format!("{what} {}: {source}", path.display()),
        }
    };

    // 1. The database. With a marker present, an absent snapshot can only
    //    mean it was already restored and then discarded on an earlier
    //    attempt — the snapshot is never discarded before this step — so
    //    absence means "already done", never "nothing to restore".
    if lifecycle::db_snapshot_path(data_subdir, "app.db").is_file() {
        lifecycle::restore_db_snapshot(data_subdir).map_err(step(
            "could not restore the pre-update database snapshot from",
            data_subdir,
        ))?;
    }
    stop_at("rollback_db_restored").map_err(|source| unfinished(source.to_string()))?;

    // 2. The tree: `.previous` still there means the swap has not happened
    //    yet; `app_dir` already a directory means it has; neither is a
    //    rollback that lost its own tree, and no marker may paper over that.
    if lifecycle::previous_tree_path(app_dir).is_dir() {
        if app_dir.exists() {
            fs::remove_dir_all(app_dir).map_err(step(
                "could not remove the tree being rolled back at",
                app_dir,
            ))?;
        }
        stop_at("rollback_tree_removed").map_err(|source| unfinished(source.to_string()))?;
        restore_tree(app_dir).map_err(step(
            "could not restore the retained previous tree onto",
            app_dir,
        ))?;
    } else if !app_dir.is_dir() {
        return Err(RollbackError::TreeLost { id: id.to_string() });
    }
    stop_at("rollback_tree_restored").map_err(|source| unfinished(source.to_string()))?;

    // 3. The version record, the registry, the desktop entry.
    lifecycle::write_data_version(data_subdir, &marker.target_version).map_err(|error| {
        unfinished(format!(
            "could not write the version record {}: {error}",
            data_subdir.join("config.json").display()
        ))
    })?;
    stop_at("rollback_version_written").map_err(|source| unfinished(source.to_string()))?;

    let now = registry::now_timestamp();
    registry::update(paths, |registry| {
        if let Some(existing) = registry.get_mut(id) {
            existing.app_version = marker.target_version.clone();
            existing.source_revision = marker.source_revision.clone();
            if let Some(source) = &marker.source {
                existing.source = source.clone();
            }
            existing.updated_at = now.clone();
        }
    })
    .map_err(|error| {
        unfinished(format!(
            "could not update the registry entry for {}: {error}",
            entry.identifier
        ))
    })?;
    stop_at("rollback_registry_written").map_err(|source| unfinished(source.to_string()))?;

    match crate::manifest::load(app_dir) {
        Ok(loaded) => install::write_desktop_entry(paths, id, &loaded.manifest, app_dir),
        Err(error) => eprintln!(
            "tfsapp-hub: warning: could not read {} to refresh the desktop entry: {error}",
            app_dir.display()
        ),
    }

    // 4. Consume the anchor's remaining halves, then the marker itself —
    //    strictly: a marker that cannot be removed keeps every other command
    //    refused, which is exactly what an error here must report.
    lifecycle::discard_db_snapshot(data_subdir);
    lifecycle::discard_rollback_anchor(data_subdir);
    stop_at("rollback_anchor_discarded").map_err(|source| unfinished(source.to_string()))?;
    remove_marker(data_dir).map_err(step(
        "could not remove the rollback marker at",
        &marker_path(data_dir),
    ))?;

    Ok(())
}

/// Which of the anchor's three halves — the retained tree, the database
/// snapshot, `rollback.json` — [`lifecycle::anchor_state`] found missing.
/// Probed separately from the pass/fail decision itself, purely to name what
/// is absent in [`RollbackError::NoAnchor`]'s message: `anchor_state` stays
/// the single "Complete or Missing" gate every caller trusts.
fn missing_halves(data_subdir: &Path, app_dir: &Path) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !lifecycle::previous_tree_path(app_dir).is_dir() {
        missing.push("the retained previous version of its code");
    }
    if !lifecycle::db_snapshot_path(data_subdir, "app.db").is_file() {
        missing.push("its pre-update database snapshot");
    }
    if lifecycle::read_rollback_anchor(data_subdir).is_none() {
        missing.push("its rollback record (rollback.json)");
    }
    missing
}

/// Copy the *current* database aside, before it is overwritten by the
/// restored pre-update snapshot — a manual-recovery artefact at the new
/// schema, never auto-restored. Returns `app.db`'s own rescue path, the one
/// named on screen; `None` when there was no live database to save (an app
/// that never got as far as creating one).
fn rescue_dump(data_subdir: &Path) -> Result<Option<std::path::PathBuf>, RollbackError> {
    let mut app_db_rescue = None;
    for name in lifecycle::DB_FILE_NAMES {
        let source = data_subdir.join(name);
        if !source.is_file() {
            continue;
        }
        let rescue =
            lifecycle::copy_rescue_dump(data_subdir, name).map_err(|error| RollbackError::Io {
                path: error.path,
                source: error.source,
            })?;
        if name == "app.db" {
            app_db_rescue = Some(rescue);
        }
    }
    Ok(app_db_rescue)
}

/// Say what is about to happen, in the terms the user will have to reason
/// about afterwards — `update`'s own `announce` for the reverse direction.
fn announce(id: &str, entry: &RegistryEntry, target_version: &str, target: Option<&Source>) {
    println!("Roll back {id}: {} -> {target_version}", entry.app_version);
    match target {
        Some(target) if *target != entry.source => println!(
            "  source: {} -> {}",
            crate::list::describe_source(&entry.source),
            crate::list::describe_source(target)
        ),
        Some(_) => {}
        None => println!(
            "  the recorded source stays as it is: this update's rollback record \
             predates source tracking"
        ),
    }
    println!(
        "  its database will be restored to its state before that update — anything \
         written since is set aside, not kept, in a rescue copy"
    );
}

/// Everything that can stop a rollback, in one type so the command has one
/// place to print from.
#[derive(Debug)]
pub enum RollbackError {
    Paths(PathsError),
    Registry(RegistryError),
    Install(InstallError),
    Lifecycle(LifecycleError),
    Gate(GateError),
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// No app is registered under this id at all.
    NotInstalled {
        id: String,
    },
    /// The rollback anchor is not complete — one or more of its three halves
    /// is missing, named in `missing`.
    NoAnchor {
        id: String,
        missing: Vec<&'static str>,
    },
    /// A rollback stopped partway through, after its marker was written: the
    /// marker is still there, every other command for this app refuses, and
    /// running `rollback <id>` again finishes what is left. Never rewound —
    /// the tree a rollback replaces is already deleted by then.
    Unfinished {
        id: String,
        detail: String,
    },
    /// A marker is present but neither the retained `.previous` tree nor the
    /// restored `app_dir` exists — a rollback that lost its own tree, which
    /// no marker can paper over.
    TreeLost {
        id: String,
    },
}

impl fmt::Display for RollbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Install(error) => write!(formatter, "{error}"),
            Self::Lifecycle(error) => write!(formatter, "{error}"),
            Self::Gate(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::NotInstalled { id } => write!(
                formatter,
                "no app is installed as {id} — `tfsapp-hub list` shows the ones that are."
            ),
            Self::NoAnchor { id, missing } => write!(
                formatter,
                "{id} has nothing to roll back to — it is missing {}. Either it was never \
                 updated since it was installed, or a previous rollback already consumed \
                 its anchor.",
                missing.join(" and ")
            ),
            Self::Unfinished { id, detail } => write!(
                formatter,
                "the rollback of {id} stopped partway ({detail}); run `tfsapp-hub rollback \
                 {id} --yes` again to finish it."
            ),
            Self::TreeLost { id } => write!(
                formatter,
                "{id}'s rollback marker names a rollback whose restored tree is gone — \
                 neither `apps/{id}` nor its `.previous` exists. The marker is left in place; \
                 resolve the missing tree by hand before running the rollback again."
            ),
        }
    }
}

impl std::error::Error for RollbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Install(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::Gate(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<PathsError> for RollbackError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<RegistryError> for RollbackError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<InstallError> for RollbackError {
    fn from(error: InstallError) -> Self {
        Self::Install(error)
    }
}

impl From<LifecycleError> for RollbackError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

impl From<GateError> for RollbackError {
    fn from(error: GateError) -> Self {
        Self::Gate(error)
    }
}

#[cfg(test)]
#[path = "rollback_tests.rs"]
mod tests;
