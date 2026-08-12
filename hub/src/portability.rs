//! `export <id> <path>` / `import <id> <path> [--force] [--yes]` — moving one
//! installed app's data between machines (`../.project/plan/022-export-and-
//! import.md`).
//!
//! This module holds what needs no I/O and is worth getting right in
//! isolation before the two commands built on it: [`Manifest`], the archive's
//! only metadata, and [`import_decision`], the whole of what `import` refuses
//! and why. The commands themselves — the busy guard, the tar/gzip writing
//! and reading, the confirmation, the rescue dump, the anchor discard — are
//! [`export`] and [`import`], added once the primitives below have their own
//! tests.
//!
//! **What travels, and what does not.** The archive holds exactly
//! `manifest.json` at its root and `data/` plus [`lifecycle::DB_FILE_NAMES`],
//! whichever of those three exist — never re-listed here, read from
//! `lifecycle` directly so there is one spelling of "the database" across the
//! update/rollback anchor and this pair. Everything else in a data directory
//! (`cache/`, `build/`, `log/`, `sessions/`, `secrets.json`, `config.json`,
//! the live locks, the rollback anchor) is excluded, each for its own reason
//! — see the plan's Overview, not restated here.

// `export`/`import` (the plan's steps 2–3) are the real callers; until they
// land, this module's own tests are the only ones exercising it. Remove the
// allow once main.rs routes through `export`/`import`.
#![allow(dead_code)]

use std::path::Path;

use serde::{Deserialize, Serialize};

/// `manifest.json`, at the archive's root — the only metadata `export`
/// writes and `import` reads back.
///
/// `unknown` carries a manifest a later hub wrote through untouched, the same
/// "warn on an unknown key, never refuse" rule `registry.rs`'s own `unknown`
/// fields follow: a manifest this hub cannot fully read is still a manifest
/// it can compare `identifier` and `app_version` from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Manifest {
    /// The exporting registry entry's own `identifier` — what `import`
    /// checks against the installation it is seeding, never against the
    /// hub's own identity.
    pub identifier: String,
    /// The exporting registry entry's own `app_version` — what `import`
    /// refuses to write into a data directory a newer app already occupies.
    pub app_version: String,
    /// RFC 3339, UTC — [`crate::registry::now_timestamp`]'s own convention,
    /// reused rather than reformatted a second way.
    pub exported_at: String,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// The manifest's filename at the archive's root.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The archive's internal directory holding the curated database files —
/// [`lifecycle::DB_FILE_NAMES`] joined under it, never a separate list.
pub const DATA_DIR: &str = "data";

/// Why `import <id> <path>` refuses, in the fixed order [`import_decision`]
/// checks them.
///
/// Identifier first, then version, then the populated dir: the two
/// unconditional refusals are checked before the one `--force` can unlock,
/// so a foreign archive is never reported as "pass --force" and a future
/// archive is never reported as merely "the data dir is populated".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportRefusal {
    /// The archive's `identifier` is not the installation's own. Not a
    /// decision to override — `--force` does not reach this.
    IdentifierMismatch { archive: String, installed: String },
    /// The archive's `app_version` is newer than the installed app's.
    /// `--force` does not reach this either: writing a future version into
    /// `data/config.json` would leave the next launch on
    /// [`lifecycle::LifecycleDecisionError::Downgrade`], which has no
    /// recovery path.
    ArchiveNewer { archive: String, installed: String },
    /// The data directory already holds a database. The one refusal
    /// `--force` unlocks.
    DataDirPopulated,
}

/// The whole of what `import` refuses, and why — pure, no I/O: every input is
/// already resolved by the caller (the manifest read from the archive, the
/// installed identity and version from the registry entry, whether the data
/// directory is populated from [`data_dir_populated`]).
///
/// `manifest.app_version` is expected to already parse as semver — `export`
/// only ever writes a registry entry's own `app_version`, which `install`
/// refused to accept in any other shape (CONTRACT.md §2) — so the caller
/// reads and validates `manifest.json` before this is ever reached, exactly
/// as `install::lifecycle_event_for_install` trusts the installed side's own
/// already-validated version.
pub fn import_decision(
    manifest: &Manifest,
    installed_identifier: &str,
    installed_version: &semver::Version,
    data_dir_populated: bool,
    force: bool,
) -> Result<(), ImportRefusal> {
    if manifest.identifier != installed_identifier {
        return Err(ImportRefusal::IdentifierMismatch {
            archive: manifest.identifier.clone(),
            installed: installed_identifier.to_string(),
        });
    }

    let archive_version = semver::Version::parse(&manifest.app_version)
        .expect("the caller already validated manifest.app_version before calling import_decision");
    if archive_version > *installed_version {
        return Err(ImportRefusal::ArchiveNewer {
            archive: archive_version.to_string(),
            installed: installed_version.to_string(),
        });
    }

    if data_dir_populated && !force {
        return Err(ImportRefusal::DataDirPopulated);
    }

    Ok(())
}

impl std::fmt::Display for ImportRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IdentifierMismatch { archive, installed } => write!(
                formatter,
                "this archive was exported from {archive}, but this installation is \
                 {installed} — importing it would seed the wrong app's data."
            ),
            Self::ArchiveNewer { archive, installed } => write!(
                formatter,
                "this archive is from version {archive}, but the installed app is only \
                 {installed} — importing it would write data a newer app understands into an \
                 older one. Update the app first."
            ),
            Self::DataDirPopulated => write!(
                formatter,
                "this installation already has a database. Pass --force to replace it — the \
                 current one is rescue-dumped first, never destroyed outright."
            ),
        }
    }
}

/// Whether `data_subdir` already holds a database — the one criterion
/// [`import_decision`] and `import` itself act on.
///
/// `app.db` existing, specifically: not "the directory exists" (`install`
/// creates it empty before anything else runs) and not "`config.json`
/// exists" (same), so a reinstall-then-import sequence is never spuriously
/// refused.
pub fn data_dir_populated(data_subdir: &Path) -> bool {
    data_subdir.join("app.db").is_file()
}

#[cfg(test)]
#[path = "portability_tests.rs"]
mod tests;
