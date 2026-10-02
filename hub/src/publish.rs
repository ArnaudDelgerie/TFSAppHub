//! `tfsapp-hub publish <local-path>` — the app author's side of the release
//! contract (CONTRACT.md's "Publishing a release",
//! `../decision/003-the-hub-publishes-apps.md`).
//!
//! Written in the gate order `../plan/019-publish-an-app.md`'s (revised)
//! Overview table lists them: gates 1–8 are local — 1, 2, 7 and 8 pure over
//! the project directory, 3–6 one `git.rs` call each ([`run_local_gates`]);
//! gates 9–11 are `gh.rs`'s `Gh`; then the archive and its sums
//! ([`build_archive`]), the announcement, the confirmation, and
//! `gh release create`. Between the manifest gates (1–2) and the changelog
//! gate (7), the pinned manifest's declared `build_outputs` are checked and
//! walked on the working tree (plan 071): those directories are the archive's
//! one source that is not the pinned commit. [`run`] is the command
//! `main.rs` reaches.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fmt, fs, io,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStringExt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    archive,
    cli::{EXIT_FAILED, EXIT_OK},
    gh::{Gh, GhError},
    git::{BlobReader, Commit, Git, GitError, TreeEntry},
    manifest::{self, Loaded, Manifest, ManifestError, MANIFEST_FILE},
    paths::Paths,
    prompt,
    release::{self, ReleaseError},
    source, version,
};

/// The changelog's filename at the project root (CONTRACT.md §1/§7).
pub const CHANGELOG_FILE: &str = "CHANGELOG.md";

/// What gates 1–8 produce for the steps after them.  The entries and their
/// reader stay together with the metadata they supplied: all three are from
/// the one commit [`Git::snapshot`] pinned. The declared build outputs are
/// the deliberate exception — the only archive input that is not that
/// commit's, read from the working tree because the contract says the
/// author's own tool builds them (plan 071).
pub struct LocalGates {
    pub loaded: Loaded,
    pub commit: Commit,
    pub notes: String,
    pub build_outputs: Vec<DeclaredOutput>,
    entries: Vec<TreeEntry>,
    blobs: BlobReader,
}

/// One file under a declared build output, ready for the archive: its
/// project-relative path, the disk file its bytes stream from, and the
/// facts the tar header and the stats line need. Captured by the gate-time
/// walk, so what the confirmation describes is what ships.
#[derive(Debug)]
pub struct DeclaredFile {
    pub path: PathBuf,
    pub source: PathBuf,
    pub size: u64,
    pub executable: bool,
    pub modified: SystemTime,
}

/// One declared `build_outputs` directory, walked and measured once the
/// manifest gates have passed: the files the archive gains beside the
/// pinned tree, plus the stats printed before the confirmation. Nothing
/// here is re-read later — this list is what the archive writes.
#[derive(Debug)]
pub struct DeclaredOutput {
    /// The entry exactly as the manifest spells it, for the messages an
    /// author reads.
    pub declared: String,
    pub files: Vec<DeclaredFile>,
    pub total_size: u64,
    /// Never absent: an output with no file is refused before one is built.
    pub newest: SystemTime,
}

impl DeclaredOutput {
    /// The one line the announcement prints per output (plan 071's
    /// settled format):
    /// `build_outputs: public/build — 42 files, 1.3 MiB, newest 3 days ago`.
    /// Staleness is deliberately not detected — knowing whether a build is
    /// stale needs its inputs declared, which is a build tool's job — so
    /// the line states what is on disk instead.
    pub fn stats_line(&self) -> String {
        format!(
            "build_outputs: {} — {}, {}, newest {}",
            self.declared,
            counted(self.files.len() as u64, "file"),
            format_size(self.total_size),
            format_age(self.newest),
        )
    }
}

#[derive(Clone, Copy)]
pub enum PublishTarget<'a> {
    Forge(Option<&'a str>),
    Local,
}

