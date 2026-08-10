//! `tfsapp-hub publish <local-path>` — the app author's side of the release
//! contract (CONTRACT.md's "Publishing a release",
//! `../decision/003-the-hub-publishes-apps.md`).
//!
//! This module is written in the gate order `../plan/019-publish-an-app.md`'s
//! Overview table lists them. What is here today is gates 1–5, all of it
//! local and pure over a project directory — no network, no `gh`. The source
//! archive (step 2) and the `gh` seam (step 3) land in this same module as
//! the plan proceeds; the command itself is wired in step 4.

// Not yet reached from `main.rs` — `publish` is declared `NotYet` in
// `cli.rs`'s `SURFACE` until step 4 wires it. This module's own tests are
// its only caller until then. Remove the allow when that step lands, rather
// than letting it linger.
#![allow(dead_code)]

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use crate::{
    manifest::{self, Loaded, Manifest, ManifestError, MANIFEST_FILE},
    prompt,
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
    let repo = resolve_repo(repo, &loaded.manifest, &manifest_path)?;
    let notes = changelog_gate(project_path, &loaded.manifest.app_version)?;
    confirm_ipc_secrets(&loaded.manifest)?;

    Ok(LocalGates {
        loaded,
        repo,
        notes,
    })
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
) -> Result<String, PublishError> {
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

    Ok(repo)
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
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ManifestError> for PublishError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
