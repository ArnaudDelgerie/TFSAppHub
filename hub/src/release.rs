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
//! **[`resolve_assets`] cannot ask for the full
//! `<project_name>-<app_version>.tar.gz` name.** `project_name` lives in the
//! manifest inside the archive, but the tag already supplies the other half:
//! before extraction this module refuses anything but `v<canonical-semver>`
//! and an archive ending in `-<version>.tar.gz`. [`source::resolve_release`]
//! confirms the remaining project-name half against the extracted manifest.
//! Together those checks implement plan 018's release identity rule: a
//! mismatch is refused, never merely warned about.
//!
//! [`download_to`], [`sha256_file`] and [`verify`] are this plan's step 3:
//! downloading the assets this module locates and checking them against
//! `SHA256SUMS.txt`, still with no filesystem trust implied — nothing here
//! extracts an archive. That hardened walk is `archive.rs`, kept separate
//! because it is generic tar handling with nothing GitHub-specific left in
//! it once the bytes are on disk.

use std::{collections::HashMap, fmt, fs::File, io, path::Path, time::Duration};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::version;

/// GitHub rejects API requests with no `User-Agent`.
const GITHUB_USER_AGENT: &str = "TFSAppHub-release-resolver";

/// `pub(crate)` so `install` and `update` can pass it to `source::resolve`
/// for real, and their own tests can pass a local stub's URL instead — the
/// seam already proven at this module's own layer by `_at` and
/// `release_tests.rs`, extended one layer up.
pub(crate) const GITHUB_API_BASE: &str = "https://api.github.com";

/// The hub's own releases repo — the value `build/releases-repo` publishes,
/// baked in by `hub/build.rs` as `TFSAPP_RELEASES_REPO` so `release.sh`
/// (publishing) and this constant (the hub's own `--update`, `../plan/020-
/// hub-self-update-and-revalidation.md`) read one shared source of truth
/// rather than two copies of the same string.
pub const RELEASES_REPO: &str = env!("TFSAPP_RELEASES_REPO");

/// Timeouts for a `releases/latest` or `releases/tags/<tag>` call — small
/// JSON, so a hung network must fail fast rather than wedge an `install`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeouts for downloading the archive or `SHA256SUMS.txt` itself — tens of
/// megabytes rather than a small JSON body, so more patience than the
/// metadata calls above get.
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The name a published release's checksums asset is always found under —
/// `CONTRACT.md`'s publishing clause, whether the archive was produced by
/// hand or by `tfsapp-hub publish` (`../plan/019-publish-an-app.md`).
pub const SHA256SUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

/// The subset of GitHub's release response this resolver reads.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
    /// The release's own notes — GitHub sends `null` for a release with none,
    /// and omits the key from nothing this resolver has seen, but `default`
    /// covers that too. `update_check.rs`'s `notes` is this, or empty.
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

/// `GET /repos/<repo>/releases/latest`. `install` and `update` always pass
/// [`GITHUB_API_BASE`] to `source::resolve`, which lands here; their own
/// tests pass a local stub's URL instead, the same way `release_tests.rs`
/// uses it directly at this layer.
pub fn fetch_latest_release_at(base_url: &str, repo: &str) -> Result<GitHubRelease, ReleaseError> {
    fetch_release(&format!("{base_url}/repos/{repo}/releases/latest"))
}

/// `GET /repos/<repo>/releases/tags/<tag>` — `--ref <tag>`'s own lookup, and
/// `rollback <id>`'s way back to a previously installed release.
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
/// `project_name` yet (see the module header) — [`resolve_assets`]'s source
/// archive, or [`resolve_appimage_assets`]'s hub binary; the field names stay
/// generic across both callers rather than forking the struct in two.
#[derive(Debug)]
pub struct ResolvedAssets<'a> {
    /// The app version parsed from the tag after its transport-level `v`.
    pub version: Option<semver::Version>,
    pub archive_name: &'a str,
    pub archive_url: &'a str,
    pub checksums_url: &'a str,
}

/// `release`'s `SHA256SUMS.txt` asset, or the [`ReleaseError::MissingAsset`]
/// naming it — shared by [`resolve_assets`] and [`resolve_appimage_assets`]
/// so the lookup exists in exactly one place.
fn find_checksums(release: &GitHubRelease) -> Result<&GitHubAsset, ReleaseError> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == SHA256SUMS_ASSET_NAME)
        .ok_or_else(|| ReleaseError::MissingAsset {
            tag: release.tag_name.clone(),
            missing: SHA256SUMS_ASSET_NAME,
        })
}