impl fmt::Debug for LocalGates {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalGates")
            .field("loaded", &self.loaded)
            .field("commit", &self.commit)
            .field("notes", &self.notes)
            .field("build_outputs", &self.build_outputs)
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

/// Run every gate up to (not including) the `gh` seam, in the Overview
/// table's order after the repository-shape check: the `git` gate pins a
/// clean, pushed project commit (3–6), then its manifest supplies the
/// canonical-version check (1–2), and its changelog supplies the release
/// notes (7).  Only when
/// `actions.secrets.ipc` is on — block on a confirmation with no `--yes`
/// escape (8).
///
/// Nothing here touches `gh`: gates 9–11 are a later step in
/// [`publish_into`], run only once every gate here has passed.
pub fn run_local_gates(
    project_path: &Path,
    target: PublishTarget<'_>,
    git: &Git,
) -> Result<LocalGates, PublishError> {
    if let PublishTarget::Forge(Some(repo)) = target {
        if !is_owner_repo_shape(repo) {
            return Err(PublishError::InvalidRepoShape {
                repo: repo.to_string(),
            });
        }
    }
    // Refuse the author-visible manifest before even asking Git for a pinned
    // tree. The same validation runs again below on the pinned bytes, so a
    // change between the two checks cannot be published under a different
    // spelling.
    let manifest_path = project_path.join(MANIFEST_FILE);
    let author_manifest = manifest::load(project_path)?;
    validate_version(&author_manifest.manifest, &manifest_path)?;

    let snapshot = match target {
        PublishTarget::Forge(repo) => git.snapshot(project_path, repo)?,
        PublishTarget::Local => git.local_snapshot(project_path)?,
    };
    let entries = git.tree_entries(project_path, &snapshot)?;
    let mut blobs = git.blob_reader(project_path)?;
    let manifest_bytes = pinned_file(&entries, &mut blobs, MANIFEST_FILE)?.ok_or_else(|| {
        ManifestError::Unreadable {
            path: manifest_path.clone(),
            source: io::Error::from(io::ErrorKind::NotFound),
        }
    })?;
    let contents =
        std::str::from_utf8(&manifest_bytes).map_err(|error| ManifestError::Malformed {
            path: manifest_path.clone(),
            detail: error.to_string(),
        })?;
    let loaded = manifest::parse(&manifest_path, contents)?;
    // The pinned path remains an author-facing path, so an unknown key is
    // still actionable even though its bytes came from Git.
    loaded.report_warnings();
    validate_version(&loaded.manifest, &manifest_path)?;

    // Plan 071's working-tree checks, on the pinned manifest's declaration:
    // the declared outputs are the only bytes in the archive that are not
    // the pinned commit's, so each is proved embeddable before anything is
    // built. `install`, `update` and `dev` never come near them.
    let build_outputs = collect_build_outputs(project_path, &loaded.manifest, &entries, git)?;

    let changelog_path = project_path.join(CHANGELOG_FILE);
    let changelog = pinned_file(&entries, &mut blobs, CHANGELOG_FILE)?.ok_or_else(|| {
        PublishError::MissingChangelog {
            path: changelog_path.clone(),
        }
    })?;
    let notes = changelog_gate(&changelog_path, &changelog, &loaded.manifest.app_version)?;
    confirm_ipc_secrets(&loaded.manifest)?;

    Ok(LocalGates {
        loaded,
        commit: snapshot.commit,
        notes,
        build_outputs,
        entries,
        blobs,
    })
}

/// Walk every directory the pinned manifest declares under
/// `build_outputs` and prove each one is publishable (plan 071): the
/// lexical shape first — pure, and the only half that never touches the
/// working tree — then the filesystem, then Git's ignore rules and the
/// pinned tracked tree. The walk's product is the archive's second source
/// and the stats line the confirmation shows.
fn collect_build_outputs(
    project: &Path,
    manifest: &Manifest,
    entries: &[TreeEntry],
    git: &Git,
) -> Result<Vec<DeclaredOutput>, PublishError> {
    let paths = manifest::build_output_shapes(&manifest.build_outputs)?;
    let mut outputs = Vec::with_capacity(paths.len());
    for (declared, path) in manifest.build_outputs.iter().zip(paths) {
        outputs.push(check_build_output(project, declared, &path, entries, git)?);
    }
    Ok(outputs)
}

/// One declared output's gate: a project-relative directory that exists,
/// stays inside the project once resolved, holds files, is gitignored,
/// holds no tracked file the pinned tree already carries, and holds
/// nothing `install` would refuse to extract. Each refusal names the
/// declared path and its own reason.
fn check_build_output(
    project: &Path,
    declared: &str,
    path: &Path,
    entries: &[TreeEntry],
    git: &Git,
) -> Result<DeclaredOutput, PublishError> {
    let on_disk = project.join(path);
    let metadata = match fs::symlink_metadata(&on_disk) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(PublishError::BuildOutputAbsent {
                declared: declared.to_string(),
            });
        }
        Err(source) => {
            return Err(PublishError::Io {
                path: on_disk.clone(),
                source,
            });
        }
    };
    // `symlink_metadata`, so a symlink where a directory is declared is
    // refused as what it is rather than followed into whatever it points
    // at.
    if !metadata.is_dir() {
        return Err(PublishError::BuildOutputNotADirectory {
            declared: declared.to_string(),
        });
    }
    // "Inside the project once resolved": the shape check already refused
    // every lexical escape, so what remains is a symlinked parent — the
    // one case a join cannot see and only resolution can.
    let project_root = fs::canonicalize(project).map_err(|source| PublishError::Io {
        path: project.to_path_buf(),
        source,
    })?;
    let resolved = fs::canonicalize(&on_disk).map_err(|source| PublishError::Io {
        path: on_disk.clone(),
        source,
    })?;
    if !resolved.starts_with(&project_root) {
        return Err(PublishError::BuildOutputEscapes {
            declared: declared.to_string(),
        });
    }

    let mut files = Vec::new();
    walk_declared_files(project, declared, path, &mut files)?;
    if files.is_empty() {
        return Err(PublishError::BuildOutputEmpty {
            declared: declared.to_string(),
        });
    }
    // Tracked before ignored, and not only for the message: `git
    // check-ignore` answers "not ignored" for any path with tracked
    // content beneath it, so the question is only honest once no tracked
    // file lives under the declaration.
    if let Some(tracked) = entries.iter().find(|entry| entry.path.starts_with(path)) {
        return Err(PublishError::BuildOutputTrackedFile {
            declared: declared.to_string(),
            file: tracked.path.clone(),
        });
    }
    if !git.is_ignored(project, path)? {
        return Err(PublishError::BuildOutputNotIgnored {
            declared: declared.to_string(),
        });
    }

    let total_size = files.iter().map(|file| file.size).sum();
    let newest = files
        .iter()
        .map(|file| file.modified)
        .max()
        .expect("an output with no file was refused above");
    Ok(DeclaredOutput {
        declared: declared.to_string(),
        files,
        total_size,
        newest,
    })
}

