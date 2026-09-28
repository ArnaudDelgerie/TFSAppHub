//! What a `<source>` argument names, and how the named thing becomes a local
//! directory the installer can copy.
//!
//! A source is one of exactly two installable kinds (decision
//! `009-a-release-is-an-archive-wherever-it-lives.md`): a forge release
//! (`github:owner/repo`) and a local release archive (`<name>-<version>.tar.gz`
//! with its `SHA256SUMS.txt` beside it). A working directory is not one —
//! `dev <dir>` serves a tree in place, and `publish <dir> --local` is how a
//! tree becomes an archive worth installing — so [`classify`]'s fallback and
//! a directory named like an archive are recognised only so [`resolve`] can
//! refuse them well, naming the route that works.
//!
//! For a release, "fetch" means the whole of
//! `../plan/018-remote-sources-releases.md`'s step 3: `release::fetch_latest_release_at`
//! or `fetch_release_by_tag_at`, `release::resolve_assets`, `download_to` into
//! the caller's `scratch` directory, `verify` against `SHA256SUMS.txt`, then
//! `archive::extract` — in that order, so nothing is extracted until the
//! checksum matches and nothing is returned until the extraction has been
//! walked.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    archive::{self, ArchiveError},
    manifest::{self, ManifestError},
    registry::{ReferenceKind, Source, SourceKind},
    release::{self, ReleaseError},
    version,
};

/// Top-level directories left out of a local source's content hash.
///
/// Dependency trees and build output, none of it the developer's source: they
/// would make the hash both enormous and noisy — `var/cache` alone changes on
/// every request the app serves, which would report "changed since install"
/// forever. `tfsapp_build/` is the same story one host over: it holds the
/// station's `make build` AppImage, ~170 MB of output that changes on every
/// build, and hashing it would report "changed since install" for a build that
/// touched no source at all. Top-level only, so a legitimately named `src/var/`
/// still counts.
///
/// `pub(crate)` so `publish.rs` filters Git's tracked file list with the
/// exact predicate `tree_hash` uses, rather than a second list that could drift
/// from it — the property `../plan/029-publish-ships-the-tracked-tree-only.md`
/// exists for is `tree_hash` of a published archive, once extracted, equalling
/// `tree_hash` of the tracked tree minus these exclusions.
pub(crate) const EXCLUDED_FROM_HASH: &[&str] =
    &[".git", "vendor", "var", "node_modules", "tfsapp_build"];

/// What a `<source>` argument names, decided from the string alone.
///
/// The test is on the *string*, not on what exists on disk: a URL that names
/// no directory must still be reported as a release, never as an unrecognised
/// path — the two errors send a reader in opposite directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    LocalArchive(PathBuf),
    /// Anything the grammar does not name — recognised only so [`resolve`]
    /// can refuse it well, like [`Origin::GitSpelling`]: an existing
    /// directory is told the `publish --local` route, everything else that
    /// the hub installs exactly two kinds of source.
    Unrecognised(PathBuf),
    /// A repository to fetch a release of. `index` names how it was found —
    /// `"github"`, the forge acting as its own index today — so a later,
    /// curated index has somewhere to record its own name instead
    /// (`../plan/018-remote-sources-releases.md`'s Overview §2).
    Release {
        index: Option<String>,
        repo: String,
    },
    /// A git clone URL or scp-like spelling — recognised only so [`resolve`]
    /// can refuse it well, pointing at the release form instead of reporting
    /// a mystifying "no such directory".
    GitSpelling(String),
}

