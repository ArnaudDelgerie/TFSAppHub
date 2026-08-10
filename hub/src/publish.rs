//! `tfsapp-hub publish <local-path>` — the app author's side of the release
//! contract (CONTRACT.md's "Publishing a release",
//! `../decision/003-the-hub-publishes-apps.md`).
//!
//! Written in the gate order `../plan/019-publish-an-app.md`'s Overview table
//! lists them: gates 1–5 are local and pure over a project directory
//! ([`run_local_gates`]); gates 6–9 are `gh.rs`'s `Gh`; then the archive and
//! its sums ([`build_archive`]), the announcement, the confirmation, and
//! `gh release create`. [`run`] is the command `main.rs` reaches.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    archive,
    cli::{EXIT_FAILED, EXIT_OK},
    gh::{Gh, GhError},
    manifest::{self, Loaded, Manifest, ManifestError, MANIFEST_FILE},
    paths::Paths,
    prompt,
    release::{self, ReleaseError},
    source,
};

/// The changelog's filename at the project root (CONTRACT.md §1/§7).
pub const CHANGELOG_FILE: &str = "CHANGELOG.md";

/// What gates 1–5 produce for the steps after them: the loaded manifest, the
/// resolved `owner/repo`, and the release notes (the changelog section,
/// verbatim).
#[derive(Debug)]
pub struct LocalGates {
    pub loaded: Loaded,
    pub repo: String,
    /// `"--repo"` or `"releases_repo"` — which of the two named [`Self::repo`],
    /// echoed in the announcement so a manifest's stale value is never used
    /// silently (the plan's "Which repository, and what happens to
    /// `releases_repo`").
    pub repo_source: &'static str,
    pub notes: String,
}

