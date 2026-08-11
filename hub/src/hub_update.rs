//! `tfsapp-hub --update` — the hub replacing itself with its own latest
//! release (`../plan/020-hub-self-update-and-revalidation.md`).
//!
//! This module starts with the one thing worth getting right before any I/O
//! exists around it: [`check`], the pure comparison between the hub's own
//! running version and an already-fetched release. Reusing the station's
//! `update.rs` `evaluate_release` shape (`v<version>` tag, `>` for "newer",
//! never a downgrade) rather than a second parser — ported here because the
//! hub has no manifest to read its own next version from the way an app
//! does; the tag is the only place it lives.
//!
//! The swap mechanics, the anchor and `run` itself are steps 3–4 of the same
//! plan and land in this file as they are written.

// `check` has no real caller until `run` (step 4) exists to fetch a release
// and hand it here. Remove the allow once that caller lands.
#![allow(dead_code)]

use crate::release::{self, GitHubRelease};

/// [`check`]'s outcome — pure over an already-fetched release, so it needs no
/// network of its own; `run` (step 4) is what calls
/// [`release::fetch_latest_release_at`] before handing the result here.
#[derive(Debug, PartialEq)]
pub enum HubUpdateCheck<'a> {
    /// The forge's latest release is newer than the running hub, and carries
    /// everything `--update` needs to fetch it.
    Available {
        version: semver::Version,
        asset_name: &'a str,
        asset_url: &'a str,
        checksums_url: &'a str,
    },
    /// The forge's latest release is this version or older. Never treated as
    /// a downgrade to refuse — `--update` follows `latest` and there is no
    /// `--ref` on it (the plan's Out of scope), so "older" only ever means
    /// "nothing to do".
    UpToDate,
    /// The release exists but could not be turned into something `--update`
    /// can act on: an unparseable tag, or a release missing one of its two
    /// required assets. The message is [`release::ReleaseError`]'s own, or
    /// names the tag directly — either way, something a user can read
    /// without a second translation.
    Unavailable(String),
}

/// Compare `current` — the running hub's own version — against `release`,
/// already fetched by the caller. No network, no filesystem.
pub fn check<'a>(current: &semver::Version, release: &'a GitHubRelease) -> HubUpdateCheck<'a> {
    let tag = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let latest = match semver::Version::parse(tag) {
        Ok(version) => version,
        Err(error) => {
            return HubUpdateCheck::Unavailable(format!(
                "release {} has a tag that is not a version ({error})",
                release.tag_name
            ))
        }
    };

    if latest <= *current {
        return HubUpdateCheck::UpToDate;
    }

    match release::resolve_appimage_assets(release) {
        Ok(assets) => HubUpdateCheck::Available {
            version: latest,
            asset_name: assets.archive_name,
            asset_url: assets.archive_url,
            checksums_url: assets.checksums_url,
        },
        Err(error) => HubUpdateCheck::Unavailable(error.to_string()),
    }
}

#[cfg(test)]
#[path = "hub_update_tests.rs"]
mod tests;
