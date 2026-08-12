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

// `import` (the plan's step 3) is not yet implemented; `import_decision` and
// `ImportRefusal` are only exercised by this module's own tests until then.
#![allow(dead_code)]

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    lifecycle,
    paths::{Paths, PathsError},
    registry::{self, RegistryError},
};

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

/// `tfsapp-hub export <id> <path>` — resolve `Paths`, run the pipeline, and
/// turn the result into an exit code.
pub fn export(id: &str, path: &str) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match run_export(&paths, id, Path::new(path)) {
        Ok(()) => EXIT_OK,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The pipeline: resolve the registry entry, refuse a busy data dir, build
/// the manifest, write the curated `.tar.gz` atomically, and report what was
/// written.
///
/// Takes its `Paths` rather than resolving them, matching every other
/// command's pipeline — what lets it run against a throwaway root in a test.
fn run_export(paths: &Paths, id: &str, target: &Path) -> Result<(), PortabilityError> {
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| PortabilityError::NotInstalled { id: id.to_string() })?;

    if target.exists() {
        return Err(PortabilityError::TargetExists {
            path: target.to_path_buf(),
        });
    }

    let data_dir = paths.app_data_dir(&entry.identifier)?;
    let holder = busy_holder(&data_dir).map_err(|source| PortabilityError::Io {
        path: data_dir.join("run.lock"),
        source,
    })?;
    if let Some(holder) = holder {
        return Err(PortabilityError::Busy {
            id: id.to_string(),
            holder,
            action: Action::Export,
        });
    }
    let data_subdir = data_dir.join("data");

    let manifest = Manifest {
        identifier: entry.identifier.clone(),
        app_version: entry.app_version.clone(),
        exported_at: registry::now_timestamp(),
        unknown: serde_json::Map::new(),
    };

    let written = write_archive(target, &manifest, &data_subdir)?;

    println!(
        "Exported {id} ({}) to {}.",
        manifest.app_version,
        target.display()
    );
    if written.is_empty() {
        println!("  no database yet — this installation has never been opened.");
    } else {
        for name in &written {
            println!("  {DATA_DIR}/{name}");
        }
    }

    Ok(())
}

/// Whether something already holds `data_dir` — a live window, or an active
/// `run` command — shared by `export` and `import` (plan 022) so both refuse
/// the same two ways rather than probing a second time each in their own
/// words.
///
/// A data directory that does not exist yet is not busy: nothing has ever
/// written to it, so there is nothing to guard against, exactly as
/// `install::check_data_dir_available` treats it.
fn busy_holder(data_dir: &Path) -> io::Result<Option<lifecycle::DataDirHolder>> {
    if !data_dir.is_dir() {
        return Ok(None);
    }
    lifecycle::data_dir_holder(data_dir)
}

/// Write `manifest` and whichever of [`lifecycle::DB_FILE_NAMES`] exist under
/// `data_subdir` into a fresh `.tar.gz`, to a temp path beside `target` and
/// `rename`d into place — so a failure midway leaves `target` itself
/// untouched, never a half-written archive at the name the caller asked for.
///
/// Returns the DB file names actually written, in [`lifecycle::DB_FILE_NAMES`]'s
/// order — empty when the installation has no database yet.
fn write_archive(
    target: &Path,
    manifest: &Manifest,
    data_subdir: &Path,
) -> Result<Vec<&'static str>, PortabilityError> {
    let temporary = target.with_extension("tmp");
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PortabilityError::Io { path, source }
    };

    let file = fs::File::create(&temporary).map_err(io_error(&temporary))?;
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));

    let manifest_json = serde_json::to_vec_pretty(manifest)
        .expect("a Manifest holds nothing that can fail to serialise");
    append_bytes(&mut builder, MANIFEST_FILE, &manifest_json).map_err(io_error(&temporary))?;

    let mut written = Vec::new();
    for name in lifecycle::DB_FILE_NAMES {
        let source_path = data_subdir.join(name);
        if !source_path.is_file() {
            continue;
        }
        let bytes = fs::read(&source_path).map_err(io_error(&source_path))?;
        let archive_path = format!("{DATA_DIR}/{name}");
        append_bytes(&mut builder, &archive_path, &bytes).map_err(io_error(&temporary))?;
        written.push(name);
    }

    let encoder = builder.into_inner().map_err(io_error(&temporary))?;
    encoder.finish().map_err(io_error(&temporary))?;

    fs::rename(&temporary, target).map_err(io_error(target))?;

    Ok(written)
}

/// Append one in-memory file to `builder` at `archive_path`, with an ordinary
/// `0644` mode and this process's own start-adjacent mtime — nothing here
/// reads back a timestamp so any fixed, valid one does.
fn append_bytes<W: io::Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &str,
    data: &[u8],
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    header.set_mtime(mtime);
    header.set_cksum();
    builder.append_data(&mut header, archive_path, data)
}

/// Which command [`PortabilityError::Busy`] was refusing — its message names
/// the risk in the command's own terms rather than a shared, blander one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Export,
}

/// Everything that can stop `export` (and, once step 3 lands, `import`), in
/// one type so both commands have one place to print from — `rollback.rs`'s
/// own `RollbackError` is the template.
#[derive(Debug)]
pub enum PortabilityError {
    Paths(PathsError),
    Registry(RegistryError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// No app is registered under this id at all.
    NotInstalled {
        id: String,
    },
    /// `export`'s target, or `import`'s source, already exists.
    TargetExists {
        path: PathBuf,
    },
    /// A live window or an active `run` command holds the data directory.
    Busy {
        id: String,
        holder: lifecycle::DataDirHolder,
        action: Action,
    },
}

impl fmt::Display for PortabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::NotInstalled { id } => write!(
                formatter,
                "no app is installed as {id} — `tfsapp-hub list` shows the ones that are."
            ),
            Self::TargetExists { path } => write!(
                formatter,
                "{} already exists — pick another path, or remove it first. export never \
                 overwrites a file it did not just write.",
                path.display()
            ),
            Self::Busy { id, holder, action } => {
                let verb = match action {
                    Action::Export => "exporting",
                };
                match holder {
                    lifecycle::DataDirHolder::Window => write!(
                        formatter,
                        "{id} has a window open right now — {verb} while it's open could copy \
                         its database mid-write. Close {id} first."
                    ),
                    lifecycle::DataDirHolder::RunCommand { alias: Some(alias) } => write!(
                        formatter,
                        "{id}'s \"{alias}\" run command is still active — {verb} while it's \
                         running could copy its database mid-write. Stop it first with \
                         `tfsapp-hub run --stop {id}`."
                    ),
                    lifecycle::DataDirHolder::RunCommand { alias: None } => write!(
                        formatter,
                        "a run command is still active for {id} — {verb} while it's running \
                         could copy its database mid-write. Stop it first with `tfsapp-hub run \
                         --stop {id}`."
                    ),
                }
            }
        }
    }
}

impl std::error::Error for PortabilityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<PathsError> for PortabilityError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<RegistryError> for PortabilityError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(test)]
#[path = "portability_tests.rs"]
mod tests;
