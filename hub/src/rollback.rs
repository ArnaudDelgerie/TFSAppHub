//! `tfsapp-hub rollback <id> [--yes]` — undo the last successful update (the
//! per-app update and rollback plan, `000-index.md`).
//!
//! The mirror image of `update <id>`'s `Apply` action, and deliberately not
//! symmetric with it: an update reverts itself on failure and leaves the
//! anchor untouched on success, because there is a next launch to protect
//! either way. A rollback has no "next attempt" of its own to protect — it
//! *is* the undo — so it runs straight through in one pass and, on success,
//! consumes the anchor rather than rotating it (the plan's Overview: a
//! rollback is one step back, and a `.previous` naming the version just left
//! would invite a "rollback forward" this plan does not define).
//!
//! The one thing it does not throw away is the database it is about to
//! replace: [`lifecycle::rescue_dump_path`] carries it forward as a
//! manual-recovery artefact, named on screen, so "I rolled back too eagerly"
//! never costs data that was never wrong.

use std::{fmt, fs, path::Path};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    install::{self, InstallError},
    lifecycle::{self, Anchor, LifecycleError},
    paths::{Paths, PathsError},
    prompt,
    registry::{self, RegistryEntry, RegistryError},
    update::restore_tree,
};

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
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| RollbackError::NotInstalled { id: id.to_string() })?
        .clone();

    let app_dir = paths.app_dir(id)?;
    let data_dir = paths.app_data_dir(&entry.identifier)?;
    let data_subdir = data_dir.join("data");

    let (target_version, source_revision) = match lifecycle::anchor_state(&data_subdir, &app_dir) {
        Anchor::Complete {
            app_version,
            source_revision,
            ..
        } => (app_version, source_revision),
        Anchor::Missing => {
            return Err(RollbackError::NoAnchor {
                id: id.to_string(),
                missing: missing_halves(&data_subdir, &app_dir),
            })
        }
    };

    install::check_data_dir_available(id, &entry.identifier, &data_dir)?;

    announce(id, &entry, &target_version);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(false);
    }

    let rescue_path = rescue_dump(&data_subdir)?;

    lifecycle::restore_db_snapshot(&data_subdir).map_err(|source| RollbackError::Io {
        path: data_subdir.clone(),
        source,
    })?;

    fs::remove_dir_all(&app_dir).map_err(|source| RollbackError::Io {
        path: app_dir.clone(),
        source,
    })?;
    restore_tree(&app_dir).map_err(|source| RollbackError::Io {
        path: app_dir.clone(),
        source,
    })?;

    lifecycle::write_data_version(&data_subdir, &target_version)?;

    let now = registry::now_timestamp();
    registry::update(paths, |registry| {
        if let Some(existing) = registry.get_mut(id) {
            existing.app_version = target_version.clone();
            existing.source_revision = source_revision.clone();
            existing.updated_at = now.clone();
        }
    })?;

    match crate::manifest::load(&app_dir) {
        Ok(loaded) => install::write_desktop_entry(paths, id, &loaded.manifest, &app_dir),
        Err(error) => eprintln!(
            "tfsapp-hub: warning: could not read {} to refresh the desktop entry: {error}",
            app_dir.display()
        ),
    }

    // The anchor is consumed, not rotated: the tree half is already gone
    // (`restore_tree` renamed `.previous` back to `app_dir`), so only the
    // database snapshot and `rollback.json` are left to discard.
    lifecycle::discard_db_snapshot(&data_subdir);
    lifecycle::discard_rollback_anchor(&data_subdir);

    if let Some(rescue_path) = rescue_path {
        println!(
            "The database from before the rollback was saved to {}.",
            rescue_path.display()
        );
    }
    println!("Rolled back {id} to {target_version}.");

    Ok(true)
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
        let rescue = lifecycle::rescue_dump_path(data_subdir, name);
        fs::copy(&source, &rescue).map_err(|error| RollbackError::Io {
            path: rescue.clone(),
            source: error,
        })?;
        if name == "app.db" {
            app_db_rescue = Some(rescue);
        }
    }
    Ok(app_db_rescue)
}

/// Say what is about to happen, in the terms the user will have to reason
/// about afterwards — `update`'s own `announce` for the reverse direction.
fn announce(id: &str, entry: &RegistryEntry, target_version: &str) {
    println!("Roll back {id}: {} -> {target_version}", entry.app_version);
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
}

impl fmt::Display for RollbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Install(error) => write!(formatter, "{error}"),
            Self::Lifecycle(error) => write!(formatter, "{error}"),
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

#[cfg(test)]
#[path = "rollback_tests.rs"]
mod tests;