/// Which kind of source `spec` is, without touching the filesystem.
///
/// The grammar (`../plan/018-remote-sources-releases.md`'s Overview),
/// checked in this order because a later rule is a special case of an
/// earlier one otherwise: a spec ending in `.git` is always a git-clone
/// spelling first, even if it also happens to start with `github:` or point
/// at `github.com` — a `.git` suffix is never part of the canonical form, so
/// seeing one is always the user having pasted the wrong button.
///
/// - `git@…` or anything ending in `.git` → [`Origin::GitSpelling`].
/// - `github:owner/repo` → the canonical [`Origin::Release`].
/// - `https://github.com/owner/repo` → the same, normalised to it — what a
///   browser's address bar hands out for a public repository.
/// - anything ending in `.tar.gz` → [`Origin::LocalArchive`].
/// - anything else → [`Origin::Unrecognised`], so [`resolve`] can refuse it
///   well.
pub fn classify(spec: &str) -> Origin {
    if spec.starts_with("git@") || spec.ends_with(".git") {
        return Origin::GitSpelling(spec.to_string());
    }
    if let Some(repo) = spec.strip_prefix("github:") {
        return Origin::Release {
            index: Some("github".to_string()),
            repo: repo.to_string(),
        };
    }
    if let Some(repo) = github_https_repo(spec) {
        return Origin::Release {
            index: Some("github".to_string()),
            repo,
        };
    }
    if spec.ends_with(".tar.gz") {
        return Origin::LocalArchive(PathBuf::from(spec));
    }
    Origin::Unrecognised(PathBuf::from(spec))
}

/// `https://github.com/<owner>/<repo>` (an optional trailing slash and an
/// optional `.git` suffix both tolerated, nothing else after it) turned into
/// `<owner>/<repo>` — the same repo string `github:<owner>/<repo>` names.
/// `None` for anything shaped differently, including a `github.com` URL
/// carrying more path segments than that: this function only recognises the
/// one shape a browser's address bar produces for a repository's own page
/// (plus the `.git`-suffixed form `git remote get-url` can print for an
/// https remote), not every possible GitHub URL.
///
/// `pub(crate)` so `git.rs`'s upstream-remote gate
/// (`../plan/019-publish-an-app.md` step 5) parses `git remote get-url`'s
/// output with the exact same rule this module already applies to a
/// user-typed spec, rather than a second one that could drift from it.
pub(crate) fn github_https_repo(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix("https://github.com/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut segments = rest.split('/');
    let owner = segments.next().filter(|segment| !segment.is_empty())?;
    let repo = segments.next().filter(|segment| !segment.is_empty())?;
    match segments.next() {
        None => Some(format!("{owner}/{repo}")),
        Some(_) => None,
    }
}

/// `git@github.com:<owner>/<repo>.git` (the scp-like spelling `git remote
/// get-url` prints for an SSH remote — an optional `.git` suffix tolerated,
/// same as the https form) turned into `<owner>/<repo>` — `git.rs`'s other
/// half of the same reuse [`github_https_repo`] documents.
pub(crate) fn github_ssh_repo(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix("git@github.com:")?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut segments = rest.split('/');
    let owner = segments.next().filter(|segment| !segment.is_empty())?;
    let repo = segments.next().filter(|segment| !segment.is_empty())?;
    match segments.next() {
        None => Some(format!("{owner}/{repo}")),
        Some(_) => None,
    }
}

/// A source turned into a directory on this machine, plus what the registry
/// has to record about where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The extracted tree, ready to be copied into `apps/<id>` — a release
    /// downloaded into the caller's scratch, or a local archive extracted
    /// there.
    pub root: PathBuf,
    /// What `update` needs to resolve this same source again.
    pub source: Source,
    /// The tree as it stood at this moment — recorded as the entry's
    /// `source_revision`, which the rollback anchor and the update journal
    /// compare against the tree they are about to replace.
    pub revision: String,
}

/// Turn `origin` into a directory, or say why it cannot be one.
///
/// The local variant resolves to an **absolute** path, deliberately: the
/// recorded `location` is re-read much later, by a `list` or an `update` run
/// from some other working directory, and a relative path recorded from today's
/// cwd would silently point at nothing.
///
/// `scratch` is where a release is downloaded, verified and extracted before
/// this function returns — the caller's directory (`paths::Paths::scratch_dir`),
/// created here if a release needs it and left for the caller to remove once
/// the install or update this call is part of has finished, on success or on
/// failure (`../plan/018-remote-sources-releases.md`'s "Where the archive
/// lands before it is trusted"). The local variant never touches it.
///
/// `base_url` is the same seam `release.rs`'s own `_at` functions give
/// `release_tests.rs`, one layer up: production callers (`install`, `update`)
/// always pass [`release::GITHUB_API_BASE`]; their own tests point a whole
/// `install`/`update` run at a local stub through this instead.
pub fn resolve(
    origin: &Origin,
    reference: Option<&str>,
    scratch: &Path,
    base_url: &str,
) -> Result<Resolved, SourceError> {
    match origin {
        Origin::LocalArchive(path) => resolve_local_archive(path, reference, scratch),
        Origin::Unrecognised(path) => {
            if path.is_dir() {
                return Err(SourceError::DirectoryNotASource { path: path.clone() });
            }
            Err(SourceError::UnrecognisedSource {
                spec: path.display().to_string(),
            })
        }
        Origin::Release { index, repo } => {
            resolve_release(repo, index.as_deref(), reference, scratch, base_url)
        }
        Origin::GitSpelling(spec) => Err(SourceError::GitSpelling { spec: spec.clone() }),
    }
}

