//! `tfsapp-hub publish <local-path>` — the app author's side of the release
//! contract (CONTRACT.md's "Publishing a release",
//! `../decision/003-the-hub-publishes-apps.md`).
//!
//! Written in the gate order `../plan/019-publish-an-app.md`'s (revised)
//! Overview table lists them: gates 1–8 are local — 1, 2, 7 and 8 pure over
//! the project directory, 3–6 one `git.rs` call each ([`run_local_gates`]);
//! gates 9–11 are `gh.rs`'s `Gh`; then the archive and its sums
//! ([`build_archive`]), the announcement, the confirmation, and
//! `gh release create`. [`run`] is the command `main.rs` reaches.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fmt, fs, io,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
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
/// the one commit [`Git::snapshot`] pinned.
pub struct LocalGates {
    pub loaded: Loaded,
    pub commit: Commit,
    pub notes: String,
    entries: Vec<TreeEntry>,
    blobs: BlobReader,
}

impl fmt::Debug for LocalGates {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalGates")
            .field("loaded", &self.loaded)
            .field("commit", &self.commit)
            .field("notes", &self.notes)
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
    repo: Option<&str>,
    git: &Git,
) -> Result<LocalGates, PublishError> {
    if let Some(repo) = repo {
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

    let snapshot = git.snapshot(project_path, repo)?;
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
        entries,
        blobs,
    })
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
/// `destination`, from explicit entries and blobs in one pinned Git tree.
///
/// The entries are filtered with [`source::EXCLUDED_FROM_HASH`] before any
/// blob is requested, which is what buys the property this step exists for:
/// `tree_hash` of the archive, once extracted, equals `tree_hash` of the
/// pinned tree minus those standing exclusions. A symlink whose target would
/// resolve outside the extracted tree is refused before anything is uploaded,
/// with the exact lexical rule `archive::extract` applies at the other end
/// (`archive::link_target_escapes`).
pub fn build_archive(
    entries: &[TreeEntry],
    blobs: &mut impl BlobSource,
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

    for (relative_path, entry) in archive_paths(entries) {
        let archive_path = archive_root.join(&relative_path);
        if let Some(entry) = entry {
            append_blob_entry(&mut builder, &archive_path, entry, blobs)?;
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

/// Sorted archive entries derived from Git's tracked files: every parent
/// directory once, before files below it. Git does not track empty directories,
/// so deriving them loses nothing.
fn archive_paths(entries: &[TreeEntry]) -> Vec<(PathBuf, Option<&TreeEntry>)> {
    let files: BTreeSet<PathBuf> = entries
        .iter()
        .filter(|entry| !excluded_from_archive(&entry.path))
        .map(|entry| entry.path.clone())
        .collect();
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

    let mut entries: Vec<_> = directories
        .into_iter()
        .map(|path| (path, None))
        .chain(files.into_iter().map(|path| {
            let entry = entries
                .iter()
                .find(|entry| entry.path == path)
                .expect("every archive path came from a tree entry");
            (path, Some(entry))
        }))
        .collect();
    entries.sort_by(|(left_path, left_entry), (right_path, right_entry)| {
        left_path
            .cmp(right_path)
            .then_with(|| right_entry.is_none().cmp(&left_entry.is_none()))
    });
    entries
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
pub fn run(project_path: &str, repo: Option<&str>, assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match publish(
        &paths,
        Path::new(project_path),
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
    let mut gates = run_local_gates(project_path, repo, git)?;
    let manifest = &gates.loaded.manifest;
    let tag = format!("v{}", manifest.app_version);

    gh.ensure_installed()?;
    gh.ensure_authenticated()?;
    gh.ensure_no_existing_release(&gates.commit.repo, &tag)?;

    let assets = build_archive(
        &gates.entries,
        &mut gates.blobs,
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
        &gates.commit.repo,
        &tag,
        &notes_path,
        &gates.commit.sha,
        &assets.archive_name,
        &assets.archive_path,
        &assets.sums_path,
    )?;

    println!("Published {tag} on {}", gates.commit.repo);
    println!("  {url}");
    println!();
    println!(
        "Users install it with: tfsapp-hub install github:{}",
        gates.commit.repo
    );

    Ok(true)
}

/// Say what is about to be published, in the terms the user will have to
/// reason about afterwards — `update::announce`'s counterpart for `publish`.
/// Names the repository, branch and commit the Git gate proved is on the
/// forge. Every displayed release input was read from that pinned commit.
fn announce(gates: &LocalGates, tag: &str, assets: &Assets) {
    println!("Repository  {}", gates.commit.repo);
    println!(
        "Commit      {} (branch {})",
        &gates.commit.sha[..gates.commit.sha.len().min(12)],
        gates.commit.branch
    );
    println!("Tag         {tag}");
    println!("Inputs      pinned Git tree");
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
/// this setting to every user who installs it.
fn confirm_ipc_secrets(manifest: &Manifest) -> Result<(), PublishError> {
    if !manifest.actions.secrets.ipc {
        return Ok(());
    }

    println!(
        "actions.secrets.ipc is enabled — declared secrets are reachable from the app's own \
         JS runtime, so an XSS in the app can read or overwrite them. IPC remains the only \
         transport where a secret's value never transits the PHP process."
    );
    match prompt::confirmed(false) {
        true => Ok(()),
        false => Err(PublishError::IpcNotConfirmed),
    }
}

/// Everything gates 1–11 can refuse over.
#[derive(Debug)]
pub enum PublishError {
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
    /// asked (see `prompt::confirmed`'s own non-terminal refusal).
    IpcNotConfirmed,
    /// A symlink in the project tree points outside it — the same lexical
    /// rule `archive::extract` applies at the other end
    /// (`archive::link_target_escapes`), applied here before a byte of the
    /// archive is written.
    EscapingSymlink {
        path: PathBuf,
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