/// Collect one declared output's files, refusing anything that is not a
/// regular file or a directory. The walk never follows symlinks — a
/// symlink is refused as an entry, and a symlinked parent was already
/// refused by the resolution check above.
fn walk_declared_files(
    project: &Path,
    declared: &str,
    directory: &Path,
    files: &mut Vec<DeclaredFile>,
) -> Result<(), PublishError> {
    let io_error = |path: PathBuf| move |source: io::Error| PublishError::Io { path, source };
    let directory_on_disk = project.join(directory);
    let entries = fs::read_dir(&directory_on_disk).map_err(io_error(directory_on_disk.clone()))?;
    for entry in entries {
        let entry = entry.map_err(io_error(directory_on_disk.clone()))?;
        let name = entry.file_name();
        let relative = directory.join(&name);
        let source = project.join(&relative);
        // `DirEntry::metadata` does not follow the entry itself, so a
        // symlink stays a symlink here rather than becoming whatever it
        // points at.
        let metadata = entry.metadata().map_err(io_error(source.clone()))?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            walk_declared_files(project, declared, &relative, files)?;
        } else if file_type.is_file() {
            let modified = metadata.modified().map_err(io_error(source.clone()))?;
            files.push(DeclaredFile {
                path: relative,
                source,
                size: metadata.len(),
                executable: metadata.permissions().mode() & 0o111 != 0,
                modified,
            });
        } else {
            return Err(PublishError::BuildOutputIrregularEntry {
                declared: declared.to_string(),
                entry: relative,
            });
        }
    }
    Ok(())
}

/// `<size>` in the one binary unit that describes it, one decimal wide —
/// the stats line is for a human deciding whether to publish, not for a
/// parser.
fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let size = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if size < KIB * KIB {
        format!("{:.1} KiB", size / KIB)
    } else if size < KIB * KIB * KIB {
        format!("{:.1} MiB", size / (KIB * KIB))
    } else {
        format!("{:.1} GiB", size / (KIB * KIB * KIB))
    }
}

/// `newest <age> ago`, in the one unit that describes the age. A clock
/// that answers with the future (skew, a touched file) reads as brand new
/// rather than as an error: the line is information, not a gate.
fn format_age(newest: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(newest)
        .map_or(0, |age| age.as_secs());
    if seconds < 60 {
        "less than a minute ago".to_string()
    } else if seconds < 3600 {
        format!("{} ago", counted(seconds / 60, "minute"))
    } else if seconds < 86_400 {
        format!("{} ago", counted(seconds / 3600, "hour"))
    } else {
        format!("{} ago", counted(seconds / 86_400, "day"))
    }
}