/// [`resolve`]'s `Origin::Release` arm: fetch the release (latest, or
/// `reference`'s tag exactly), resolve its two assets, download and verify
/// the archive against `SHA256SUMS.txt`, then extract it — in that order, so
/// nothing is extracted until the checksum matches and nothing is returned
/// until [`archive::extract`] has walked the whole tree.
fn resolve_release(
    repo: &str,
    index: Option<&str>,
    reference: Option<&str>,
    scratch: &Path,
    base_url: &str,
) -> Result<Resolved, SourceError> {
    let release = match reference {
        Some(tag) => release::fetch_release_by_tag_at(base_url, repo, tag)?,
        None => release::fetch_latest_release_at(base_url, repo)?,
    };
    let assets = release::resolve_assets(&release)?;

    fs::create_dir_all(scratch).map_err(|source| SourceError::Io {
        path: scratch.to_path_buf(),
        source,
    })?;
    let archive_path = scratch.join(assets.archive_name);
    release::download_to(assets.archive_url, &archive_path)?;

    let checksums = release::parse_sha256sums(&release::fetch_text(assets.checksums_url)?);
    let actual = release::sha256_file(&archive_path)?;
    check_checksum(&checksums, assets.archive_name, &actual)?;

    let root = archive::extract(&archive_path, &scratch.join("extracted"))?;
    let version = assets
        .version
        .as_ref()
        .expect("resolve_assets always returns the parsed app-release tag version");
    check_archive_manifest(
        &root,
        assets.archive_name,
        Some((&release.tag_name, version)),
    )?;
    let revision = tree_hash(&root).map_err(|source| SourceError::Unreadable {
        path: root.clone(),
        source,
    })?;

    Ok(Resolved {
        source: Source {
            kind: SourceKind::Release,
            location: repo.to_string(),
            reference: Some(release.tag_name),
            reference_kind: Some(ReferenceKind::Tag),
            index: Some(index.unwrap_or("github").to_string()),
        },
        root,
        revision,
    })
}

fn resolve_local_archive(
    path: &Path,
    reference: Option<&str>,
    scratch: &Path,
) -> Result<Resolved, SourceError> {
    if let Some(reference) = reference {
        return Err(SourceError::ReferenceOnLocalArchive {
            reference: reference.to_string(),
        });
    }
    let archive_path = fs::canonicalize(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => SourceError::Missing {
            path: path.to_path_buf(),
        },
        _ => SourceError::Unreadable {
            path: path.to_path_buf(),
            source,
        },
    })?;
    // Reached by `update <id> <dir>` — which always builds a
    // `LocalArchive` from its argument — and by a directory whose name ends
    // in `.tar.gz`; both are owed the `publish --local` route rather than a
    // mystifying "not a regular archive file".
    if archive_path.is_dir() {
        return Err(SourceError::DirectoryNotASource { path: archive_path });
    }
    if !archive_path.is_file() {
        return Err(SourceError::NotAFile { path: archive_path });
    }
    if crate::portability::is_backup(&archive_path) {
        return Err(SourceError::IsABackup { path: archive_path });
    }
    let directory = archive_path.parent().expect("canonical file has a parent");
    let sums_path = directory.join(release::SHA256SUMS_ASSET_NAME);
    let sums_text = fs::read_to_string(&sums_path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => SourceError::ChecksumFileMissing {
            directory: directory.to_path_buf(),
        },
        _ => SourceError::Unreadable {
            path: sums_path.clone(),
            source,
        },
    })?;
    // Canonicalising resolved any symlink, so the name checked against
    // `SHA256SUMS.txt` is the real file's — which the classified spelling
    // no longer guarantees to be UTF-8, nor even to end in `.tar.gz`.
    let archive_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SourceError::NotAFile {
            path: archive_path.clone(),
        })?;
    let checksums = release::parse_sha256sums(&sums_text);
    let actual = release::sha256_file(&archive_path)?;
    check_checksum(&checksums, archive_name, &actual)?;
    let root = archive::extract(&archive_path, &scratch.join("extracted"))?;
    check_archive_manifest(&root, archive_name, None)?;
    let revision = tree_hash(&root).map_err(|source| SourceError::Unreadable {
        path: root.clone(),
        source,
    })?;
    Ok(Resolved {
        source: Source {
            kind: SourceKind::LocalArchive,
            location: archive_path.display().to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        root,
        revision,
    })
}

