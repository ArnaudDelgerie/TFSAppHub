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
//! Step 3 adds the swap mechanics' pure half, in isolation from the network
//! and from the actual swap: the `$APPIMAGE` guard, and the rollback
//! anchor's readiness. `run` itself (step 4) and `--rollback`
//! (`hub_rollback.rs`, step 7) are what call into these.

// `check`, `resolve_appimage_target` and `anchor_state` have no real caller
// until `run` (step 4) and `hub_rollback.rs` (step 7) exist to call them.
// Remove the allow once those callers land.
#![allow(dead_code)]

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use crate::{
    hub_bin,
    release::{self, GitHubRelease},
};

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

/// `$APPIMAGE`'s value, refused when unset or empty — the Overview's order,
/// step 1: "`$APPIMAGE` is set — otherwise refuse, nothing contacted." Pure
/// over an already-read environment variable rather than reading it itself,
/// so the unset/empty/set cases are each one line to test. `None` means this
/// is not a packaged hub — a `cargo run`, a plain binary — and there is no
/// image on disk for `--update` to replace.
pub(crate) fn resolve_appimage_target(appimage_env: Option<&str>) -> Option<PathBuf> {
    match appimage_env {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Whether `a` and `b` are the same file — `$APPIMAGE` against the stable
/// copy (the Overview's "Two files, not one": when they coincide there is
/// one swap, not two). Reuses `hub_bin::same_file`'s canonicalize comparison
/// rather than a second one.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    hub_bin::same_file(a, b)
}

/// Which half of the rollback anchor is missing — [`anchor_state`]'s
/// refusal, named so the message can say which one rather than a bare "no
/// anchor": half-present is exactly as unusable as absent, but it is a
/// different thing to have gone wrong (an interrupted `--update`, never a
/// network problem).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingAnchorHalf {
    Binary,
    Registry,
    Both,
}

impl fmt::Display for MissingAnchorHalf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Binary => "the previous hub binary",
            Self::Registry => "its registry snapshot",
            Self::Both => "the previous hub binary and its registry snapshot",
        };
        write!(formatter, "{text}")
    }
}

/// Whether the rollback anchor left by a previous `--update` is usable:
/// `binary_path` present and non-empty, `registry_snapshot_path` present
/// beside it. No version comparison against the running hub — correction 3
/// of the plan's Overview: the anchor is deleted by the `--rollback` that
/// consumes it, so its mere presence already answers "is there something to
/// roll back to".
pub(crate) fn anchor_state(
    binary_path: &Path,
    registry_snapshot_path: &Path,
) -> Result<(), MissingAnchorHalf> {
    let binary_present = fs::metadata(binary_path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false);
    let registry_present = registry_snapshot_path.is_file();

    match (binary_present, registry_present) {
        (true, true) => Ok(()),
        (false, true) => Err(MissingAnchorHalf::Binary),
        (true, false) => Err(MissingAnchorHalf::Registry),
        (false, false) => Err(MissingAnchorHalf::Both),
    }
}

#[cfg(test)]
#[path = "hub_update_tests.rs"]
mod tests;
