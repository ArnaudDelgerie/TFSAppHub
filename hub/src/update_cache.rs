//! `update_cache.json` — every repository's resolved release feed, refreshed
//! in the background at most once a day (`update.rs`'s refresh thread, this
//! plan's step 5) and read by both transports on every `actions.update` call
//! (`update_check.rs`, step 3).
//!
//! One file under the hub's own root, keyed by `owner/repo` rather than by
//! installed app: every app installed from the same repository shares one
//! entry, so two apps polling the same feed cost one refresh, not two.
//!
//! Three properties this module owes its callers, deliberately looser than
//! `registry.rs`'s — the cache is disposable in a way the registry is not, and
//! its whole point is that nothing ever waits on it:
//!
//! - **A missing *or* unparseable file is "no entries", never an error.** The
//!   registry treats a malformed file as a hard failure worth refusing to
//!   touch; this file is a cache with no ground truth beyond it — worst case,
//!   a corrupt file is indistinguishable from a cold one, and the answer both
//!   produce is `no_answer_yet` until the next background refresh writes it.
//! - **Writes are atomic and serialised**, the same temp-file-plus-rename
//!   discipline under an exclusive lock that `registry.rs` uses, reused rather
//!   than reinvented — two overlapping refreshes for different repositories
//!   must not lose one entry to the other.
//! - **Unknown fields survive a read-modify-write**, per entry, for the same
//!   reason `registry.rs` carries them: a newer hub's cache file must still be
//!   readable, and re-writable without loss, by an older one.
//!
//! No network anywhere in this module — it only ever reads and writes what
//! `release.rs` already fetched.

// `update_check::answer_now` (step 4) is this module's first real caller, of
// `load`/`load_from`. `update` and `UpdateCacheError` are still only
// exercised by this module's own tests — the background refresh (step 5) is
// their real caller. Remove the allow once that lands too.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// The whole file: one entry per `owner/repo`.
pub type UpdateCache = BTreeMap<String, CachedRelease>;

/// One repository's last successfully resolved release.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CachedRelease {
    /// RFC 3339, UTC — when this entry was written, and what the background
    /// refresh's TTL is measured against.
    pub checked_at: String,
    /// The release tag, verbatim (`v1.2.0`) — `update_check.rs` is what turns
    /// this into a comparable version; this module has no opinion on its
    /// shape.
    pub tag: String,
    pub release_url: String,
    #[serde(default)]
    pub notes: String,
    /// Keys a newer hub wrote and this one does not know. Same rule as
    /// `registry::RegistryEntry::unknown`.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// Read the cache. A missing *or* unparseable file is empty — see the module
/// header for why that differs from `registry::load`.
pub fn load(paths: &Paths) -> UpdateCache {
    load_from(&paths.update_cache_path())
}

/// [`load`], from a bare path rather than a [`Paths`] — what
/// `update_check::answer_now` calls, since a launch's `Context` already
/// resolved the cache path once and has no reason to rebuild a whole `Paths`
/// from it on every request.
pub fn load_from(path: &Path) -> UpdateCache {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return UpdateCache::default(),
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Read, modify and write under one lock — the primitive a refresh wants:
/// holding the lock across the whole cycle is what keeps two concurrent
/// refreshes (different repositories, or the same one racing itself) from
/// each starting from the same snapshot and one write erasing the other's.
pub fn update<T>(
    paths: &Paths,
    change: impl FnOnce(&mut UpdateCache) -> T,
) -> Result<T, UpdateCacheError> {
    let _lock = lock(paths)?;
    let mut cache = load(paths);
    let outcome = change(&mut cache);
    write_locked(paths, &cache)?;
    Ok(outcome)
}

fn lock(paths: &Paths) -> Result<fs::File, UpdateCacheError> {
    let hub_root = paths.hub_root();
    fs::create_dir_all(&hub_root).map_err(|source| UpdateCacheError::Io {
        path: hub_root,
        source,
    })?;

    let path = paths.update_cache_lock_path();
    tfsapp_core::process::lock_file_exclusive(&path)
        .map_err(|source| UpdateCacheError::Io { path, source })
}

/// Temp file, `fsync`, rename — the caller must already hold the lock. The
/// same discipline as `registry::write_locked`, kept separate because the two
/// files have nothing else in common.
fn write_locked(paths: &Paths, cache: &UpdateCache) -> Result<(), UpdateCacheError> {
    let mut json =
        serde_json::to_vec_pretty(cache).map_err(|error| UpdateCacheError::Unserialisable {
            detail: error.to_string(),
        })?;
    json.push(b'\n');

    let hub_root = paths.hub_root();
    let target = paths.update_cache_path();
    // A fixed temp name is safe because the lock is held: only one writer is
    // ever between `create` and `rename`.
    let temp = target.with_extension("json.tmp");

    let io_error = |path: &std::path::Path| {
        let path = path.to_path_buf();
        move |source| UpdateCacheError::Io { path, source }
    };

    let mut file = fs::File::create(&temp).map_err(io_error(&temp))?;
    file.write_all(&json).map_err(io_error(&temp))?;
    file.sync_all().map_err(io_error(&temp))?;
    drop(file);

    fs::rename(&temp, &target).map_err(io_error(&target))?;

    fs::File::open(&hub_root)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error(&hub_root))?;

    Ok(())
}

#[derive(Debug)]
pub enum UpdateCacheError {
    Io { path: PathBuf, source: io::Error },
    Unserialisable { detail: String },
}

impl fmt::Display for UpdateCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Unserialisable { detail } => {
                write!(formatter, "cannot serialise the update cache: {detail}")
            }
        }
    }
}

impl std::error::Error for UpdateCacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "update_cache_tests.rs"]
mod tests;
