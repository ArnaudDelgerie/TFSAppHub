//! `actions.update`, and the answer the hub can honestly give today.
//!
//! The group is **served** — over IPC and over the bridge, exactly as the
//! station serves it — and always answers `{"status": "unavailable", "reason":
//! …}`. That variant is the station's own, so an app needs no new shape, meets
//! no 404, and never branches on which host it is running under. Which is the
//! whole point: the app asks its host whether a newer version of itself exists,
//! and the host answers.
//!
//! **Why not the station's answer.** Over there the question resolves to a
//! `releases_repo` on GitHub, and the result carries the name, URL and size of a
//! newer **AppImage**. A hubbed app has no AppImage: it was installed from a
//! source the registry records, and it is updated with `tfsapp-hub update <id>`.
//! Pointing an app at an AppImage release it cannot apply would be worse than
//! saying nothing.
//!
//! **What the hub could answer, and why not yet.** The registry already holds
//! everything the real answer needs — `source.kind`, `source.location`, `ref`,
//! `reference_kind`, `app_version`, `source_revision`. A git source pinned to a
//! tag compares against the repository's newest tag; one on a branch is marked
//! unpinned and the question is whether its sha moved; a local path is re-read
//! from its own `tfsapp.config.json`, and `source_revision` answers the very
//! common case of a developer who changed the tree without bumping the version.
//! All of it needs the resolver `source.rs` has not grown yet. The applier is
//! written: `tfsapp-hub update <id>` (`update.rs`, the per-app update and
//! rollback plan). What is still missing is the hub self-update and lazy
//! revalidation plan (see `000-index.md`), and "a newer version exists" means
//! nothing until that question is answered about *sources* rather than acted
//! on by hand.
//!
//! CONTRACT.md §7 states this the way it should have been stated all along: the
//! result says what changed and nothing about how to apply it, because how an
//! update is applied belongs to the host.

use serde::{Deserialize, Serialize};

/// The wire shape, both transports (CONTRACT.md's `actions.update`). Internally
/// tagged on `status`, the station's spelling, so `{"status": "unavailable",
/// "reason": "…"}` serialises with no extra nesting — an app that already
/// handles the station's answers handles this one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum UpdateCheckResult {
    Ok {
        current: String,
        latest: String,
        update_available: bool,
        release_url: String,
        notes: String,
    },
    Unavailable {
        reason: String,
    },
}

/// The reason string the hub answers with.
///
/// A stable machine-readable token rather than a sentence, like every other
/// `reason` on this route: an app may want to hide its "check for updates"
/// button on this one and show a network error for another.
pub const HOST_RESOLVES_UPDATES: &str = "host_resolves_updates_itself";

/// The hub's answer. Never an error, whatever happens — a check that cannot be
/// made is a result, not a failure, which is what lets an app call it on a timer
/// without handling exceptions.
pub fn check() -> UpdateCheckResult {
    UpdateCheckResult::Unavailable {
        reason: HOST_RESOLVES_UPDATES.to_string(),
    }
}

#[tauri::command]
pub fn update_check() -> UpdateCheckResult {
    check()
}

#[cfg(test)]
#[path = "update_check_tests.rs"]
mod tests;
