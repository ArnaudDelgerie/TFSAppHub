//! `export <id> <path>` / `import <id> <path> [--force] [--yes]` — moving one
//! installed app's data between machines (`../.project/plan/022-export-and-
//! import.md`).
//!
//! This module holds what needs no I/O and is worth getting right in
//! isolation before the two commands built on it: [`Manifest`], the archive's
//! only metadata, and [`import_decision`], the whole of what `import` refuses
//! and why. The commands themselves — the busy guard, the tar/gzip writing
//! and reading, the confirmation, the destination cache invalidation, the
//! rescue dump, the anchor discard, the forward migration of an archive older
//! than what is installed — are
//! [`export`] and [`import`], added once the primitives below have their own
//! tests.
//!
//! **What travels, and what does not.** The archive holds `manifest.json` at
//! its root, `data/` plus [`lifecycle::DB_FILE_NAMES`] (whichever of those
//! exist — never re-listed here, read from `lifecycle` directly so there is
//! one spelling of "the database" across the update/rollback anchor and this
//! pair), and — since plan 049 — every regular file under `uploads/`
//! (decision 006: `<app data>/uploads/`, injected as `APP_UPLOAD_DIR`, is the
//! app's own durable-file directory and travels alongside the database it
//! indexes). Everything else in a data directory (`cache/`, `build/`, `log/`,
//! `sessions/`, `secrets.json`, `config.json`, the live locks, the rollback
//! anchor) is excluded, each for its own reason — see the plan's Overview,
//! not restated here.