/// Find `release`'s source archive and its `SHA256SUMS.txt`, or say which one
/// is missing (or ambiguous).
pub fn resolve_assets(release: &GitHubRelease) -> Result<ResolvedAssets<'_>, ReleaseError> {
    let version = release
        .tag_name
        .strip_prefix('v')
        .filter(|version| !version.is_empty())
        .and_then(|version| version::parse_app_version(version).ok())
        .ok_or_else(|| ReleaseError::InvalidTag {
            tag: release.tag_name.clone(),
        })?;
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
    let expected_suffix = format!("-{version}.tar.gz");
    if !archive.name.ends_with(&expected_suffix) {
        return Err(ReleaseError::ArchiveTagMismatch {
            tag: release.tag_name.clone(),
            archive: archive.name.clone(),
            expected_suffix,
        });
    }
    let checksums = find_checksums(release)?;

    Ok(ResolvedAssets {
        version: Some(version),
        archive_name: &archive.name,
        archive_url: &archive.browser_download_url,
        checksums_url: &checksums.browser_download_url,
    })
}

/// Find `release`'s hub `.AppImage` and its `SHA256SUMS.txt` — the hub
/// self-update's own view of [`resolve_assets`], matching a `.AppImage`
/// asset where that one matches `.tar.gz`, same exactly-one rule (an
/// ambiguous release is refused, not guessed) and the same
/// [`ReleaseError::MissingAsset`] vocabulary.
///
/// `#[allow(dead_code)]`: `hub_update::check` (step 2) is its first caller
/// outside tests; `hub_update::run` (step 4) is what makes it reachable from
/// `main`.
#[allow(dead_code)]
pub fn resolve_appimage_assets(
    release: &GitHubRelease,
) -> Result<ResolvedAssets<'_>, ReleaseError> {
    let appimages: Vec<&GitHubAsset> = release
        .assets
        .iter()
        .filter(|asset| asset.name.ends_with(".AppImage"))
        .collect();
    let appimage = match appimages.as_slice() {
        [one] => *one,
        [] => {
            return Err(ReleaseError::MissingAsset {
                tag: release.tag_name.clone(),
                missing: "a TFSAppHub_<version>_<arch>.AppImage asset",
            })
        }
        _ => {
            return Err(ReleaseError::MissingAsset {
                tag: release.tag_name.clone(),
                missing: "exactly one .AppImage asset — this release carries more than one",
            })
        }
    };
    let checksums = find_checksums(release)?;

    Ok(ResolvedAssets {
        version: None,
        archive_name: &appimage.name,
        archive_url: &appimage.browser_download_url,
        checksums_url: &checksums.browser_download_url,
    })
}

/// A `ureq` agent with the download timeouts — one place so [`download_to`]
/// and [`fetch_text`] share the same connect/read deadlines, distinct from
/// [`metadata_agent`]'s tighter ones.
fn download_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(DOWNLOAD_CONNECT_TIMEOUT)
        .timeout_read(DOWNLOAD_READ_TIMEOUT)
        .build()
}