fn check_checksum(
    checksums: &std::collections::HashMap<String, String>,
    archive_name: &str,
    actual: &str,
) -> Result<(), SourceError> {
    match release::verify(checksums, archive_name, actual) {
        release::VerifyOutcome::Match => Ok(()),
        release::VerifyOutcome::Mismatch { expected, actual } => {
            Err(SourceError::ChecksumMismatch {
                archive_name: archive_name.to_string(),
                expected,
                actual,
            })
        }
        release::VerifyOutcome::MissingEntry => Err(SourceError::ChecksumMissing {
            archive_name: archive_name.to_string(),
        }),
    }
}

/// Check the extracted app identity against the release's name and, for a
/// forge release, its tag. The local name is derived from the manifest itself.
fn check_archive_manifest(
    root: &Path,
    archive_name: &str,
    tagged: Option<(&str, &semver::Version)>,
) -> Result<(), SourceError> {
    let tag = tagged.map_or("local archive", |(tag, _)| tag);
    let manifest = manifest::load(root).map_err(|source| SourceError::ReleaseManifest {
        tag: tag.to_string(),
        archive_name: archive_name.to_string(),
        source,
    })?;
    let manifest_version =
        version::parse_app_version(&manifest.manifest.app_version).map_err(|_| {
            SourceError::ManifestVersionNotCanonical {
                tag: tag.to_string(),
                archive_name: archive_name.to_string(),
                manifest_version: manifest.manifest.app_version.clone(),
            }
        })?;
    if let Some((_, version)) = tagged {
        if manifest_version != *version {
            return Err(SourceError::ManifestVersionMismatch {
                tag: tag.to_string(),
                archive_name: archive_name.to_string(),
                manifest_version: manifest.manifest.app_version,
            });
        }
    }
    let expected_name = format!(
        "{}-{}.tar.gz",
        manifest.manifest.project_name, manifest_version
    );
    if archive_name != expected_name && tagged.is_none() {
        return Err(SourceError::ArchiveNameMismatch {
            archive_name: archive_name.to_string(),
            expected: expected_name,
        });
    }
    let archive_project_name = archive_name
        .strip_suffix(&format!("-{manifest_version}.tar.gz"))
        .unwrap_or("");
    if manifest.manifest.project_name != archive_project_name {
        return Err(SourceError::ManifestProjectNameMismatch {
            tag: tag.to_string(),
            archive_name: archive_name.to_string(),
            manifest_project_name: manifest.manifest.project_name,
        });
    }
    Ok(())
}

