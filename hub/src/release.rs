//! The forge client for a remote source (`../decision/002-remote-sources-are-
//! releases.md`): resolve a repository's latest release, or the one `--ref`
//! names, into the two assets `source::resolve` needs — a source archive and
//! its `SHA256SUMS.txt`.
//!
//! Ported from the station's `desktop/src-tauri/src/update.rs` (its release
//! half), which is ~750 lines of exactly this already proven in production:
//! `ureq` with its `json` feature, a dedicated User-Agent, explicit
//! connect/read timeouts, and transport errors mapped to categories a user
//! can act on rather than an HTTP status code they cannot.
//!
//! **No caching.** The station cached because a *running app* polled on a
//! timer; `install` and `update` are one-shot commands a human just typed, and
//! a cache would only hide a release published thirty seconds ago.
//!
//! **[`resolve_assets`] cannot yet ask for `<project_name>-<app_version>.tar.gz`
//! by its exact name.** That name needs `project_name`, which lives in a
//! manifest inside the very archive this function is looking for — so the
//! rule it applies here is what it *can* know before the archive is opened:
//! exactly one asset ending in `.tar.gz`, and one named `SHA256SUMS.txt`. The
//! manifest read after extraction (`../plan/018-remote-sources-releases.md`'s
//! step 4) is what confirms the archive's name was honest; a mismatch there
//! is a refusal, not a warning.
//!
//! Downloading the assets this module locates, verifying them against
//! `SHA256SUMS.txt` and extracting them into scratch space is step 3's own
//! module, not this one — this one only ever reaches GitHub's JSON API.

// `source::resolve`'s `Origin::Release` arm is this module's caller
// (this plan's step 4); until then nothing in the binary calls it.
#![allow(dead_code)]

use std::{fmt, time::Duration};

use serde::Deserialize;

/// GitHub rejects API requests with no `User-Agent`.
const GITHUB_USER_AGENT: &str = "TFSAppHub-release-resolver";

const GITHUB_API_BASE: &str = "https://api.github.com";

/// Timeouts for a `releases/latest` or `releases/tags/<tag>` call — small
/// JSON, so a hung network must fail fast rather than wedge an `install`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The name `build/scripts/publish-app.sh` always writes the checksums asset
/// under (this plan's step 5).
pub const SHA256SUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

/// The subset of GitHub's release response this resolver reads.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

/// `GET /repos/<repo>/releases/latest` against the real GitHub API.
pub fn fetch_latest_release(repo: &str) -> Result<GitHubRelease, ReleaseError> {
    fetch_latest_release_at(GITHUB_API_BASE, repo)
}

/// [`fetch_latest_release`], against `base_url` instead of GitHub's — the seam
/// tests use to point this at a local stub.
pub fn fetch_latest_release_at(base_url: &str, repo: &str) -> Result<GitHubRelease, ReleaseError> {
    fetch_release(&format!("{base_url}/repos/{repo}/releases/latest"))
}

/// `GET /repos/<repo>/releases/tags/<tag>` — `--ref <tag>`'s own lookup, and
/// `rollback <id>`'s way back to a previously installed release.
pub fn fetch_release_by_tag(repo: &str, tag: &str) -> Result<GitHubRelease, ReleaseError> {
    fetch_release_by_tag_at(GITHUB_API_BASE, repo, tag)
}

pub fn fetch_release_by_tag_at(
    base_url: &str,
    repo: &str,
    tag: &str,
) -> Result<GitHubRelease, ReleaseError> {
    fetch_release(&format!("{base_url}/repos/{repo}/releases/tags/{tag}"))
}

fn metadata_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .build()
}

fn fetch_release(url: &str) -> Result<GitHubRelease, ReleaseError> {
    match metadata_agent()
        .get(url)
        .set("User-Agent", GITHUB_USER_AGENT)
        .call()
    {
        Ok(response) => response
            .into_json::<GitHubRelease>()
            .map_err(|error| ReleaseError::InvalidResponse(error.to_string())),
        Err(ureq::Error::Status(404, _)) => Err(ReleaseError::NotFound),
        Err(ureq::Error::Status(403, _)) | Err(ureq::Error::Status(429, _)) => {
            Err(ReleaseError::RateLimited)
        }
        Err(ureq::Error::Status(status, _)) => {
            Err(ReleaseError::InvalidResponse(format!("HTTP {status}")))
        }
        Err(ureq::Error::Transport(error)) => Err(ReleaseError::Offline(error.to_string())),
    }
}

/// The two assets a release must carry, located without knowing the app's
/// `project_name` yet (see the module header).
#[derive(Debug)]
pub struct ResolvedAssets<'a> {
    pub archive_name: &'a str,
    pub archive_url: &'a str,
    pub checksums_url: &'a str,
}

/// Find `release`'s source archive and its `SHA256SUMS.txt`, or say which one
/// is missing (or ambiguous).
pub fn resolve_assets(release: &GitHubRelease) -> Result<ResolvedAssets<'_>, ReleaseError> {
    let archives: Vec<&GitHubAsset> = release
        .assets
        .iter()
        .filter(|asset| asset.name.ends_with(".tar.gz"))
        .collect();
    let archive = match archives.as_slice() {
        [one] => *one,
        [] => {
            return Err(ReleaseError::MissingAsset {
                tag: release.tag_name.clone(),
                missing: "a <project_name>-<app_version>.tar.gz source archive",
            })
        }
        _ => {
            return Err(ReleaseError::MissingAsset {
                tag: release.tag_name.clone(),
                missing: "exactly one .tar.gz asset — this release carries more than one",
            })
        }
    };
    let checksums = release
        .assets
        .iter()
        .find(|asset| asset.name == SHA256SUMS_ASSET_NAME)
        .ok_or_else(|| ReleaseError::MissingAsset {
            tag: release.tag_name.clone(),
            missing: SHA256SUMS_ASSET_NAME,
        })?;

    Ok(ResolvedAssets {
        archive_name: &archive.name,
        archive_url: &archive.browser_download_url,
        checksums_url: &checksums.browser_download_url,
    })
}

/// Why fetching or resolving a release failed — every branch a message a user
/// can act on, never a bare HTTP status.
#[derive(Debug)]
pub enum ReleaseError {
    /// A transport-level failure: DNS, TLS, connection refused, timed out.
    Offline(String),
    /// GitHub's unauthenticated rate limit (60/hour/IP) was hit — a 403 or a
    /// 429, indistinguishable in the way that matters: wait, or use a clone.
    RateLimited,
    /// No such repository, or no such release/tag.
    NotFound,
    /// The response was not the JSON this resolver expected.
    InvalidResponse(String),
    /// A release exists but is missing one of the two assets it must carry.
    MissingAsset { tag: String, missing: &'static str },
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline(detail) => write!(
                formatter,
                "cannot reach GitHub ({detail}) — check your network, or install from a clone."
            ),
            Self::RateLimited => write!(
                formatter,
                "GitHub's API rate limit was hit — wait a while, or install from a clone."
            ),
            Self::NotFound => write!(
                formatter,
                "no such repository or release on GitHub — check the spelling, or that the \
                 release exists."
            ),
            Self::InvalidResponse(detail) => write!(
                formatter,
                "GitHub answered with something this hub could not read ({detail})."
            ),
            Self::MissingAsset { tag, missing } => write!(
                formatter,
                "release {tag} has no {missing} — ask the app's author to publish one with \
                 build/scripts/publish-app.sh."
            ),
        }
    }
}

impl std::error::Error for ReleaseError {}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
