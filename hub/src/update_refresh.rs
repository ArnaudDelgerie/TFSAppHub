//! The background refresh that keeps `update_cache.json` current — the other
//! half of `update_check.rs`'s guarantee that answering `actions.update`
//! never opens a socket: something has to have opened one *before* the
//! question is asked, and this is it.
//!
//! Spawned once per launch, by `main::serve` after the window already exists
//! and the app is already running — never before, so a slow or offline
//! network can never delay a launch reaching its window. [`spawn`] is itself
//! the guard: no thread at all unless the launch is a release install that
//! declares `actions.update` on at least one transport. A local-path source,
//! a `dev` session, or an app that never declares the group costs the
//! network nothing.
//!
//! One request, one cache write, at most once per [`REFRESH_TTL`] per
//! repository — never per app, since two apps installed from the same
//! repository share the one cache entry `update_cache.rs` already keys on
//! `owner/repo`. A failure — offline, rate-limited, not found, an
//! unreadable response — changes nothing on disk and is logged to `hub.log`;
//! the next launch (this app's, or another sharing the same repository)
//! tries again once the entry is stale again. A release whose tag does not
//! parse as a version is a different case: still cached, raw, exactly as
//! published — `update_check::answer` is what turns that into
//! `no_answer_yet` at read time — but worth its own line in `hub.log`, since
//! nothing else will ever tell the app's author their tag does not parse.

use std::{
    path::{Path, PathBuf},
    thread,
};

use crate::{hub_update, paths::Paths, registry, release, update_cache, update_check};

/// How long a cached entry is trusted before the next launch refreshes it.
/// Not a config key or a CLI flag, and deliberately the same for every app:
/// CONTRACT.md's "pull, never push" posture rests on the hub alone deciding
/// how often it asks the forge, with nothing an app or a user can turn into
/// a tighter poll loop.
const REFRESH_TTL: time::Duration = time::Duration::hours(24);

/// Spawn the refresh thread for one launch, if it is worth spawning at all.
///
/// `context` is the same `Context` `main::serve` manages for IPC and hands to
/// the bridge — see `update_check.rs`. `declares_update` is
/// `manifest.actions.update.ipc || manifest.actions.update.bridge`:
/// `Context::Installed` alone is not enough to spawn on, since an installed
/// release that never declares the group is never asked and a refresh for it
/// would just be a request nobody wanted. `hub_log` is where a failure or a
/// malformed-tag warning is recorded; `None` swallows it silently rather than
/// panicking a launch over its own diagnostics, matching
/// `open::prepare_hub_log`'s own best-effort stance.
pub fn spawn(
    paths: Paths,
    context: &update_check::Context,
    declares_update: bool,
    hub_log: Option<PathBuf>,
) {
    let update_check::Context::Installed { source, .. } = context else {
        return;
    };
    if source.kind != registry::SourceKind::Release || !declares_update {
        return;
    }
    let source = source.clone();

    thread::spawn(
        move || match refresh_if_due(&paths, release::GITHUB_API_BASE, &source) {
            RefreshOutcome::NotDue
            | RefreshOutcome::Refreshed {
                malformed_tag: None,
            } => {}
            RefreshOutcome::Refreshed {
                malformed_tag: Some(tag),
            } => log(
                hub_log.as_deref(),
                &format!(
                    "{}'s latest release ({tag}) has a tag that does not parse as a version — \
                 actions.update will answer no_answer_yet for it until a release with a \
                 parseable tag is published.",
                    source.location
                ),
            ),
            RefreshOutcome::Failed(error) => log(
                hub_log.as_deref(),
                &format!(
                    "could not refresh {}'s release feed: {error}",
                    source.location
                ),
            ),
        },
    );
}

fn log(hub_log: Option<&Path>, message: &str) {
    if let Some(hub_log) = hub_log {
        tfsapp_core::log::append_log(hub_log, &format!("tfsapp-hub: warning: {message}"));
    }
}

/// What one refresh attempt did — [`spawn`]'s match, and what
/// `update_refresh_tests.rs` asserts against directly, with no thread and no
/// `hub_log` in the way.
#[derive(Debug, PartialEq)]
enum RefreshOutcome {
    /// The cached entry, if any, is still within [`REFRESH_TTL`] — no request
    /// was made at all.
    NotDue,
    /// A request was made and the cache was written. `malformed_tag` carries
    /// the raw tag when it does not parse as a version — still cached exactly
    /// as published, see this module's header.
    Refreshed { malformed_tag: Option<String> },
    /// The request failed, or the write did — either way the cache is
    /// untouched. `String` rather than `ReleaseError`/`UpdateCacheError`
    /// directly: the two failure sources have nothing else in common, and
    /// [`spawn`]'s only use for this is one line in `hub.log`.
    Failed(String),
}

/// One attempt, against `base_url` rather than GitHub's real API in
/// `update_refresh_tests.rs` — the same seam `release_tests.rs` and
/// `hub_update.rs` already use. Production always calls this with
/// [`release::GITHUB_API_BASE`], through [`spawn`] above.
fn refresh_if_due(paths: &Paths, base_url: &str, source: &registry::Source) -> RefreshOutcome {
    let cache = update_cache::load(paths);
    let due = match cache.get(&source.location) {
        None => true,
        Some(entry) => is_stale(&entry.checked_at),
    };
    if !due {
        return RefreshOutcome::NotDue;
    }

    let release = match release::fetch_latest_release_at(base_url, &source.location) {
        Ok(release) => release,
        Err(error) => return RefreshOutcome::Failed(error.to_string()),
    };
    let malformed_tag = hub_update::parse_tag_version(&release.tag_name)
        .err()
        .map(|_| release.tag_name.clone());

    let write = update_cache::update(paths, |cache| {
        cache.insert(
            source.location.clone(),
            update_cache::CachedRelease {
                checked_at: registry::now_timestamp(),
                tag: release.tag_name.clone(),
                release_url: release.html_url.clone(),
                notes: release.body.clone().unwrap_or_default(),
                unknown: serde_json::Map::new(),
            },
        );
    });
    if let Err(error) = write {
        return RefreshOutcome::Failed(error.to_string());
    }

    RefreshOutcome::Refreshed { malformed_tag }
}

/// Whether an RFC 3339 `checked_at` is more than [`REFRESH_TTL`] old. An
/// unparseable timestamp is treated as stale rather than propagated — the
/// same "cache is disposable" stance `update_cache.rs`'s own header states
/// for the file as a whole, applied to one entry.
fn is_stale(checked_at: &str) -> bool {
    let Ok(checked_at) =
        time::OffsetDateTime::parse(checked_at, &time::format_description::well_known::Rfc3339)
    else {
        return true;
    };
    time::OffsetDateTime::now_utc() - checked_at > REFRESH_TTL
}

#[cfg(test)]
#[path = "update_refresh_tests.rs"]
mod tests;