/// `1 file`, `2 files` — the stats line's counts, singular when there is
/// one.
fn counted(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// Copy a named project-root file from the pinned tree.  Metadata is small
/// enough to parse in memory; archive payloads continue to stream one at a
/// time through the same reader.
fn pinned_file(
    entries: &[TreeEntry],
    blobs: &mut impl BlobSource,
    name: &str,
) -> Result<Option<Vec<u8>>, GitError> {
    let Some(entry) = entries.iter().find(|entry| entry.path == Path::new(name)) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    blobs.copy_blob(&entry.object_id, &mut bytes)?;
    Ok(Some(bytes))
}

/// The two files a published release carries, built into a directory the
/// caller provides (the hub's scratch directory in production — step 4 — a
/// temp directory in this module's own tests).
#[derive(Debug)]
pub struct Assets {
    pub archive_path: PathBuf,
    pub archive_name: String,
    pub archive_size: u64,
    pub sums_path: PathBuf,
    pub sha256: String,
}

/// A bounded source of pinned Git blobs.  It intentionally exposes copying,
/// not whole-blob reads: the archive consumes one object at a time.
pub(crate) trait BlobSource {
    fn copy_blob(&mut self, object: &str, destination: &mut dyn Write) -> Result<u64, GitError>;
}

impl BlobSource for BlobReader {
    fn copy_blob(&mut self, object: &str, destination: &mut dyn Write) -> Result<u64, GitError> {
        BlobReader::copy_blob(self, object, destination)
    }
}

/// Build `<project_name>-<app_version>.tar.gz` and its `SHA256SUMS.txt` into
/// `destination`, from explicit entries and blobs in one pinned Git tree,
/// plus the declared build outputs walked at gate time (plan 071) — the
/// archive's one deliberate second source, streamed from the working tree.
///
/// The tracked entries are filtered with [`source::EXCLUDED_FROM_HASH`]
/// before any blob is requested, which is what buys the property this step
/// exists for: `tree_hash` of the archive, once extracted, equals
/// `tree_hash` of the pinned tree minus those standing exclusions — plus
/// the declared outputs, which the walk proved to hold nothing
/// `tree_hash` cannot hash. A symlink whose target would resolve outside
/// the extracted tree is refused before anything is uploaded, with the
/// exact lexical rule `archive::extract` applies at the other end
/// (`archive::link_target_escapes`).
pub fn build_archive(
    entries: &[TreeEntry],
    blobs: &mut impl BlobSource,
    declared_outputs: &[DeclaredOutput],
    project_name: &str,
    app_version: &str,
    destination: &Path,
) -> Result<Assets, PublishError> {
    fs::create_dir_all(destination).map_err(|source| PublishError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    let prefix = format!("{project_name}-{app_version}");
    let archive_name = format!("{prefix}.tar.gz");
    let archive_path = destination.join(&archive_name);

    let file = fs::File::create(&archive_path).map_err(|source| PublishError::Io {
        path: archive_path.clone(),
        source,
    })?;
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    // Every mainstream tar preserves symlinks by default; this crate's
    // default is the other way (see `Builder::follow_symlinks`'s own
    // warning), and following one here would silently swap the byte content
    // of a symlink `source::tree_hash` never reads for the content of
    // whatever it points at.
    let archive_root = PathBuf::from(&prefix);
    append_directory(&mut builder, &archive_root)?;

    for (relative_path, source) in archive_paths(entries, declared_outputs) {
        let archive_path = archive_root.join(&relative_path);
        if let Some(source) = source {
            match source {
                ArchiveSource::Tracked(entry) => {
                    append_blob_entry(&mut builder, &archive_path, entry, blobs)?
                }
                ArchiveSource::Declared(file) => {
                    append_declared_file(&mut builder, &archive_path, file)?
                }
            }
            continue;
        }
        append_directory(&mut builder, &archive_path)?;
    }

    let encoder = builder.into_inner().map_err(|source| PublishError::Io {
        path: archive_path.clone(),
        source,
    })?;
    let file = encoder.finish().map_err(|source| PublishError::Io {
        path: archive_path.clone(),
        source,
    })?;
    let archive_size = file
        .metadata()
        .map_err(|source| PublishError::Io {
            path: archive_path.clone(),
            source,
        })?
        .len();

    let hash = release::sha256_file(&archive_path)?;
    let sums_path = destination.join(release::SHA256SUMS_ASSET_NAME);
    fs::write(&sums_path, format!("{hash}  {archive_name}\n")).map_err(|source| {
        PublishError::Io {
            path: sums_path.clone(),
            source,
        }
    })?;

    Ok(Assets {
        archive_path,
        archive_name,
        archive_size,
        sums_path,
        sha256: hash,
    })
}

/// Where one archive entry's bytes and header come from: a pinned Git
/// blob, or a declared build-output file streamed from the working tree.
#[derive(Clone, Copy)]
enum ArchiveSource<'a> {
    Tracked(&'a TreeEntry),
    Declared(&'a DeclaredFile),
}

/// Sorted archive entries derived from Git's tracked files plus the
/// declared build outputs: every parent directory once, before files
/// below it. Git does not track empty directories, and a declared output
/// with no file in it never reaches the archive, so deriving them loses
/// nothing.
fn archive_paths<'a>(
    entries: &'a [TreeEntry],
    declared_outputs: &'a [DeclaredOutput],
) -> Vec<(PathBuf, Option<ArchiveSource<'a>>)> {
    let mut files: BTreeSet<PathBuf> = entries
        .iter()
        .filter(|entry| !excluded_from_archive(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
    for output in declared_outputs {
        files.extend(output.files.iter().map(|file| file.path.clone()));
    }
    let mut directories = BTreeSet::new();
    for path in &files {
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_path_buf());
            parent = directory.parent();
        }
    }

    let mut sorted: Vec<_> = directories
        .into_iter()
        .map(|path| (path, None))
        .chain(files.into_iter().map(|path| {
            let source = entries
                .iter()
                .find(|entry| entry.path == path)
                .map(ArchiveSource::Tracked)
                .or_else(|| {
                    declared_outputs
                        .iter()
                        .flat_map(|output| output.files.iter())
                        .find(|file| file.path == path)
                        .map(ArchiveSource::Declared)
                })
                .expect("every archive path came from a tree entry or a declared output");
            (path, Some(source))
        }))
        .collect();
    sorted.sort_by(|(left_path, left_source), (right_path, right_source)| {
        left_path
            .cmp(right_path)
            .then_with(|| right_source.is_none().cmp(&left_source.is_none()))
    });
    sorted
}

fn append_directory<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &Path,
) -> Result<(), PublishError> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_mode(0o755);
    header.set_size(0);
    header.set_cksum();
    builder
        .append_data(&mut header, path, io::empty())
        .map_err(|source| PublishError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn append_blob_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &Path,
    entry: &TreeEntry,
    blobs: &mut impl BlobSource,
) -> Result<(), PublishError> {
    let mut blob = tempfile::tempfile().map_err(|source| PublishError::Io {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let size = blobs.copy_blob(&entry.object_id, &mut blob)?;
    blob.seek(SeekFrom::Start(0))
        .map_err(|source| PublishError::Io {
            path: archive_path.to_path_buf(),
            source,
        })?;

    let mut header = tar::Header::new_gnu();
    header.set_mode(if entry.is_executable() { 0o755 } else { 0o644 });
    if entry.is_symlink() {
        let mut target = Vec::new();
        blob.read_to_end(&mut target)
            .map_err(|source| PublishError::Io {
                path: archive_path.to_path_buf(),
                source,
            })?;
        let target = PathBuf::from(OsString::from_vec(target));
        if archive::link_target_escapes(archive_path, &target) {
            return Err(PublishError::EscapingSymlink {
                path: entry.path.clone(),
            });
        }
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header
            .set_link_name(target)
            .map_err(|source| PublishError::Io {
                path: archive_path.to_path_buf(),
                source,
            })?;
        header.set_cksum();
        return builder
            .append_data(&mut header, archive_path, io::empty())
            .map_err(|source| PublishError::Io {
                path: archive_path.to_path_buf(),
                source,
            });
    }

    header.set_size(size);
    header.set_cksum();
    builder
        .append_data(&mut header, archive_path, blob)
        .map_err(|source| PublishError::Io {
            path: archive_path.to_path_buf(),
            source,
        })
}

/// One declared build-output file, streamed from the working tree — the
/// author's bytes at publish time, the one archive input that is not the
/// pinned commit's (plan 071). The same header scheme as a tracked blob:
/// mode from the file's own executable bit as the gate-time walk read it,
/// no mtime, no owner.
///
/// The header's size is the walk's, and `tar` copies whatever the reader
/// yields without checking it against the header: a file a still-running
/// build rewrote since the walk would desynchronise the archive, and the
/// checksum computed afterwards would vouch for it. So exactly the walk's
/// size is read, and a file that turns out shorter or longer is refused —
/// the archive is then discarded with its scratch directory.
fn append_declared_file<W: Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &Path,
    file: &DeclaredFile,
) -> Result<(), PublishError> {
    let io_error = |source| PublishError::Io {
        path: file.source.clone(),
        source,
    };
    let mut source = fs::File::open(&file.source).map_err(io_error)?;
    let mut header = tar::Header::new_gnu();
    header.set_mode(if file.executable { 0o755 } else { 0o644 });
    header.set_size(file.size);
    header.set_cksum();
    let mut limited = (&mut source).take(file.size);
    builder
        .append_data(&mut header, archive_path, &mut limited)
        .map_err(|source| PublishError::Io {
            path: archive_path.to_path_buf(),
            source,
        })?;
    let shorter = limited.limit() != 0;
    let longer = source.read(&mut [0; 1]).map_err(io_error)? != 0;
    if shorter || longer {
        return Err(PublishError::BuildOutputChanged {
            file: file.path.clone(),
        });
    }
    Ok(())
}

/// Whether a Git-tracked path is still excluded from the archive because a
/// local install and `tree_hash` both exclude its top-level component.
fn excluded_from_archive(path: &Path) -> bool {
    path.components().next().is_some_and(|component| {
        source::EXCLUDED_FROM_HASH
            .iter()
            .any(|excluded| component.as_os_str() == *excluded)
    })
}

/// `tfsapp-hub publish <local-path>` — resolve `Paths`, run the pipeline into
/// the hub's own scratch directory, and turn the result into an exit code.
pub fn run(project_path: &str, repo: Option<&str>, local: Option<&str>, assume_yes: bool) -> i32 {
    if let Some(out_dir) = local {
        return match publish_local(
            // Both anchored at the invoking directory (`owd`): under the
            // AppImage the process's cwd is the image's own mount, so a
            // relative `publish myapp --local out/` read against it would
            // look inside the AppImage.
            &crate::owd::resolve_argument(Path::new(project_path)),
            &crate::owd::resolve_argument(Path::new(out_dir)),
            assume_yes,
            &Git::new(),
        ) {
            Ok(true) => EXIT_OK,
            Ok(false) => EXIT_FAILED,
            Err(error) => {
                eprintln!("tfsapp-hub: {error}");
                EXIT_FAILED
            }
        };
    }
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match publish(
        &paths,
        // Same anchor as the `--local` branch above.
        &crate::owd::resolve_argument(Path::new(project_path)),
        repo,
        assume_yes,
        &Git::new(),
        &Gh::new(),
    ) {
        Ok(true) => EXIT_OK,
        // Declining is not a failure of the command, but nothing was
        // published either — a script reading 0 would conclude it was.
        Ok(false) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The pipeline, in the Overview table's order: gates 1–8
/// ([`run_local_gates`]), gates 9–11 (`gh`, below), the archive and its sums
/// built into scratch, the announcement, the confirmation, then
/// `gh release create`. `false` means the user declined — everything up to
/// that point already ran, but nothing was uploaded.
///
/// Takes `Paths`, a [`Git`] and a [`Gh`] rather than resolving/constructing
/// them, matching `update::update` — what lets the whole pipeline run in a
/// test against a throwaway scratch directory and fake `git`/`gh`.
pub(crate) fn publish(
    paths: &Paths,
    project_path: &Path,
    repo: Option<&str>,
    assume_yes: bool,
    git: &Git,
    gh: &Gh,
) -> Result<bool, PublishError> {
    // Same reasoning as `install::install`'s own wrapper: the archive and its
    // sums are built into it, and it is removed on the way out regardless of
    // how this call ends — the hub writes nothing into the project itself
    // (the plan's "Where it is built, and what is left behind").
    let scratch = paths.scratch_dir();
    let result = publish_into(&scratch, project_path, repo, assume_yes, git, gh);
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn publish_into(
    scratch: &Path,
    project_path: &Path,
    repo: Option<&str>,
    assume_yes: bool,
    git: &Git,
    gh: &Gh,
) -> Result<bool, PublishError> {
    let mut gates = run_local_gates(project_path, PublishTarget::Forge(repo), git)?;
    let repository = gates
        .commit
        .repo
        .as_deref()
        .expect("forge snapshot has repository");
    let manifest = &gates.loaded.manifest;
    let tag = format!("v{}", manifest.app_version);

    gh.ensure_installed()?;
    gh.ensure_authenticated()?;
    gh.ensure_no_existing_release(repository, &tag)?;

    let assets = build_archive(
        &gates.entries,
        &mut gates.blobs,
        &gates.build_outputs,
        &manifest.project_name,
        &manifest.app_version,
        scratch,
    )?;
    let notes_path = scratch.join("NOTES.md");
    fs::write(&notes_path, &gates.notes).map_err(|source| PublishError::Io {
        path: notes_path.clone(),
        source,
    })?;

    announce(&gates, &tag, &assets);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was published.");
        return Ok(false);
    }

    let url = gh.create_release(
        repository,
        &tag,
        &notes_path,
        &gates.commit.sha,
        &assets.archive_name,
        &assets.archive_path,
        &assets.sums_path,
    )?;

    println!("Published {tag} on {repository}");
    println!("  {url}");
    println!();
    println!(
        "Users install it with: tfsapp-hub install github:{}",
        repository
    );

    Ok(true)
}

/// Build a release on the destination filesystem, then expose it with one rename.
pub(crate) fn publish_local(
    project_path: &Path,
    out_dir: &Path,
    assume_yes: bool,
    git: &Git,
) -> Result<bool, PublishError> {
    if !out_dir.is_dir() {
        return Err(PublishError::OutputDirMissing {
            path: out_dir.to_path_buf(),
        });
    }
    let mut gates = run_local_gates(project_path, PublishTarget::Local, git)?;
    let manifest = &gates.loaded.manifest;
    let folder = out_dir.join(format!(
        "{}-{}",
        manifest.project_name, manifest.app_version
    ));
    if folder.exists() {
        return Err(PublishError::LocalReleaseExists { path: folder });
    }
    let temporary = tempfile::Builder::new()
        .prefix(".tfsapp-release-")
        .tempdir_in(out_dir)
        .map_err(|source| PublishError::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;
    let assets = build_archive(
        &gates.entries,
        &mut gates.blobs,
        &gates.build_outputs,
        &manifest.project_name,
        &manifest.app_version,
        temporary.path(),
    )?;
    let notes_path = temporary.path().join("NOTES.md");
    let notes = format!(
        "{}\n\nBuilt from commit {}\n",
        gates.notes, gates.commit.sha
    );
    fs::write(&notes_path, &notes).map_err(|source| PublishError::Io {
        path: notes_path.clone(),
        source,
    })?;
    for file in [&assets.archive_path, &assets.sums_path, &notes_path] {
        fs::File::open(file)
            .and_then(|handle| handle.sync_all())
            .map_err(|source| PublishError::Io {
                path: file.to_path_buf(),
                source,
            })?;
    }
    fs::File::open(temporary.path())
        .and_then(|handle| handle.sync_all())
        .map_err(|source| PublishError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;

    println!(
        "Commit      {} (branch {})",
        gates.commit.sha, gates.commit.branch
    );
    println!("Inputs      {}", inputs_description(&gates.build_outputs));
    for output in &gates.build_outputs {
        println!("{}", output.stats_line());
    }
    println!("Destination {}", folder.display());
    println!(
        "Archive     {} ({} bytes)",
        assets.archive_name, assets.archive_size
    );
    println!("  sha256    {}", assets.sha256);
    println!("Notes:\n{notes}");
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was published.");
        return Ok(false);
    }
    // The target was checked before building; recheck after confirmation too.
    if folder.exists() {
        return Err(PublishError::LocalReleaseExists { path: folder });
    }
    fs::rename(temporary.path(), &folder).map_err(|source| PublishError::Io {
        path: folder.clone(),
        source,
    })?;
    // The directory now lives under its final name; the guard must not try
    // to remove the old one on drop.
    let _ = temporary.keep();
    fs::File::open(out_dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| PublishError::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;
    println!("Published {}", folder.display());
    println!(
        "Install it with: tfsapp-hub install {}",
        folder.join(&assets.archive_name).display()
    );
    Ok(true)
}

/// Say what is about to be published, in the terms the user will have to
/// reason about afterwards — `update::announce`'s counterpart for `publish`.
/// Names the repository, branch and commit the Git gate proved is on the
/// forge. Every displayed release input was read from that pinned commit,
/// except the declared build outputs, named on their own lines as the
/// author's bytes at publish time.
fn announce(gates: &LocalGates, tag: &str, assets: &Assets) {
    println!(
        "Repository  {}",
        gates
            .commit
            .repo
            .as_deref()
            .expect("forge snapshot has repository")
    );
    println!(
        "Commit      {} (branch {})",
        &gates.commit.sha[..gates.commit.sha.len().min(12)],
        gates.commit.branch
    );
    println!("Tag         {tag}");
    println!("Inputs      {}", inputs_description(&gates.build_outputs));
    for output in &gates.build_outputs {
        println!("{}", output.stats_line());
    }
    println!(
        "Archive     {} ({} bytes)",
        assets.archive_name, assets.archive_size
    );
    println!("  sha256    {}", assets.sha256);
    println!("Checksums   {}", release::SHA256SUMS_ASSET_NAME);
    println!();
    println!("Notes:");
    for line in gates.notes.lines() {
        println!("  {line}");
    }
}

/// The `Inputs` line: the pinned Git tree alone, or with the declared
/// build outputs appended — the archive's two sources, named so the cost
/// of the declaration is stated, not hidden.
fn inputs_description(build_outputs: &[DeclaredOutput]) -> &'static str {
    if build_outputs.is_empty() {
        "pinned Git tree"
    } else {
        "pinned Git tree + build outputs"
    }
}

/// Gate 2: `app_version` must have the contract's canonical spelling — the value a
/// published tag and archive name are both built from, and CONTRACT.md §2's
/// own requirement.
fn validate_version(manifest: &Manifest, manifest_path: &Path) -> Result<(), PublishError> {
    version::parse_app_version(&manifest.app_version).map_err(|error| {
        PublishError::UnusableVersion {
            path: manifest_path.to_path_buf(),
            version: manifest.app_version.clone(),
            detail: error.to_string(),
        }
    })?;
    Ok(())
}

/// Whether `repo` is exactly one non-empty `owner`, a `/`, and one non-empty
/// `repo` — no leading, trailing or doubled slash. Applied to `--repo` before
/// it is trusted, since [`Git::ensure_pushed`]'s own remote-URL resolution
/// produces this shape by construction and needs no second check.
fn is_owner_repo_shape(repo: &str) -> bool {
    let mut segments = repo.split('/');
    let owner = segments.next().filter(|segment| !segment.is_empty());
    let name = segments.next().filter(|segment| !segment.is_empty());
    owner.is_some() && name.is_some() && segments.next().is_none()
}

/// Gate 7: `CHANGELOG.md` must exist at the project root and carry a heading
/// for `version`. The matched section becomes the release notes, verbatim.
fn changelog_gate(path: &Path, contents: &[u8], version: &str) -> Result<String, PublishError> {
    let contents = std::str::from_utf8(contents).map_err(|_| PublishError::MissingChangelog {
        path: path.to_path_buf(),
    })?;
    changelog_section(contents, version).ok_or(PublishError::MissingChangelogEntry {
        path: path.to_path_buf(),
        version: version.to_string(),
    })
}

/// The version a changelog heading line names, or `None` when the line is
/// not a level-2 heading at all, or names a different version.
///
/// The three accepted spellings (CONTRACT.md §7): `## 1.2.0`, `## v1.2.0`,
/// `## [1.2.0]`, each optionally followed by more text on the same line — a
/// date, a link — which is never inspected, only the version token is. This
/// is also what keeps `## 1.2.0.1` from ever answering for `1.2.0`: its
/// token is `1.2.0.1`, which simply does not equal it.
///
/// The gate (whether *some* heading matches) and the extractor
/// ([`changelog_section`]) both call this one function, so they cannot drift
/// apart the way two independently written patterns could.
fn heading_version(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("## ")?.trim_start();
    let candidate = match rest.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next()?,
        None => rest.split_whitespace().next()?,
    };
    Some(candidate.strip_prefix('v').unwrap_or(candidate))
}

/// The section under the heading matching `version` exactly, running to the
/// next `## ` heading or the end of the file — `None` when no heading in
/// `contents` names `version`.
fn changelog_section(contents: &str, version: &str) -> Option<String> {
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines
        .iter()
        .position(|line| heading_version(line) == Some(version))?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("## "))
        .map_or(lines.len(), |offset| start + 1 + offset);
    Some(lines[start + 1..end].join("\n").trim().to_string())
}

/// Gate 8: `actions.secrets.ipc` off is silent. On, it blocks on a
/// confirmation whose wording is carried over from the station's
/// `release.sh` — and, deliberately, has no `--yes` escape: a release ships
/// this setting to every user who installs it, so away from a terminal the
/// gate's own refusal says the confirmation is only given at a terminal.
fn confirm_ipc_secrets(manifest: &Manifest) -> Result<(), PublishError> {
    if !manifest.actions.secrets.ipc {
        return Ok(());
    }

    println!(
        "actions.secrets.ipc is enabled — declared secrets are reachable from the app's own \
         JS runtime, so an XSS in the app can read or overwrite them. IPC remains the only \
         transport where a secret's value never transits the PHP process."
    );
    match prompt::confirmed_at_terminal() {
        true => Ok(()),
        false => Err(PublishError::IpcNotConfirmed),
    }
}

/// Everything gates 1–11 can refuse over.
#[derive(Debug)]
pub enum PublishError {
    OutputDirMissing {
        path: PathBuf,
    },
    LocalReleaseExists {
        path: PathBuf,
    },
    Manifest(ManifestError),
    /// `app_version` does not parse as canonical semver.
    UnusableVersion {
        path: PathBuf,
        version: String,
        detail: String,
    },
    /// `--repo` is not `owner/repo`.
    InvalidRepoShape {
        repo: String,
    },
    /// Gates 3–6: the project is not committed and pushed — `git.rs`'s own
    /// taxonomy.
    Git(GitError),
    /// No `CHANGELOG.md` at the project root at all.
    MissingChangelog {
        path: PathBuf,
    },
    /// `CHANGELOG.md` exists but carries no heading for this version.
    MissingChangelogEntry {
        path: PathBuf,
        version: String,
    },
    /// The `actions.secrets.ipc` confirmation was declined, or could not be
    /// asked (see `prompt::confirmed_at_terminal`'s own non-terminal
    /// refusal).
    IpcNotConfirmed,
    /// A symlink in the project tree points outside it — the same lexical
    /// rule `archive::extract` applies at the other end
    /// (`archive::link_target_escapes`), applied here before a byte of the
    /// archive is written.
    EscapingSymlink {
        path: PathBuf,
    },
    /// A declared `build_outputs` directory resolves outside the project —
    /// the shape check already refused every lexical escape, so this one
    /// is a symlinked parent, which only resolution can see.
    BuildOutputEscapes {
        declared: String,
    },
    /// A declared `build_outputs` directory does not exist on disk.
    BuildOutputAbsent {
        declared: String,
    },
    /// A declared `build_outputs` path exists but is not a directory.
    BuildOutputNotADirectory {
        declared: String,
    },
    /// A declared `build_outputs` directory holds no file at all.
    BuildOutputEmpty {
        declared: String,
    },
    /// A declared `build_outputs` directory is not covered by Git's
    /// ignore rules.
    BuildOutputNotIgnored {
        declared: String,
    },
    /// A declared `build_outputs` directory holds a file the pinned
    /// tracked tree already carries.
    BuildOutputTrackedFile {
        declared: String,
        file: PathBuf,
    },
    /// A symlink or special file under a declared `build_outputs`
    /// directory — the extraction rule `install` applies (plan 062),
    /// applied here first.
    BuildOutputIrregularEntry {
        declared: String,
        entry: PathBuf,
    },
    /// A declared build-output file changed size between the gate-time
    /// walk and the archive — a build still running while publishing.
    BuildOutputChanged {
        file: PathBuf,
    },
    /// Reading the project tree, or writing the archive or its checksums,
    /// failed.
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// Hashing the archive for `SHA256SUMS.txt` failed — `release.rs`'s own
    /// taxonomy, reported here because it is exactly as much a reason the
    /// archive could not be produced as an `Io` failure is.
    Release(ReleaseError),
    /// Gates 9–11, or the final `gh release create`, refused — `gh.rs`'s own
    /// taxonomy.
    Gh(GhError),
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirMissing { path } => write!(
                formatter,
                "output directory {} must already exist and be a directory.",
                path.display()
            ),
            Self::LocalReleaseExists { path } => write!(
                formatter,
                "local release {} already exists — bump app_version before publishing again.",
                path.display()
            ),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::UnusableVersion {
                path,
                version,
                detail,
            } => write!(
                formatter,
                "\"app_version\" is {version:?} in {}, which is not canonical semver \
                 ({detail}) — the published tag and archive name are both built from it \
                 (CONTRACT.md §2).",
                path.display()
            ),
            Self::InvalidRepoShape { repo } => {
                write!(formatter, "--repo is {repo:?}, which is not owner/repo.")
            }
            Self::Git(error) => write!(formatter, "{error}"),
            Self::MissingChangelog { path } => write!(
                formatter,
                "no {CHANGELOG_FILE} found at {} — required to publish (CONTRACT.md §7).",
                path.display()
            ),
            Self::MissingChangelogEntry { path, version } => write!(
                formatter,
                "{} has no \"## {version}\" heading for app_version {version} — \"## v{version}\" \
                 and \"## [{version}]\" are accepted too, optionally followed by a date or a \
                 link (CONTRACT.md §7).",
                path.display()
            ),
            Self::IpcNotConfirmed => write!(
                formatter,
                "publish aborted — actions.secrets.ipc gate not confirmed."
            ),
            Self::EscapingSymlink { path } => write!(
                formatter,
                "{} is a symlink pointing outside the project tree — the hub's own installer \
                 would refuse to extract an archive carrying it, so publish refuses to build \
                 one.",
                path.display()
            ),
            Self::BuildOutputEscapes { declared } => write!(
                formatter,
                "\"build_outputs\" entry {declared:?} resolves outside the project — the \
                 archive would reach outside it, so publish refuses to build one \
                 (CONTRACT.md §2)."
            ),
            Self::BuildOutputAbsent { declared } => write!(
                formatter,
                "\"build_outputs\" entry {declared:?} does not exist in the project — build \
                 the output before publishing (CONTRACT.md §2)."
            ),
            Self::BuildOutputNotADirectory { declared } => write!(
                formatter,
                "\"build_outputs\" entry {declared:?} is not a directory (CONTRACT.md §2)."
            ),
            Self::BuildOutputEmpty { declared } => write!(
                formatter,
                "\"build_outputs\" entry {declared:?} is empty — build the output before \
                 publishing (CONTRACT.md §2)."
            ),
            Self::BuildOutputNotIgnored { declared } => write!(
                formatter,
                "\"build_outputs\" entry {declared:?} is not gitignored — a declared output \
                 and a tracked file are two different things, and an unignored directory \
                 would also make the tree dirty (CONTRACT.md §2)."
            ),
            Self::BuildOutputTrackedFile { declared, file } => write!(
                formatter,
                "\"build_outputs\" entry {declared:?} holds tracked file {}, which the \
                 archive already carries from the pinned commit — untrack it or undeclare \
                 it (CONTRACT.md §2).",
                file.display()
            ),
            Self::BuildOutputIrregularEntry { declared, entry } => write!(
                formatter,
                "{} under \"build_outputs\" entry {declared:?} is neither a regular file \
                 nor a directory — the hub's own installer would refuse to extract an \
                 archive carrying it, so publish refuses to build one (CONTRACT.md §2).",
                entry.display()
            ),
            Self::BuildOutputChanged { file } => write!(
                formatter,
                "{} changed while publish was reading it — nothing was published; \
                 let the build finish, then publish again.",
                file.display()
            ),
            Self::Io { path, source } => {
                write!(formatter, "{}: {source}", path.display())
            }
            Self::Release(error) => write!(formatter, "{error}"),
            Self::Gh(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Git(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Release(error) => Some(error),
            Self::Gh(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ManifestError> for PublishError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<GitError> for PublishError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

impl From<ReleaseError> for PublishError {
    fn from(error: ReleaseError) -> Self {
        Self::Release(error)
    }
}

impl From<GhError> for PublishError {
    fn from(error: GhError) -> Self {
        Self::Gh(error)
    }
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