use std::{
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    archive,
    cli::{EXIT_FAILED, EXIT_OK},
    install::{self, InstallError},
    lifecycle::{self, LifecycleEvent},
    lifecycle_gate::{self, GateError},
    manifest::{self, ManifestError},
    paths::{Paths, PathsError},
    php::{self, PhpError},
    prompt,
    registry::{self, RegistryError},
    update,
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
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

/// The archive's internal directory holding the curated database files —
/// [`lifecycle::DB_FILE_NAMES`] joined under it, never a separate list.
pub const DATA_DIR: &str = "data";

/// The archive's internal directory holding the app's durable files — a
/// sibling of [`DATA_DIR`] here exactly as `uploads/` is a sibling of `data/`
/// on disk (decision 006), never a child of it.
pub const UPLOADS_DIR: &str = "uploads";

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

/// Whether `data_dir` already holds something `import` would silently
/// replace — the one criterion [`import_decision`] and `import` itself act
/// on. True when `data/app.db` exists, or when `uploads/` holds anything at
/// all — a durable file is exactly as much the user's data as the database
/// that indexes it (decision 006).
///
/// Not "the directories exist" (`install` creates both empty before anything
/// else runs) and not "`config.json` exists" (same), so a reinstall-then-
/// import sequence is never spuriously refused.
pub fn data_dir_populated(data_dir: &Path) -> bool {
    if data_dir.join(DATA_DIR).join("app.db").is_file() {
        return true;
    }
    match fs::read_dir(data_dir.join(UPLOADS_DIR)) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
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
        .ok_or_else(|| PortabilityError::NotInstalled { id: id.to_string() })?
        .clone();
    let _maintenance = lifecycle_gate::acquire_maintenance(paths, &entry.identifier, "export")?;
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
    let holder =
        busy_holder(&data_dir, &entry.identifier).map_err(|source| PortabilityError::Io {
            path: data_dir.join("runs"),
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
    let uploads_dir = data_dir.join("uploads");

    let manifest = Manifest {
        identifier: entry.identifier.clone(),
        app_version: entry.app_version.clone(),
        exported_at: registry::now_timestamp(),
        unknown: serde_json::Map::new(),
    };

    let written = write_archive(target, &manifest, &data_subdir, &uploads_dir)?;

    println!(
        "Exported {id} ({}) to {}.",
        manifest.app_version,
        target.display()
    );
    if written.database.is_empty() {
        println!("  no database yet — this installation has never been opened.");
    } else {
        for name in &written.database {
            println!("  {DATA_DIR}/{name}");
        }
    }
    if written.uploads_count > 0 {
        println!(
            "  {UPLOADS_DIR}/ ({} file{}, {} bytes)",
            written.uploads_count,
            if written.uploads_count == 1 { "" } else { "s" },
            written.uploads_bytes
        );
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
fn busy_holder(data_dir: &Path, identifier: &str) -> io::Result<Option<lifecycle::DataDirHolder>> {
    if !data_dir.is_dir() {
        return Ok(None);
    }
    lifecycle::data_dir_holder(data_dir, identifier)
}

/// What [`write_archive`] actually wrote, for `run_export`'s report — a count
/// and a total size for `uploads/` is enough; the point is that the user sees
/// the archive is not just a database.
struct ArchiveContents {
    /// The DB file names actually written, in [`lifecycle::DB_FILE_NAMES`]'s
    /// order — empty when the installation has no database yet.
    database: Vec<&'static str>,
    uploads_count: u64,
    uploads_bytes: u64,
}

/// Write `manifest`, whichever of [`lifecycle::DB_FILE_NAMES`] exist under
/// `data_subdir`, and every regular file under `uploads_dir` into a fresh
/// `.tar.gz`, to a temp path beside `target` and `rename`d into place — so a
/// failure midway leaves `target` itself untouched, never a half-written
/// archive at the name the caller asked for.
fn write_archive(
    target: &Path,
    manifest: &Manifest,
    data_subdir: &Path,
    uploads_dir: &Path,
) -> Result<ArchiveContents, PortabilityError> {
    let temporary = export_temp_path(target);
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PortabilityError::Io { path, source }
    };

    if temporary.exists() {
        return Err(PortabilityError::TemporaryExists { path: temporary });
    }

    let file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
    {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PortabilityError::TemporaryExists { path: temporary });
        }
        Err(source) => return Err(io_error(&temporary)(source)),
    };

    let result = (|| {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        ));

        let manifest_json = serde_json::to_vec_pretty(manifest)
            .expect("a Manifest holds nothing that can fail to serialise");
        append_bytes(&mut builder, MANIFEST_FILE, &manifest_json).map_err(io_error(&temporary))?;

        let mut database = Vec::new();
        for name in lifecycle::DB_FILE_NAMES {
            let source_path = data_subdir.join(name);
            if !source_path.is_file() {
                continue;
            }
            let bytes = fs::read(&source_path).map_err(io_error(&source_path))?;
            let archive_path = format!("{DATA_DIR}/{name}");
            append_bytes(&mut builder, &archive_path, &bytes).map_err(io_error(&temporary))?;
            database.push(name);
        }

        let (uploads_count, uploads_bytes) = append_uploads(&mut builder, uploads_dir, &temporary)?;

        let encoder = builder.into_inner().map_err(io_error(&temporary))?;
        encoder.finish().map_err(io_error(&temporary))?;

        fs::rename(&temporary, target).map_err(io_error(target))?;

        Ok(ArchiveContents {
            database,
            uploads_count,
            uploads_bytes,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Walk `uploads_dir` and append every regular file it holds under an
/// `uploads/`-prefixed archive path, sorted at every level so two exports of
/// identical content produce byte-identical archives (the same determinism
/// the fixed header in [`append_bytes`] exists for). A directory that does
/// not exist is not a skip and not an error — the ordinary case for an app
/// that has never written a file. Anything that is not a regular file (a
/// symlink, a socket, a fifo) is skipped and named on stderr rather than
/// followed or embedded.
///
/// `io_error`s are attributed to `temporary` — `append_bytes`'s own failure
/// is always a write to the archive being built, never to the source file
/// just read, matching the database loop just above this call.
fn append_uploads<W: io::Write>(
    builder: &mut tar::Builder<W>,
    uploads_dir: &Path,
    temporary: &Path,
) -> Result<(u64, u64), PortabilityError> {
    if !uploads_dir.is_dir() {
        return Ok((0, 0));
    }
    let mut count = 0u64;
    let mut bytes = 0u64;
    append_uploads_dir(
        builder,
        uploads_dir,
        Path::new(""),
        temporary,
        &mut count,
        &mut bytes,
    )?;
    Ok((count, bytes))
}

fn append_uploads_dir<W: io::Write>(
    builder: &mut tar::Builder<W>,
    uploads_dir: &Path,
    relative: &Path,
    temporary: &Path,
    count: &mut u64,
    bytes: &mut u64,
) -> Result<(), PortabilityError> {
    let absolute = uploads_dir.join(relative);
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PortabilityError::Io { path, source }
    };

    let mut entries: Vec<fs::DirEntry> = fs::read_dir(&absolute)
        .map_err(io_error(&absolute))?
        .collect::<io::Result<Vec<_>>>()
        .map_err(io_error(&absolute))?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let file_type = entry.file_type().map_err(io_error(&absolute))?;
        let child_relative = relative.join(entry.file_name());
        let child_absolute = uploads_dir.join(&child_relative);
        if file_type.is_dir() {
            append_uploads_dir(
                builder,
                uploads_dir,
                &child_relative,
                temporary,
                count,
                bytes,
            )?;
        } else if file_type.is_file() {
            let data = fs::read(&child_absolute).map_err(io_error(&child_absolute))?;
            let archive_path = Path::new(UPLOADS_DIR).join(&child_relative);
            append_bytes(builder, &archive_path, &data).map_err(io_error(temporary))?;
            *count += 1;
            *bytes += data.len() as u64;
        } else {
            eprintln!(
                "skipping {}: not a regular file ({})",
                child_absolute.display(),
                if file_type.is_symlink() {
                    "a symlink"
                } else {
                    "neither a file nor a directory"
                }
            );
        }
    }
    Ok(())
}

/// The unfinished archive sits next to its eventual target under its complete
/// filename, so `backup.tar.gz` becomes `backup.tar.gz.tmp`.
fn export_temp_path(target: &Path) -> PathBuf {
    let mut temporary = target.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

/// Append one in-memory file to `builder` at `archive_path`, with an ordinary
/// `0644` mode and this process's own start-adjacent mtime — nothing here
/// reads back a timestamp so any fixed, valid one does.
fn append_bytes<W: io::Write>(
    builder: &mut tar::Builder<W>,
    archive_path: impl AsRef<Path>,
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

/// `tfsapp-hub import <id> <path> [--force] [--yes]` — resolve `Paths`, run
/// the pipeline, and turn the result into an exit code.
pub fn import(id: &str, path: &str, force: bool, assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match run_import(&paths, id, Path::new(path), force, assume_yes) {
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

/// The pipeline (the plan's step 3): resolve the entry, read the manifest out
/// of the archive before anything else, refuse a busy data dir, run
/// [`import_decision`], confirm when overwriting, then — past the point of no
/// return — rescue-dump, discard the anchor, extract, migrate forward when
/// the archive is older than what is installed, and record the version.
/// `false` means the user declined.
///
/// Takes its `Paths` rather than resolving them, matching every other
/// command's pipeline — what lets it run against a throwaway root in a test.
fn run_import(
    paths: &Paths,
    id: &str,
    archive_path: &Path,
    force: bool,
    assume_yes: bool,
) -> Result<bool, PortabilityError> {
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| PortabilityError::NotInstalled { id: id.to_string() })?
        .clone();

    let _maintenance = lifecycle_gate::acquire_maintenance(paths, &entry.identifier, "import")?;
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| PortabilityError::NotInstalled { id: id.to_string() })?
        .clone();

    // Before anything else touches the data dir: a malformed or non-archive
    // `<path>` is a clean refusal here rather than a half-run pipeline.
    let manifest = read_manifest(archive_path)?;
    let archive_version = semver::Version::parse(&manifest.app_version).map_err(|error| {
        PortabilityError::MalformedManifest {
            path: archive_path.to_path_buf(),
            detail: format!("app_version {:?}: {error}", manifest.app_version),
        }
    })?;

    let data_dir = paths.app_data_dir(&entry.identifier)?;
    let holder =
        busy_holder(&data_dir, &entry.identifier).map_err(|source| PortabilityError::Io {
            path: data_dir.join("runs"),
            source,
        })?;
    if let Some(holder) = holder {
        return Err(PortabilityError::Busy {
            id: id.to_string(),
            holder,
            action: Action::Import,
        });
    }
    let data_subdir = data_dir.join("data");

    let installed_version = semver::Version::parse(&entry.app_version)
        .expect("the registry only ever holds a canonical semver app_version");
    let populated = data_dir_populated(&data_dir);
    import_decision(
        &manifest,
        &entry.identifier,
        &installed_version,
        populated,
        force,
    )
    .map_err(PortabilityError::Refused)?;

    if populated {
        announce_overwrite(
            id,
            &data_dir,
            &entry.app_version,
            &archive_version.to_string(),
        );
        if !prompt::confirmed(assume_yes) {
            println!("Aborted — nothing was changed.");
            return Ok(false);
        }
    }

    // The disposable-cache boundary — the first thing the import changes, and
    // the one write that must land before the rescue/replacement boundary
    // below. The preflight above only reads; this clears the state *derived*
    // from the database the archive is about to replace, so that neither the
    // forward migration below nor a command run right after the import can
    // read a container compiled from the previous database (`Mode::Run` and
    // `Mode::Install` deliberately clear nothing, and the stamp's three
    // dimensions do not include the database — the Overview's bug). Strict on
    // failure, unlike `Mode::Launch`'s best-effort wipe: the error stops the
    // import here, before the rescue phase, with the database, `uploads/`,
    // the version record and the rollback anchor all untouched.
    clear_destination_cache(&data_dir, &data_subdir)?;

    // The rescue/replacement boundary: what follows rescue-dumps what it is
    // about to overwrite, discards the anchor
    // (an import proceeding has already made it incoherent — see the
    // Overview), and then writes.
    let rescue_path = rescue_dump(&data_subdir)?;
    remove_live_db_files(&data_subdir).map_err(|error| {
        import_incomplete(
            &data_subdir,
            rescue_path.as_deref(),
            None,
            "the previous database was rescued but could not be fully moved aside",
            error,
        )
    })?;
    let uploads_rescue_path = rescue_uploads(&data_dir).map_err(|error| {
        import_incomplete(
            &data_subdir,
            rescue_path.as_deref(),
            None,
            "the previous database was rescued and removed but its uploads/ directory could \
             not be rescued aside",
            error,
        )
    })?;
    lifecycle::discard_rollback_anchor(&data_subdir);
    lifecycle::discard_db_snapshot(&data_subdir);
    let app_dir = paths.app_dir(id).map_err(|error| {
        import_incomplete(
            &data_subdir,
            rescue_path.as_deref(),
            uploads_rescue_path.as_deref(),
            "the previous database was rescued but the import could not start",
            error,
        )
    })?;
    update::discard_tree(&app_dir);

    archive::extract_prefix(archive_path, DATA_DIR, &data_subdir).map_err(|error| {
        import_incomplete(
            &data_subdir,
            rescue_path.as_deref(),
            uploads_rescue_path.as_deref(),
            "the data directory may be partially extracted",
            error,
        )
    })?;
    archive::extract_prefix(archive_path, UPLOADS_DIR, &data_dir.join(UPLOADS_DIR)).map_err(
        |error| {
            import_incomplete(
                &data_subdir,
                rescue_path.as_deref(),
                uploads_rescue_path.as_deref(),
                "the database was extracted but uploads/ may be partially extracted",
                error,
            )
        },
    )?;

    // Stamp the archive's version before any forward migration. If a hook
    // fails now, `open` reads this older record as the same unfinished update
    // it already knows to refuse, rather than serving an unmigrated database
    // under the installed version's old, misleading stamp.
    lifecycle::write_data_version(&data_subdir, &manifest.app_version).map_err(|error| {
        import_incomplete(
            &data_subdir,
            rescue_path.as_deref(),
            uploads_rescue_path.as_deref(),
            "the database was extracted but its version record could not be written",
            error,
        )
    })?;

    // `lifecycle::check_version` — the guard every `open` runs — reads only
    // `data/config.json`; it has no way to tell "an update never finished"
    // from "an older archive was just imported", and refuses either way. Left
    // at the archive's own version, an older archive would make the app
    // permanently unopenable: `open` refuses citing `update <id>`, and
    // `update` itself cannot rescue it, because its own decision compares the
    // registry to the freshly resolved *source*, never to the data directory
    // (`update::update_decision`'s own doc) — so an unchanged source reads as
    // "nothing to update" regardless of what the data directory says. Running
    // the installed manifest's `pre-update`/`post-update` right here — the
    // same event `update`'s own `Apply` runs — is what keeps "restore an
    // older backup onto an already-updated installation", the ordinary
    // cross-machine case (the plan's Overview), from landing on data nothing
    // can ever open again.
    let migrated_forward = if archive_version < installed_version {
        let installed_manifest = manifest::load(&app_dir)
            .map_err(|error| {
                import_incomplete(
                    &data_subdir,
                    rescue_path.as_deref(),
                    uploads_rescue_path.as_deref(),
                    "the database was extracted but not migrated forward",
                    error,
                )
            })?
            .manifest;
        let toolchain = php::toolchain(paths).map_err(|error| {
            import_incomplete(
                &data_subdir,
                rescue_path.as_deref(),
                uploads_rescue_path.as_deref(),
                "the database was extracted but not migrated forward",
                error,
            )
        })?;
        install::prepare(
            paths,
            &toolchain,
            &installed_manifest,
            &app_dir,
            LifecycleEvent::Update,
            &entry.platform,
        )
        .map_err(|error| {
            import_incomplete(
                &data_subdir,
                rescue_path.as_deref(),
                uploads_rescue_path.as_deref(),
                "the database was extracted but not migrated forward",
                error,
            )
        })?;
        true
    } else {
        // Equal versions only — `import_decision` already refused anything
        // newer, and the archive's version record was written above.
        false
    };

    if let Some(rescue_path) = rescue_path {
        println!(
            "The database being replaced was saved to {}.",
            rescue_path.display()
        );
    }
    if let Some(uploads_rescue_path) = uploads_rescue_path {
        println!(
            "The uploads/ directory being replaced was saved to {}.",
            uploads_rescue_path.display()
        );
    }
    if migrated_forward {
        println!(
            "Imported into {id} at {} and migrated forward to {} — the archive was older than \
             the installed app, so pre-update then post-update ran on it.",
            manifest.app_version, entry.app_version
        );
    } else {
        println!("Imported into {id} at {}.", manifest.app_version);
    }
    println!(
        "  this machine keeps its own APP_SECRET: any session or remember-me token in the \
         imported database is invalid here, anything the source encrypted with its own \
         APP_SECRET is unreadable, and actions.secrets values must be re-provisioned"
    );

    Ok(true)
}

/// Read and parse `manifest.json` out of the archive at `archive_path`,
/// without extracting anything else — `import`'s first read, so a malformed
/// or non-`.tar.gz` file is refused before the data directory is touched at
/// all.
fn read_manifest(archive_path: &Path) -> Result<Manifest, PortabilityError> {
    let io_error = |source: io::Error| PortabilityError::Io {
        path: archive_path.to_path_buf(),
        source,
    };

    let file = fs::File::open(archive_path).map_err(io_error)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = archive.entries().map_err(io_error)?;

    for entry in entries {
        let entry = entry.map_err(io_error)?;
        let path = entry.path().map_err(io_error)?.into_owned();
        if path != Path::new(MANIFEST_FILE) {
            continue;
        }

        let mut contents = String::new();
        entry
            .take(MANIFEST_MAX_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(io_error)?;
        if contents.len() as u64 > MANIFEST_MAX_BYTES {
            return Err(PortabilityError::MalformedManifest {
                path: archive_path.to_path_buf(),
                detail: "manifest.json exceeds the 1 MiB limit".to_string(),
            });
        }
        return serde_json::from_str(&contents).map_err(|error| {
            PortabilityError::MalformedManifest {
                path: archive_path.to_path_buf(),
                detail: error.to_string(),
            }
        });
    }

    Err(PortabilityError::NoManifest {
        path: archive_path.to_path_buf(),
    })
}

/// Say what overwriting is about to cost, in the terms the user will have to
/// reason about afterwards — `rollback.rs`'s own `announce` for this
/// command's shape of "and if I got this wrong?".
fn announce_overwrite(id: &str, data_dir: &Path, installed_version: &str, archive_version: &str) {
    println!(
        "Import into {id}: {installed_version} data -> replaced by the archive's {archive_version}"
    );
    println!(
        "  its current database will be replaced — the database being replaced will be saved \
         to {}",
        lifecycle::rescue_dump_pattern(&data_dir.join(DATA_DIR), "app.db").display()
    );
    println!(
        "  its uploads/ directory, if not empty, will be replaced too — it will be saved to {}",
        lifecycle::rescue_dump_pattern(data_dir, UPLOADS_DIR).display()
    );
    println!(
        "  its rollback anchor, if any, will be discarded — a rollback after this import would \
         have nothing coherent left to restore"
    );
}

/// Copy the *current* database aside, before it is overwritten by the
/// archive's — a manual-recovery artefact, never auto-restored. Returns
/// `app.db`'s own rescue path, the one named on screen; `None` when there was
/// nothing to save (an app that never got as far as creating a database).
///
/// `rollback.rs`'s own `rescue_dump`, duplicated rather than shared: both are
/// small, module-private pipeline steps over the same
/// [`lifecycle::copy_rescue_dump`], and the two commands' error types differ.
fn rescue_dump(data_subdir: &Path) -> Result<Option<PathBuf>, PortabilityError> {
    let mut app_db_rescue = None;
    for name in lifecycle::DB_FILE_NAMES {
        let source = data_subdir.join(name);
        if !source.is_file() {
            continue;
        }
        let rescue = lifecycle::copy_rescue_dump(data_subdir, name).map_err(|error| {
            PortabilityError::Io {
                path: error.path,
                source: error.source,
            }
        })?;
        if name == "app.db" {
            app_db_rescue = Some(rescue);
        }
    }
    Ok(app_db_rescue)
}

/// Rename an existing, non-empty `uploads/` aside to
/// `uploads.rescue-<YYYYMMDDTHHMMSSZ>`, following the same naming and
/// collision rule `app.db.rescue-*` uses (plan 027). A rename, not a copy —
/// unlike [`rescue_dump`], nothing afterwards needs the source gone on its
/// own account, but a directory of uploads can be gigabytes and copying it
/// would make every forced import pay for a case nobody asked for (the
/// plan's Overview). `None` when there was nothing to rescue: an absent or
/// empty `uploads/` needs no rescue, and extraction alone seeds it.
fn rescue_uploads(data_dir: &Path) -> Result<Option<PathBuf>, PortabilityError> {
    let uploads_dir = data_dir.join(UPLOADS_DIR);
    let non_empty = match fs::read_dir(&uploads_dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(PortabilityError::Io {
                path: uploads_dir,
                source,
            })
        }
    };
    if !non_empty {
        return Ok(None);
    }
    let rescue = lifecycle::move_rescue_dump_dir(data_dir, UPLOADS_DIR).map_err(|error| {
        PortabilityError::Io {
            path: error.path,
            source: error.source,
        }
    })?;
    Ok(Some(rescue))
}

/// The destination's disposable cache directories — the exact pair the launch
/// cache policy manages (`app_env::resolve`'s `Mode::Launch` wipe), and never
/// a third: `uploads/` is durable (decision 006) and `log/`/`sessions/` are
/// the app's own churn, none of them the import's to clear.
const CACHE_DIR_NAMES: [&str; 2] = ["cache", "build"];

/// Invalidate the destination's derived state before the archive's data
/// replaces the database it was derived from: discard the cache stamp, then
/// remove `cache/` and `build/` ([`CACHE_DIR_NAMES`]).
///
/// **Why the stamp first:** a partial cleanup must not leave a stamp claiming
/// the old container is still reusable — `read_cache_stamp` would catch an
/// emptied `cache/`, but a half-removed one would pass its
/// `cache_dir_has_entries` check and hand the next launch the leftovers.
///
/// **Why here at all:** the stamp compares the app's version, the snapshot
/// path and the PHP platform, never the database, so an equal-version restore
/// changes none of them and the previous container would otherwise be reused
/// against the imported database. `Mode::Run` and `Mode::Install` deliberately
/// clear nothing later in this pipeline either, so nothing else catches it.
///
/// **Strict, unlike `Mode::Launch`'s best-effort wipe:** a directory the
/// import could not remove is exactly the stale cache this exists to clear, so
/// the error stops the import before the rescue phase. An absent path counts
/// as already cleared, and a top-level symlink is removed as the link it is
/// rather than traversed into whatever it points at.
fn clear_destination_cache(data_dir: &Path, data_subdir: &Path) -> Result<(), PortabilityError> {
    lifecycle::discard_cache_stamp(data_subdir).map_err(|source| {
        PortabilityError::CacheCleanup {
            path: lifecycle::cache_stamp_path(data_subdir),
            stamp_discarded: false,
            source,
        }
    })?;
    for name in CACHE_DIR_NAMES {
        let path = data_dir.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(PortabilityError::CacheCleanup {
                    path,
                    stamp_discarded: true,
                    source,
                })
            }
        };
        let removal = if metadata.file_type().is_symlink() {
            fs::remove_file(&path)
        } else {
            fs::remove_dir_all(&path)
        };
        removal.map_err(|source| PortabilityError::CacheCleanup {
            path,
            stamp_discarded: true,
            source,
        })?;
    }
    Ok(())
}

/// Remove the live database set after [`rescue_dump`] has copied it aside and
/// before an archive writes its replacement. Extraction only creates files it
/// carries, so leaving an old WAL or SHM beside an archive that has only the
/// main database could silently replay transactions from the replaced one.
fn remove_live_db_files(data_subdir: &Path) -> Result<(), PortabilityError> {
    for name in lifecycle::DB_FILE_NAMES {
        let path = data_subdir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(PortabilityError::Io { path, source }),
        }
    }
    Ok(())
}

fn import_incomplete(
    data_subdir: &Path,
    db_rescue_path: Option<&Path>,
    uploads_rescue_path: Option<&Path>,
    data_state: &'static str,
    source: impl std::fmt::Display,
) -> PortabilityError {
    PortabilityError::ImportIncomplete {
        data_subdir: data_subdir.to_path_buf(),
        db_rescue_path: db_rescue_path.map(Path::to_path_buf),
        uploads_rescue_path: uploads_rescue_path.map(Path::to_path_buf),
        data_state,
        detail: source.to_string(),
    }
}

/// Which command [`PortabilityError::Busy`] was refusing — its message names
/// the risk in the command's own terms rather than a shared, blander one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Export,
    Import,
}

/// Everything that can stop `export` or `import`, in one type so both
/// commands have one place to print from — `rollback.rs`'s own
/// `RollbackError` is the template.
#[derive(Debug)]
pub enum PortabilityError {
    Paths(PathsError),
    Registry(RegistryError),
    Lifecycle(lifecycle::LifecycleError),
    Gate(GateError),
    /// Reloading the installed manifest, or running its `pre-update`/
    /// `post-update` commands, to migrate an older archive forward.
    Install(InstallError),
    Manifest(ManifestError),
    Php(PhpError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// No app is registered under this id at all.
    NotInstalled {
        id: String,
    },
    /// `export`'s target already exists.
    TargetExists {
        path: PathBuf,
    },
    /// `export`'s own unfinished archive is still present.
    TemporaryExists {
        path: PathBuf,
    },
    /// A live window or an active `run` command holds the data directory.
    Busy {
        id: String,
        holder: lifecycle::DataDirHolder,
        action: Action,
    },
    /// The archive at `path` has no `manifest.json` at its root.
    NoManifest {
        path: PathBuf,
    },
    /// `manifest.json` exists but is not readable as a [`Manifest`], or its
    /// `app_version` does not parse as semver.
    MalformedManifest {
        path: PathBuf,
        detail: String,
    },
    /// [`import_decision`] refused.
    Refused(ImportRefusal),
    /// An import stopped while clearing the destination's disposable cache,
    /// before the rescue/replacement boundary — the one import failure where
    /// no persistent data has been touched, which is why it is not an
    /// [`PortabilityError::ImportIncomplete`]: the database, `uploads/`, the
    /// recorded version and the rollback anchor are all still exactly as the
    /// preflight found them. What may have changed is the cache cleanup
    /// itself, and `stamp_discarded` says how far it got: `false` means the
    /// stamp removal is what failed, so the stamp is still exactly as found;
    /// `true` means the stamp is already gone and the directories may be
    /// half-removed. The message reports whichever is true rather than
    /// assuming the stamp is clear.
    CacheCleanup {
        path: PathBuf,
        stamp_discarded: bool,
        source: io::Error,
    },
    /// An import failed after its rescue dump was safely taken. The ordinary
    /// error alone is not enough: the caller also needs to know the state now
    /// left on disk and where the data it replaced can be recovered — both
    /// halves of it, the database and (since plan 049) `uploads/`.
    ImportIncomplete {
        data_subdir: PathBuf,
        db_rescue_path: Option<PathBuf>,
        uploads_rescue_path: Option<PathBuf>,
        data_state: &'static str,
        detail: String,
    },
}

impl fmt::Display for PortabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Lifecycle(error) => write!(formatter, "{error}"),
            Self::Gate(error) => write!(formatter, "{error}"),
            Self::Install(error) => write!(formatter, "{error}"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Php(error) => write!(formatter, "{error}"),
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
            Self::TemporaryExists { path } => write!(
                formatter,
                "{} already exists — it is left over from a failed export and must be \
                 removed by hand before exporting again.",
                path.display()
            ),
            Self::Busy { id, holder, action } => {
                let (verb, risk) = match action {
                    Action::Export => ("exporting", "copy its database mid-write"),
                    Action::Import => (
                        "importing",
                        "corrupt the database it writes into, or leave it reading a file that \
                         changed out from under it",
                    ),
                };
                match holder {
                    lifecycle::DataDirHolder::Window => write!(
                        formatter,
                        "{id} has a window open right now — {verb} while it's open could \
                         {risk}. Close {id} first."
                    ),
                    lifecycle::DataDirHolder::RunCommand { active } => {
                        match active.first().and_then(|run| run.alias.as_deref()) {
                            Some(alias) => write!(
                                formatter,
                                "{id}'s \"{alias}\" run command is still active — {verb} while \
                                 it's running could {risk}. Stop it first with `tfsapp-hub run \
                                 --stop {id}`."
                            ),
                            None => write!(
                                formatter,
                                "a run command is still active for {id} — {verb} while it's \
                                 running could {risk}. Stop it first with `tfsapp-hub run --stop \
                                 {id}`."
                            ),
                        }
                    }
                }
            }
            Self::NoManifest { path } => write!(
                formatter,
                "{}: no manifest.json at its root — this does not look like an archive \
                 `export` wrote.",
                path.display()
            ),
            Self::MalformedManifest { path, detail } => write!(
                formatter,
                "{}: its manifest.json is unreadable ({detail}) — this does not look like an \
                 archive `export` wrote.",
                path.display()
            ),
            Self::Refused(refusal) => write!(formatter, "{refusal}"),
            Self::CacheCleanup {
                path,
                stamp_discarded,
                source,
            } => {
                let cleanup_state = if *stamp_discarded {
                    "the cleanup may be partial (the cache stamp is already discarded)"
                } else {
                    "the cache stamp itself could not be discarded, so the cleanup has not \
                     progressed past it"
                };
                write!(
                    formatter,
                    "import stopped before replacing any data: clearing the previous cache \
                     failed at {}: {source}. {cleanup_state}, but the database, uploads/ and the \
                     rollback anchor are untouched — resolve the error and run the import again.",
                    path.display()
                )
            }
            Self::ImportIncomplete {
                data_subdir,
                db_rescue_path,
                uploads_rescue_path,
                data_state,
                detail,
            } => {
                write!(
                    formatter,
                    "import did not finish: {detail}. {data_state} in {}",
                    data_subdir.display()
                )?;
                match db_rescue_path {
                    Some(path) => write!(
                        formatter,
                        "; the database it replaced was saved to {}",
                        path.display()
                    )?,
                    None => write!(formatter, "; no existing database needed a rescue dump")?,
                }
                match uploads_rescue_path {
                    Some(path) => write!(
                        formatter,
                        "; the uploads/ directory it replaced was saved to {}",
                        path.display()
                    ),
                    None => write!(
                        formatter,
                        "; no existing uploads/ directory needed a rescue"
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
            Self::Lifecycle(error) => Some(error),
            Self::Gate(error) => Some(error),
            Self::Install(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Php(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::CacheCleanup { source, .. } => Some(source),
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

impl From<lifecycle::LifecycleError> for PortabilityError {
    fn from(error: lifecycle::LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

impl From<GateError> for PortabilityError {
    fn from(error: GateError) -> Self {
        Self::Gate(error)
    }
}

impl From<InstallError> for PortabilityError {
    fn from(error: InstallError) -> Self {
        Self::Install(error)
    }
}

impl From<ManifestError> for PortabilityError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<PhpError> for PortabilityError {
    fn from(error: PhpError) -> Self {
        Self::Php(error)
    }
}

#[cfg(test)]
#[path = "portability_tests.rs"]
mod tests;
