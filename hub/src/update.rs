//! `tfsapp-hub update <id>` — replace an installed app with a newer version of
//! its own source (the per-app update and rollback plan, `000-index.md`).
//!
//! This module holds two things that need no I/O and are worth getting right
//! in isolation before the command around them exists at all:
//!
//! - [`update_decision`] — the Overview's decision table, built on the same
//!   [`lifecycle::lifecycle_decision`] `open`, `run` and `install` already use.
//!   No second comparison, no second spelling of what "newer" means.
//! - the tree-anchor helpers ([`retain_tree`], [`restore_tree`],
//!   [`discard_tree`]) — the code half of the rollback anchor
//!   ([`lifecycle::previous_tree_path`]), which is a rename rather than a
//!   copy: an update pays for one generation of the tree, never a copy pass
//!   over it.
//!
//! The command's own imperative flow — resolving the source, guarding the
//! data directory, snapshotting the database, swapping the trees, running the
//! event's commands, and committing or reverting the whole thing — is a later
//! step's addition.

use std::{fs, io, path::Path};

use crate::lifecycle::{self, LifecycleDecisionError, LifecycleEvent};

/// What `update <id>` does, decided from the registry's recorded `app_version`
/// against the freshly resolved source's own. No I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// The update event: snapshot, swap, `pre-update` then `post-update`,
    /// commit.
    Apply,
    /// `--force` on an equal record: re-sync the code and its dependencies,
    /// run no lifecycle command, take no snapshot, and leave the existing
    /// anchor alone.
    ResyncOnly,
}

/// Why [`update_decision`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRefusal {
    /// Nothing is recorded for this id at all — that moment is an install,
    /// and `install` owns it. Unreachable from the real command (a
    /// registered `id` always carries a registry `app_version`); kept as a
    /// row of the decision table anyway so it is one function to test rather
    /// than an assumption baked silently into the caller.
    NoRecord,
    /// The source is exactly what is already installed. `--force` unlocks
    /// [`UpdateAction::ResyncOnly`]; without it, this refuses.
    Equal { version: semver::Version },
    /// The source is *older* than what is installed — a downgrade, which
    /// `--force` does not unlock: it is for the case `source_revision` exists
    /// to catch (a tree edited without bumping the version), not for
    /// reverting to an older release.
    Downgrade {
        recorded: semver::Version,
        source: semver::Version,
    },
    /// The registry's own recorded `app_version` does not parse as semver.
    /// Unreachable in practice — `install::validate` already refused a
    /// non-canonical one at install time — carried as a message (`semver::Error`
    /// implements neither `Clone` nor `PartialEq`) so this function is total
    /// over its input rather than panicking on a corrupt registry.
    InvalidRecordedVersion(String),
}

/// The Overview's decision table:
///
/// | record vs source | action |
/// | --- | --- |
/// | source newer | [`UpdateAction::Apply`] |
/// | equal | [`UpdateRefusal::Equal`], naming `--force` |
/// | equal, with `--force` | [`UpdateAction::ResyncOnly`] |
/// | source older | [`UpdateRefusal::Downgrade`] — `--force` does not unlock it |
/// | no record at all | [`UpdateRefusal::NoRecord`] |
///
/// `recorded` is the registry entry's own `app_version`, straight from the
/// caller's already-loaded entry (never [`lifecycle::read_data_version`] —
/// that record belongs to `install`'s own gate, not this one). `source` is
/// the freshly re-resolved source's `app_version`, already validated as
/// semver by whichever `validate` the caller ran.
pub fn update_decision(
    recorded: Option<&str>,
    source: &semver::Version,
    force: bool,
) -> Result<UpdateAction, UpdateRefusal> {
    match lifecycle::lifecycle_decision(recorded, source) {
        Ok(LifecycleEvent::Install) => Err(UpdateRefusal::NoRecord),
        Ok(LifecycleEvent::None) => match force {
            true => Ok(UpdateAction::ResyncOnly),
            false => Err(UpdateRefusal::Equal {
                version: source.clone(),
            }),
        },
        Ok(LifecycleEvent::Update) => Ok(UpdateAction::Apply),
        Err(LifecycleDecisionError::Downgrade { recorded, current }) => {
            Err(UpdateRefusal::Downgrade {
                recorded,
                source: current,
            })
        }
        Err(LifecycleDecisionError::InvalidVersion(error)) => {
            Err(UpdateRefusal::InvalidRecordedVersion(error.to_string()))
        }
    }
}

/// Rename `app_dir` to its `.previous` sibling
/// ([`lifecycle::previous_tree_path`]), removing any older one first.
///
/// An update never copies the outgoing tree, it moves it (the plan's
/// Overview): the cost of holding an anchor is one generation of `apps/<id>`,
/// not a copy pass over it. A leftover `.previous` from an interrupted
/// earlier update is replaced rather than appended to — there is only ever
/// one generation to roll back to.
pub fn retain_tree(app_dir: &Path) -> io::Result<()> {
    let previous = lifecycle::previous_tree_path(app_dir);
    if previous.is_dir() {
        fs::remove_dir_all(&previous)?;
    }
    fs::rename(app_dir, &previous)
}

/// Rename the `.previous` tree back to `app_dir` — a failed update's undo of
/// [`retain_tree`], and `rollback <id>`'s own restoration of the anchor's
/// tree half.
pub fn restore_tree(app_dir: &Path) -> io::Result<()> {
    fs::rename(lifecycle::previous_tree_path(app_dir), app_dir)
}

/// Delete the `.previous` tree without touching `app_dir` — `remove <id>`'s
/// own anchor cleanup, and `rollback <id>`'s consumption of the anchor's tree
/// half once [`restore_tree`] has already put it back as `app_dir`.
/// Best-effort: an already-missing tree is not an error.
pub fn discard_tree(app_dir: &Path) {
    let _ = fs::remove_dir_all(lifecycle::previous_tree_path(app_dir));
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
