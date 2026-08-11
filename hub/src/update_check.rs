//! `actions.update`'s answer, as a pure function over what the hub already
//! knows: the recorded [`registry::Source`], the installed `app_version`, and
//! whatever [`update_cache`] holds for that source's repository.
//!
//! No network here, and no filesystem beyond what the caller already read —
//! [`answer`] is the whole table CONTRACT.md §7 now states: a local-path
//! source (or a `dev` session, which has none at all) answers `local_source`;
//! a release with nothing cached yet, or a cached tag that will not parse as
//! a version, answers `no_answer_yet`; otherwise `ok`, comparing the cached
//! tag against the installed version. This module has no opinion on *how*
//! the cache gets populated — that is the background refresh's job
//! (`../plan/021-the-update-check-an-app-can-read.md`'s step 5) — or on how
//! either transport reaches it, which is step 4's.

// `answer` has no caller yet outside its own tests — `check()`/`update_check()`
// below are what `main.rs` and `bridge.rs` still call, until step 4 rewires
// both to build real context and call `answer` directly. Remove the allow
// when that lands.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::{hub_update, registry, update_cache};

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

/// Installed from a local directory, or a `dev` session — no release feed
/// exists, and the question has no other meaning there (CONTRACT.md §7).
pub const REASON_LOCAL_SOURCE: &str = "local_source";

/// A release-installed app whose cache holds nothing usable yet: the first
/// launch after install, every refresh attempted so far has failed, or the
/// cached tag does not parse as a version. Offline, rate-limited, a malformed
/// release and a 404 all collapse into this same token — an app cannot act
/// differently on any of them.
pub const REASON_NO_ANSWER_YET: &str = "no_answer_yet";

/// The hub's answer for one app, from what it already knows — never an error,
/// whatever `cached` holds, which is what lets an app call this on a timer
/// without handling exceptions.
///
/// `app_version` is taken as already-validated semver, the same invariant
/// `install::validate` establishes for every registry entry (see
/// `install.rs`'s `lifecycle_event_for_install`) — the parse below cannot
/// fail in practice.
pub fn answer(
    source: &registry::Source,
    app_version: &str,
    cached: Option<&update_cache::CachedRelease>,
) -> UpdateCheckResult {
    if source.kind == registry::SourceKind::LocalPath {
        return unavailable(REASON_LOCAL_SOURCE);
    }

    let Some(cached) = cached else {
        return unavailable(REASON_NO_ANSWER_YET);
    };

    let Ok(latest) = hub_update::parse_tag_version(&cached.tag) else {
        return unavailable(REASON_NO_ANSWER_YET);
    };

    let current = semver::Version::parse(app_version)
        .expect("validate() already refused a non-canonical app_version");

    UpdateCheckResult::Ok {
        current: current.to_string(),
        latest: latest.to_string(),
        update_available: latest > current,
        release_url: cached.release_url.clone(),
        notes: cached.notes.clone(),
    }
}

fn unavailable(reason: &str) -> UpdateCheckResult {
    UpdateCheckResult::Unavailable {
        reason: reason.to_string(),
    }
}

/// The pre-context entry points both transports still call directly (`main.rs`'s
/// `invoke_handler`, `bridge.rs`'s `/update/check` route). Neither has anything
/// to answer [`answer`] with yet — no launch context is threaded through
/// either side before this plan's step 4 — so both simply report the one
/// honest thing true of every app today: nothing is cached, because nothing
/// populates the cache before step 5 either. Step 4 removes this pair and
/// calls [`answer`] with the real thing.
pub fn check() -> UpdateCheckResult {
    unavailable(REASON_NO_ANSWER_YET)
}

#[tauri::command]
pub fn update_check() -> UpdateCheckResult {
    check()
}

#[cfg(test)]
#[path = "update_check_tests.rs"]
mod tests;