/// Stream `url` to `path`, following the redirect GitHub's
/// `browser_download_url` issues to its signed objects host.
///
/// A failure here — transport or the local write — leaves `path` as whatever
/// was written so far; this function does not clean it up. That is deliberate
/// rather than an omission: the caller's scratch directory is removed
/// wholesale on any failure (this plan's Overview, "Where the archive lands
/// before it is trusted"), so a second cleanup here would only be a second
/// place for that guarantee to drift from the first.
pub fn download_to(url: &str, path: &Path) -> Result<(), ReleaseError> {
    let response = download_agent()
        .get(url)
        .set("User-Agent", GITHUB_USER_AGENT)
        .call()
        .map_err(|error| ReleaseError::Io(format!("download failed: {error}")))?;
    let mut reader = response.into_reader();
    let mut file = File::create(path)
        .map_err(|error| ReleaseError::Io(format!("cannot create {}: {error}", path.display())))?;
    io::copy(&mut reader, &mut file).map_err(|error| {
        ReleaseError::Io(format!(
            "download failed while writing {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

/// Fetch `url` as UTF-8 text — `SHA256SUMS.txt`'s body, small enough to hold
/// in memory unlike the archive itself.
pub fn fetch_text(url: &str) -> Result<String, ReleaseError> {
    download_agent()
        .get(url)
        .set("User-Agent", GITHUB_USER_AGENT)
        .call()
        .map_err(|error| ReleaseError::Io(format!("fetching checksums failed: {error}")))?
        .into_string()
        .map_err(|error| ReleaseError::Io(format!("reading checksums failed: {error}")))
}

/// The lowercase hex SHA-256 of the file at `path`, streamed through the
/// hasher via [`io::copy`] so a multi-hundred-megabyte archive is never fully
/// buffered.
pub fn sha256_file(path: &Path) -> Result<String, ReleaseError> {
    let mut file = File::open(path)
        .map_err(|error| ReleaseError::Io(format!("cannot open {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)
        .map_err(|error| ReleaseError::Io(format!("cannot read {}: {error}", path.display())))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Parse a `SHA256SUMS.txt` body — `sha256sum`'s own output, one
/// `<hex>  <filename>` line per file (two spaces in text mode, ` *` in binary
/// mode) — into a `{filename → lowercase hex hash}` map. Blank lines and
/// lines with no whitespace separator are skipped; hashes are lowercased so
/// [`verify`] can compare regardless of the producer's casing.
pub fn parse_sha256sums(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hash, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        // A leading `*` marks binary mode in `sha256sum` output — not part of
        // the filename.
        let filename = rest.strip_prefix('*').unwrap_or(rest);
        if hash.is_empty() || filename.is_empty() {
            continue;
        }
        map.insert(filename.to_string(), hash.to_ascii_lowercase());
    }
    map
}

/// Outcome of checking a computed hash against a parsed `SHA256SUMS.txt`: the
/// asset's line is present and matches, present and disagrees, or absent
/// entirely — three cases a caller reports distinctly. A missing line is
/// never a pass by absence: it is as much a failure as a mismatch, just a
/// different one to explain.
#[derive(Debug, PartialEq)]
pub enum VerifyOutcome {
    Match,
    Mismatch { expected: String, actual: String },
    MissingEntry,
}

/// Pure verification: look up `asset_name` in the parsed map and compare its
/// hash against `computed_hash`, case-insensitively.
pub fn verify(
    expected: &HashMap<String, String>,
    asset_name: &str,
    computed_hash: &str,
) -> VerifyOutcome {
    match expected.get(asset_name) {
        None => VerifyOutcome::MissingEntry,
        Some(expected_hash) => {
            let actual = computed_hash.to_ascii_lowercase();
            if expected_hash.eq_ignore_ascii_case(&actual) {
                VerifyOutcome::Match
            } else {
                VerifyOutcome::Mismatch {
                    expected: expected_hash.clone(),
                    actual,
                }
            }
        }
    }
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
    /// A release tag does not name a canonical app version.
    InvalidTag { tag: String },
    /// The source archive's version does not agree with the release tag.
    ArchiveTagMismatch {
        tag: String,
        archive: String,
        expected_suffix: String,
    },
    /// A release exists but is missing one of the two assets it must carry.
    MissingAsset { tag: String, missing: &'static str },
    /// Downloading an asset, fetching its checksums text, or hashing it back
    /// off disk failed — network transport or a local I/O error, distinct
    /// from the metadata calls above because neither "not found" nor "rate
    /// limited" means anything for a signed, one-shot download URL.
    Io(String),
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
            Self::InvalidTag { tag } => write!(
                formatter,
                "release tag {tag:?} does not follow the required v<app_version> form, with a \
                 canonical semantic version — ask the app's author to fix the release."
            ),
            Self::ArchiveTagMismatch {
                tag,
                archive,
                expected_suffix,
            } => write!(
                formatter,
                "release {tag} carries {archive}, but its source archive must end in \
                 {expected_suffix} — ask the app's author to fix the release."
            ),
            Self::MissingAsset { tag, missing } => write!(
                formatter,
                "release {tag} has no {missing} — ask the app's author to publish one that \
                 does, with `tfsapp-hub publish` or by hand per CONTRACT.md."
            ),
            Self::Io(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