/// Why a source could not be turned into a directory.
#[derive(Debug)]
pub enum SourceError {
    Missing {
        path: PathBuf,
    },
    /// A working directory where an installable source was asked for — the
    /// refusal names the `publish --local` route that turns a tree into an
    /// archive worth installing.
    DirectoryNotASource {
        path: PathBuf,
    },
    /// Anything `classify` could not name: not a git spelling, not a forge
    /// spec, not an archive. The refusal states the two kinds the hub does
    /// install.
    UnrecognisedSource {
        spec: String,
    },
    NotAFile {
        path: PathBuf,
    },
    IsABackup {
        path: PathBuf,
    },
    ChecksumFileMissing {
        directory: PathBuf,
    },
    ReferenceOnLocalArchive {
        reference: String,
    },
    ArchiveNameMismatch {
        archive_name: String,
        expected: String,
    },
    Unreadable {
        path: PathBuf,
        source: io::Error,
    },
    /// A git clone URL or scp-like spelling — recognised so the refusal can
    /// point at the release form instead of reporting a mystifying "no such
    /// directory".
    GitSpelling {
        spec: String,
    },
    /// Fetching the release's metadata, downloading an asset, or reading its
    /// checksums text failed — the release client's own taxonomy
    /// (`release::ReleaseError`), reported through this module because a bad
    /// release is exactly as much a reason `resolve` could not produce a
    /// directory as a missing local path is.
    Release(ReleaseError),
    /// The downloaded archive's SHA-256 does not match the line
    /// `SHA256SUMS.txt` carries for it — a tampered or corrupted download.
    /// Nothing under `apps/` is touched: this refusal fires before
    /// [`archive::extract`] ever runs.
    ChecksumMismatch {
        archive_name: String,
        expected: String,
        actual: String,
    },
    /// `SHA256SUMS.txt` has no line naming the archive at all — a missing
    /// line is a failure, never a pass by absence.
    ChecksumMissing {
        archive_name: String,
    },
    /// The archive passed its checksum but its manifest is not an app
    /// manifest. The release cannot be installed until its author corrects it.
    ReleaseManifest {
        tag: String,
        archive_name: String,
        source: ManifestError,
    },
    /// The extracted manifest's version is not the contract's exact
    /// `MAJOR.MINOR.PATCH` spelling, even before it can be compared to its tag.
    ManifestVersionNotCanonical {
        tag: String,
        archive_name: String,
        manifest_version: String,
    },
    /// The extracted manifest names a version different from its release tag.
    ManifestVersionMismatch {
        tag: String,
        archive_name: String,
        manifest_version: String,
    },
    /// The extracted manifest names a project different from its archive.
    ManifestProjectNameMismatch {
        tag: String,
        archive_name: String,
        manifest_project_name: String,
    },
    /// The archive passed its checksum but could not be safely extracted —
    /// `archive::ArchiveError`'s own taxonomy (a path escaping the tree, a
    /// malformed top level).
    Archive(ArchiveError),
    /// Creating the scratch directory a release downloads into failed.
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => write!(formatter, "no such source: {}", path.display()),
            Self::DirectoryNotASource { path } => write!(
                formatter,
                "{} is a directory. The hub installs release archives, not working \
                 trees: commit the project, run `tfsapp-hub publish {} --local <out-dir>`, \
                 then install the `.tar.gz` it writes. To run a project in place while \
                 developing, use `tfsapp-hub dev {}`.",
                path.display(),
                path.display(),
                path.display()
            ),
            Self::UnrecognisedSource { spec } => write!(
                formatter,
                "{spec} is not a source the hub installs. Give `github:owner/repo`, \
                 or the path to a `<name>-<version>.tar.gz` release archive with its \
                 `SHA256SUMS.txt` beside it."
            ),
            Self::NotAFile { path } => write!(
                formatter,
                "{} is not a regular archive file with a UTF-8 name",
                path.display()
            ),
            Self::IsABackup { path } => write!(
                formatter,
                "{} is a backup written by export — use `tfsapp-hub import <id> {}`",
                path.display(),
                path.display()
            ),
            Self::ChecksumFileMissing { directory } => write!(
                formatter,
                "no SHA256SUMS.txt beside the archive in {} — an unverified archive \
                 cannot be installed",
                directory.display()
            ),
            Self::ReferenceOnLocalArchive { reference } => write!(
                formatter,
                "--ref {reference} cannot select a revision of a local archive — pass the \
                 desired archive itself"
            ),
            Self::ArchiveNameMismatch {
                archive_name,
                expected,
            } => write!(
                formatter,
                "archive {archive_name} does not match its manifest — expected {expected}"
            ),
            Self::Unreadable { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::GitSpelling { spec } => write!(
                formatter,
                "{spec} looks like a git clone URL — the hub installs releases, not \
                 repositories. Use github:owner/repo, or publish a local release \
                 archive from a clone with tfsapp-hub publish."
            ),
            Self::Release(error) => write!(formatter, "{error}"),
            Self::ChecksumMismatch {
                archive_name,
                expected,
                actual,
            } => write!(
                formatter,
                "{archive_name} does not match the checksum SHA256SUMS.txt carries for it \
                 (expected {expected}, got {actual}) — the archive may be corrupted or \
                 tampered with. Nothing was installed."
            ),
            Self::ChecksumMissing { archive_name } => write!(
                formatter,
                "SHA256SUMS.txt has no line for {archive_name} — a release missing an \
                 entry for its own archive cannot be verified, so it is refused rather \
                 than installed unverified."
            ),
            Self::ReleaseManifest {
                tag,
                archive_name,
                source,
            } => write!(
                formatter,
                "release {tag}'s archive {archive_name} has an invalid manifest ({source}) — \
                 ask the app's author to fix and republish the release."
            ),
            Self::ManifestVersionMismatch {
                tag,
                archive_name,
                manifest_version,
            } => write!(
                formatter,
                "release {tag}'s archive {archive_name} declares app_version {manifest_version:?} \
                 in its manifest, not the tag's version — ask the app's author to fix and \
                 republish the release."
            ),
            Self::ManifestVersionNotCanonical {
                tag,
                archive_name,
                manifest_version,
            } => write!(
                formatter,
                "release {tag}'s archive {archive_name} declares app_version {manifest_version:?} \
                 in its manifest, which must be canonical MAJOR.MINOR.PATCH — ask the app's \
                 author to fix and republish the release."
            ),
            Self::ManifestProjectNameMismatch {
                tag,
                archive_name,
                manifest_project_name,
            } => write!(
                formatter,
                "release {tag}'s archive {archive_name} declares project_name \
                 {manifest_project_name:?} in its manifest, not the archive's project name — \
                 ask the app's author to fix and republish the release."
            ),
            Self::Archive(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::Release(error) => Some(error),
            Self::Archive(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<ReleaseError> for SourceError {
    fn from(error: ReleaseError) -> Self {
        Self::Release(error)
    }
}

impl From<ArchiveError> for SourceError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(error)
    }
}

/// Hash a directory tree into a stable, comparable string.
///
/// The definition, since it is the value's whole meaning: every file under
/// `root` outside [`EXCLUDED_FROM_HASH`], in sorted path order, contributing
/// its relative path, whether it is executable, and its bytes. Sorted rather
/// than in directory order because readdir order is a filesystem detail, and
/// the same tree copied elsewhere must hash the same.
///
/// A symlink contributes its target, not the target's content: what changed
/// when a link is repointed is the source tree, and following it would let a
/// link out of the tree pull unrelated bytes into the hash.
pub fn tree_hash(root: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    hash_directory(root, Path::new(""), 0, &mut hasher)?;
    Ok(format!(
        "sha256:{}",
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn hash_directory(
    directory: &Path,
    relative: &Path,
    depth: usize,
    hasher: &mut Sha256,
) -> io::Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();

    for entry in entries {
        let Some(name) = entry.file_name() else {
            continue;
        };
        if depth == 0 && EXCLUDED_FROM_HASH.iter().any(|excluded| name == *excluded) {
            continue;
        }

        let relative = relative.join(name);
        let metadata = fs::symlink_metadata(&entry)?;

        // The path goes in before anything else, so that moving a file's
        // content to another name changes the hash even when the bytes are
        // identical.
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");

        if metadata.is_symlink() {
            hasher.update(b"symlink\0");
            hasher.update(fs::read_link(&entry)?.as_os_str().as_encoded_bytes());
        } else if metadata.is_dir() {
            hasher.update(b"dir\0");
            hash_directory(&entry, &relative, depth + 1, hasher)?;
        } else {
            use std::os::unix::fs::PermissionsExt;

            // The executable bit is part of the source: a hook or a binary in
            // `bin/` that lost it is a changed tree, and the bytes alone would
            // not say so.
            let executable = metadata.permissions().mode() & 0o111 != 0;
            hasher.update(match executable {
                true => b"file+x\0".as_slice(),
                false => b"file\0".as_slice(),
            });
            // Length-prefixed, because file content is the one thing here that
            // can contain the NUL the other fields are separated by.
            let content = fs::read(&entry)?;
            hasher.update((content.len() as u64).to_le_bytes());
            hasher.update(content);
        }
        hasher.update(b"\0");
    }

    Ok(())
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
