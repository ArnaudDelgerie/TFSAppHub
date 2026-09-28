//! `actions.update`'s answer, as a pure function over what the hub already
//! knows: the recorded [`registry::Source`], the installed `app_version`, and
//! whatever [`update_cache`] holds for that source's repository.
//!
//! No network here, and no filesystem beyond what the caller already read —
//! [`answer`] is the whole table CONTRACT.md §7 now states: a local-archive
//! source (or a `dev` session, which has none at all) answers `local_source`;
//! a release with nothing cached yet, or a cached tag that will not parse as
//! a version, answers `no_answer_yet`; otherwise `ok`, comparing the cached
//! tag against the installed version. This module has no opinion on *how*
//! the cache gets populated — that is the background refresh's job
//! (`../plan/021-the-update-check-an-app-can-read.md`'s step 5).
//!
//! [`Context`] and [`answer_now`] are step 4's: the one piece of state each
//! launch resolves once — from the registry entry `open::resolve` already
//! read, or `Dev` outright for a session with no registry entry at all — and
//! hands to both transports (`main::serve`'s `app.manage`, `sidecar::start`'s
//! bridge context). Neither transport opens a socket to answer a poll; both
//! just re-read whatever [`update_cache`] holds at the moment they are asked.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::Manager;

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

/// Installed from a local release archive, or a `dev` session — no release
/// feed exists, and the question has no other meaning there (CONTRACT.md §7).
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
    if matches!(source.kind, registry::SourceKind::LocalArchive) {
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

/// What `actions.update`'s answer needs about one launch, resolved once —
/// `open::resolve` already reads the registry entry this is built from, and
/// `main::serve` hands a clone to `sidecar::start` for the bridge alongside
/// managing one for IPC.
///
/// `Dev` covers a `dev` session (`launch::Source::Live`), which has no
/// registry entry at all — [`REASON_LOCAL_SOURCE`] is the whole answer there,
/// with nothing to look up. An app *installed* from a local release archive
/// is a different case, `Installed` with a
/// [`registry::SourceKind::LocalArchive`] source: it does have an entry, and
/// [`answer`] is what turns that into the same reason.
#[derive(Debug, Clone)]
pub enum Context {
    Installed {
        source: registry::Source,
        app_version: String,
        cache_path: PathBuf,
    },
    Dev,
}

/// The real answer for one launch, as either transport's route handler calls
/// it. The cache is re-read from disk on every call — no socket opens here,
/// whatever `cache_path` holds is exactly what the background refresh last
/// wrote — which is what makes this, unlike [`answer`], not a pure function.
pub fn answer_now(context: &Context) -> UpdateCheckResult {
    match context {
        Context::Dev => unavailable(REASON_LOCAL_SOURCE),
        Context::Installed {
            source,
            app_version,
            cache_path,
        } => {
            let cache = update_cache::load_from(cache_path);
            answer(source, app_version, cache.get(&source.location))
        }
    }
}

/// The IPC side: `window`'s app handle carries the `Context` `main::serve`
/// managed for this launch. Not-yet-managed only happens for a webview call
/// that somehow lands before that `app.manage` runs — impossible on the
/// ordinary path, since the window is only navigated to the backend after —
/// so it is answered the same as "nothing resolved yet" rather than treated
/// as a distinct error.
#[tauri::command]
pub fn update_check(window: tauri::Window) -> UpdateCheckResult {
    match window.app_handle().try_state::<Context>() {
        Some(context) => answer_now(&context),
        None => unavailable(REASON_NO_ANSWER_YET),
    }
}

#[cfg(test)]
#[path = "update_check_tests.rs"]
mod tests;