/// Run every local gate, in the Overview's order: load the manifest (1),
/// check `app_version` is canonical semver (2), resolve the target
/// repository (3), extract the changelog section for this version (4), and —
/// only when `actions.secrets.ipc` is on — block on a confirmation with no
/// `--yes` escape (5).
///
/// Nothing here touches the network: gates 6–9 (the `gh` seam) are a later
/// step's function, run only once every gate here has passed.
pub fn run_local_gates(
    project_path: &Path,
    repo: Option<&str>,
) -> Result<LocalGates, PublishError> {
    let loaded = manifest::load(project_path)?;
    // Before any further gate runs, so a typo in a key is read next to the
    // project it came from rather than after the changelog and repository
    // have both been resolved — the same ordering `install`'s own pipeline
    // uses.
    loaded.report_warnings();
    let manifest_path = project_path.join(MANIFEST_FILE);

    validate_version(&loaded.manifest, &manifest_path)?;
    let (repo, repo_source) = resolve_repo(repo, &loaded.manifest, &manifest_path)?;
    let notes = changelog_gate(project_path, &loaded.manifest.app_version)?;
    confirm_ipc_secrets(&loaded.manifest)?;

    Ok(LocalGates {
        loaded,
        repo,
        repo_source,
        notes,
    })
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

/// Build `<project_name>-<app_version>.tar.gz` and its `SHA256SUMS.txt` into
/// `destination`, from `project_path`'s tree as it stands right now —
/// `CONTRACT.md` §1's artefact.
///
/// The walk shares [`source::EXCLUDED_FROM_HASH`] with `source::tree_hash`
/// rather than a second list, which is what buys the property this step
/// exists for: `tree_hash` of the archive, once extracted, equals
/// `tree_hash` of the tree it was built from. A symlink whose target would
/// resolve outside the extracted tree is refused before a byte of the
/// archive is written, with the exact lexical rule `archive::extract` applies
/// at the other end (`archive::link_target_escapes`).
pub fn build_archive(
    project_path: &Path,
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
    builder.follow_symlinks(false);

    let archive_root = PathBuf::from(&prefix);
    builder
        .append_dir(&archive_root, project_path)
        .map_err(|source| PublishError::Io {
            path: project_path.to_path_buf(),
            source,
        })?;
    append_tree(&mut builder, project_path, &archive_root, 0)?;

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

/// Append `directory`'s entries under `archive_dir`, sorted, recursing into
/// subdirectories — the same walk `source::hash_directory` performs, over the
/// same [`source::EXCLUDED_FROM_HASH`] predicate at `depth == 0`.
fn append_tree<W: io::Write>(
    builder: &mut tar::Builder<W>,
    directory: &Path,
    archive_dir: &Path,
    depth: usize,
) -> Result<(), PublishError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)
        .map_err(|source| PublishError::Io {
            path: directory.to_path_buf(),
            source,
        })?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(|source| PublishError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    entries.sort();

    for entry in entries {
        let Some(name) = entry.file_name() else {
            continue;
        };
        if depth == 0
            && source::EXCLUDED_FROM_HASH
                .iter()
                .any(|excluded| name == *excluded)
        {
            continue;
        }

        let archive_path = archive_dir.join(name);
        let metadata = fs::symlink_metadata(&entry).map_err(|source| PublishError::Io {
            path: entry.clone(),
            source,
        })?;

        if metadata.is_symlink() {
            let target = fs::read_link(&entry).map_err(|source| PublishError::Io {
                path: entry.clone(),
                source,
            })?;
            if archive::link_target_escapes(&archive_path, &target) {
                return Err(PublishError::EscapingSymlink { path: entry });
            }
            builder
                .append_path_with_name(&entry, &archive_path)
                .map_err(|source| PublishError::Io {
                    path: entry.clone(),
                    source,
                })?;
        } else if metadata.is_dir() {
            builder
                .append_dir(&archive_path, &entry)
                .map_err(|source| PublishError::Io {
                    path: entry.clone(),
                    source,
                })?;
            append_tree(builder, &entry, &archive_path, depth + 1)?;
        } else {
            builder
                .append_path_with_name(&entry, &archive_path)
                .map_err(|source| PublishError::Io {
                    path: entry.clone(),
                    source,
                })?;
        }
    }

    Ok(())
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

/// The pipeline, in the Overview table's order: gates 1–5
/// ([`run_local_gates`]), gates 6–9 (`gh`, below), the archive and its sums
/// built into scratch, the announcement, the confirmation, then
/// `gh release create`. `false` means the user declined — everything up to
/// that point already ran, but nothing was uploaded.
///
/// Takes `Paths` and a [`Gh`] rather than resolving/constructing them,
/// matching `update::update` — what lets the whole pipeline run in a test
/// against a throwaway scratch directory and a fake `gh`.
pub(crate) fn publish(
    paths: &Paths,
    project_path: &Path,
    repo: Option<&str>,
    assume_yes: bool,
    gh: &Gh,
) -> Result<bool, PublishError> {
    // Same reasoning as `install::install`'s own wrapper: the archive and its
    // sums are built into it, and it is removed on the way out regardless of
    // how this call ends — the hub writes nothing into the project itself
    // (the plan's "Where it is built, and what is left behind").
    let scratch = paths.scratch_dir();
    let result = publish_into(&scratch, project_path, repo, assume_yes, gh);
    let _ = fs::remove_dir_all(&scratch);
    result
}

fn publish_into(
    scratch: &Path,
    project_path: &Path,
    repo: Option<&str>,
    assume_yes: bool,
    gh: &Gh,
) -> Result<bool, PublishError> {
    let gates = run_local_gates(project_path, repo)?;
    let manifest = &gates.loaded.manifest;
    let tag = format!("v{}", manifest.app_version);

    gh.ensure_installed()?;
    gh.ensure_authenticated()?;
    gh.ensure_tag_pushed(&gates.repo, &tag)?;
    gh.ensure_no_existing_release(&gates.repo, &tag)?;

    let assets = build_archive(
        project_path,
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
        &gates.repo,
        &tag,
        &notes_path,
        &assets.archive_name,
        &assets.archive_path,
        &assets.sums_path,
    )?;

    println!("Published {tag} on {}", gates.repo);
    println!("  {url}");
    println!();
    println!(
        "Users install it with: tfsapp-hub install github:{}",
        gates.repo
    );

    Ok(true)
}

/// Say what is about to be published, in the terms the user will have to
/// reason about afterwards — `update::announce`'s counterpart for `publish`.
/// Names where `gates.repo` came from (never uses it silently — the plan's
/// "Which repository, and what happens to `releases_repo`"), and closes on
/// the one sentence that is this command's residual risk: the archive is the
/// working tree in front of the hub right now, not the tag.
fn announce(gates: &LocalGates, tag: &str, assets: &Assets) {
    println!("Repository  {} (from {})", gates.repo, gates.repo_source);
    println!("Tag         {tag}");
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
    println!();
    println!(
        "This archives the working tree in front of the hub right now — not the tag {tag}. \
         Publish from a clean checkout of the tag you just pushed."
    );
}

/// Gate 2: `app_version` must parse as canonical semver — the value a
/// published tag and archive name are both built from, and CONTRACT.md §2's
/// own requirement.
fn validate_version(manifest: &Manifest, manifest_path: &Path) -> Result<(), PublishError> {
    semver::Version::parse(&manifest.app_version).map_err(|error| {
        PublishError::UnusableVersion {
            path: manifest_path.to_path_buf(),
            version: manifest.app_version.clone(),
            detail: error.to_string(),
        }
    })?;
    Ok(())
}

/// Gate 3: `--repo` wins; otherwise the manifest's `releases_repo`; otherwise
/// a refusal naming both ways to supply one. Whichever wins, its shape is
/// checked before it is trusted any further.
fn resolve_repo(
    explicit: Option<&str>,
    manifest: &Manifest,
    manifest_path: &Path,
) -> Result<(String, &'static str), PublishError> {
    let (repo, source) = match explicit {
        Some(repo) => (repo.to_string(), "--repo"),
        None => match manifest
            .releases_repo
            .as_deref()
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
        {
            Some(repo) => (repo.to_string(), "releases_repo"),
            None => {
                return Err(PublishError::NoRepository {
                    path: manifest_path.to_path_buf(),
                })
            }
        },
    };

    if !is_owner_repo_shape(&repo) {
        return Err(PublishError::InvalidRepoShape {
            repo,
            source,
            path: manifest_path.to_path_buf(),
        });
    }

    Ok((repo, source))
}

/// Whether `repo` is exactly one non-empty `owner`, a `/`, and one non-empty
/// `repo` — no leading, trailing or doubled slash.
fn is_owner_repo_shape(repo: &str) -> bool {
    let mut segments = repo.split('/');
    let owner = segments.next().filter(|segment| !segment.is_empty());
    let name = segments.next().filter(|segment| !segment.is_empty());
    owner.is_some() && name.is_some() && segments.next().is_none()
}

/// Gate 4: `CHANGELOG.md` must exist at the project root and carry a heading
/// for `version`. The matched section becomes the release notes, verbatim.
fn changelog_gate(project_path: &Path, version: &str) -> Result<String, PublishError> {
    let path = project_path.join(CHANGELOG_FILE);
    let contents = fs::read_to_string(&path)
        .map_err(|_| PublishError::MissingChangelog { path: path.clone() })?;
    changelog_section(&contents, version).ok_or(PublishError::MissingChangelogEntry {
        path,
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

/// Gate 5: `actions.secrets.ipc` off is silent. On, it blocks on a
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

/// Everything gates 1–5 can refuse over.
#[derive(Debug)]
pub enum PublishError {
    Manifest(ManifestError),
    /// `app_version` does not parse as canonical semver.
    UnusableVersion {
        path: PathBuf,
        version: String,
        detail: String,
    },
    /// Neither `--repo` nor the manifest's `releases_repo` named a target.
    NoRepository {
        path: PathBuf,
    },
    /// Whichever of `--repo`/`releases_repo` won is not `owner/repo`.
    InvalidRepoShape {
        repo: String,
        source: &'static str,
        path: PathBuf,
    },
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
    /// Gates 6–9, or the final `gh release create`, refused — `gh.rs`'s own
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
            Self::NoRepository { path } => write!(
                formatter,
                "no target repository — pass --repo owner/repo, or add \"releases_repo\": \
                 \"owner/repo\" to {} (CONTRACT.md §2).",
                path.display()
            ),
            Self::InvalidRepoShape { repo, source, path } => {
                let origin = match *source {
                    "releases_repo" => format!("\"releases_repo\" in {}", path.display()),
                    flag => flag.to_string(),
                };
                write!(formatter, "{origin} is {repo:?}, which is not owner/repo.")
            }
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
