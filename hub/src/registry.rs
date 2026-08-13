//! `registry.json` — what is installed, from where, at what version.
//!
//! One file under the hub's own root, holding one entry per installed app plus
//! a little about the hub that last wrote it. Its shape is deliberately stable:
//! retrofitting a field would mean migrating installed users' state.
//!
//! Three properties this module owes its callers, in the order they bite:
//!
//! - **A missing file is an empty registry, not an error.** The overwhelmingly
//!   common state on a fresh machine is "nothing installed", and a `list` that
//!   errors there would be a bug, not a diagnostic.
//! - **Writes are atomic and serialised.** Two `install`s can genuinely
//!   overlap, so a write goes to a temp file in the same directory, is
//!   `fsync`ed, and is renamed over the target while an exclusive lock is held.
//!   A reader therefore always sees one complete generation of the file — never
//!   a half-written one — which is why [`load`] takes no lock.
//! - **Unknown fields survive a read-modify-write.** A registry written by a
//!   newer hub, read and rewritten by this one, keeps the fields this one has
//!   never heard of, at both the top level and per entry. It is the same
//!   additive-growth rule the manifest's unknown-key warning serves, applied to
//!   the hub's own state: a user who tries a newer hub and steps back must not
//!   lose what it recorded.
//!
//! There is deliberately no `format_version` field. `hub_version` already
//! records which hub last wrote the file, which is what a future reader would
//! actually want to reason about, and a second version number invites
//! migration machinery that nothing has asked for yet.

// Kept temporarily while a few helpers have no production caller. The todo
// session that removes this module-wide allow will delete or narrow them.
#![allow(dead_code)]

use std::{
    fmt, fs,
    io::{self, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// The whole file.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Registry {
    /// The version of the hub that last wrote this file. Not a format version
    /// (see the module header) — it is what lets a later reader say "written
    /// by an older hub" without a second number to keep in step. `None` on a
    /// registry no hub has stamped yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_version: Option<String>,
    /// The platform fingerprint of the hub that last wrote this file. Compared
    /// against each entry's own after a hub self-update: where they differ, PHP
    /// has moved under an installed app and that app needs revalidating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    /// One per installed app, in install order — which is the order `list`
    /// prints, so it must be preserved rather than sorted on write.
    #[serde(default)]
    pub apps: Vec<RegistryEntry>,
    /// Top-level keys a newer hub wrote and this one does not know. Carried
    /// through untouched; never read, never invented.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// One installed app.
///
/// Every field earns its place by answering a question the hub cannot answer
/// any other way — a registry field is cheap to add and expensive to change, so
/// the reason for each is recorded here rather than rediscovered later.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RegistryEntry {
    /// The hub-local handle, used on the CLI and as the directory name under
    /// `apps/`. Distinct from `identifier` on purpose: this one is the hub's to
    /// choose and keep short, that one is the app's and is a reverse-DNS
    /// string.
    pub id: String,
    /// The app's own `identifier`, from its `tfsapp.config.json`. The data dir,
    /// the keyring namespace and the window identity all derive from it — which
    /// is what makes a hubbed install and a packaged AppImage of the same app
    /// share one data dir. Recorded here so `list`, `remove` and `open` need
    /// not re-read the manifest to name the app's data.
    pub identifier: String,
    /// Where the installed snapshot came from, and how to re-resolve it on
    /// `update`.
    pub source: Source,
    /// The installed `app_version`, and the lifecycle authority: `update`
    /// compares the source tree's against this one to decide install / update /
    /// downgrade. Never the git ref, which is only a source selector.
    pub app_version: String,
    /// The git sha, or a content hash of the tree for a plain directory. It
    /// answers the question `app_version` cannot — *has the source changed
    /// since I installed it?* — for the very common case of a developer who
    /// forgot to bump the version. Without it, `list` cannot flag a stale
    /// source and `update --force` stays buried in `--help`.
    pub source_revision: String,
    /// The static port the app pinned, if any. Recorded rather than re-read so
    /// an install can refuse, or warn about, a second app pinning a port
    /// another one already claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    /// The fingerprint of the hub that installed (or last revalidated) this
    /// app. See [`Platform`].
    pub platform: Platform,
    /// Whether the app is usable as it stands. Stored and round-tripped here;
    /// what moves it and what acts on it is the revalidation flow's job, not
    /// this module's.
    pub state: State,
    /// RFC 3339, UTC. Kept as strings rather than a timestamp type because
    /// their only consumers are a human reading `list` and a human reading the
    /// file — no arithmetic is done on them.
    pub installed_at: String,
    pub updated_at: String,
    /// Per-entry keys a newer hub wrote and this one does not know. Same rule
    /// as [`Registry::unknown`], and the reason [`Registry::upsert`] carries
    /// them over rather than replacing an entry wholesale.
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// Where an installed snapshot came from.
///
/// `install` copies the resolved tree into `apps/<id>/`, always — there is no
/// live link back to the source, and editing it changes nothing until
/// `update <id>` runs. What this records is how to resolve that source again.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Source {
    pub kind: SourceKind,
    /// A filesystem path for a local source. For a release, the index's own
    /// key for the app — today that is `owner/repo`, the forge acting as its
    /// own index (see [`Source::index`]).
    pub location: String,
    /// The selector, when there is one: a tag. A plain directory has none.
    /// Named `ref` in the file, since that is what it is, and `reference` in
    /// Rust, where `ref` is a keyword.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// What that selector *is*, recorded by whoever resolved it. Every release
    /// records `Tag` — there is no other kind of selector a release resolver
    /// can produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_kind: Option<ReferenceKind>,
    /// Which index resolved this release — `"github"` today, the forge acting
    /// as its own index. `hub/004` §6 requires recording *which index an app
    /// came from*, not a resolved URL, since revocation and refresh cannot be
    /// added later over a field that only remembers a download link. `None`
    /// for a local source, which has no index at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
}

/// What a source's `ref` selects.
///
/// `Tag` is the only kind a release resolver ever produces — spelled out
/// rather than left implicit so a reader of the file can tell a resolver that
/// recorded a selector from one that recorded nothing. `Commit` is declared
/// but resolved by nobody yet, kept for the same reason `SourceKind::Release`
/// once was: an enum grown ahead of its resolver rather than retrofitted into
/// users' registries later.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReferenceKind {
    Tag,
    Commit,
}

/// The source kinds the hub anticipates.
///
/// A remote source is always a **release** (`../decision/002-remote-sources-
/// are-releases.md`): no git client, no branch, no arbitrary commit. The hub
/// resolves a tag through the forge's release API, verifies the published
/// archive, and installs the extracted tree exactly as it would a local
/// directory.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    LocalPath,
    Release,
}

/// Whether an installed app is usable as it stands.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Installed and known to work against the current hub's PHP.
    #[default]
    Ready,
    /// The hub's platform moved under it; `composer install` against the
    /// existing `composer.lock` has not been re-run yet. Lazy on purpose —
    /// triggered by the app's next use, not by the hub update itself.
    NeedsRevalidation,
    /// Revalidation failed: this app's `composer.lock` cannot satisfy the
    /// current PHP. Recoverable by rolling the hub back or by updating the app,
    /// never by pretending otherwise.
    Broken,
}

impl fmt::Display for State {
    /// The same spelling the file uses, so a `list` line and `registry.json`
    /// never disagree about what an app's state is called.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            Self::Ready => "ready",
            Self::NeedsRevalidation => "needs-revalidation",
            Self::Broken => "broken",
        };
        formatter.write_str(word)
    }
}

/// The platform an app was installed against.
///
/// PHP's major.minor plus a hash of its sorted loaded-extension list — the two
/// things a `composer.lock` was resolved against, and the two a hub self-update
/// can move under an installed app. Comparing an app's recorded fingerprint
/// with the running hub's is what marks it for revalidation.
///
/// It is **not** a security check and **not** a version pin. It proves nothing
/// about the binary and blocks no launch on its own: an app whose fingerprint
/// differs is revalidated, not refused. Computing it is `platform.rs`'s job;
/// the type lives here because the registry is what it is for.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `major.minor` — the granularity a `composer.lock`'s platform
    /// requirements are written at. A patch bump moves no constraint.
    pub php_version: String,
    /// Hex SHA-256 of the sorted extension list. A hash rather than the list
    /// itself: the comparison is equality, and the list is long enough to make
    /// the file unreadable.
    pub extensions_hash: String,
}

impl fmt::Display for Platform {
    /// A short, eyeballable form: `8.5+a1b2c3d4`. The full hash stays in the
    /// file; nobody compares 64 hex characters by eye.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let short: String = self.extensions_hash.chars().take(8).collect();
        write!(formatter, "{}+{short}", self.php_version)
    }
}

impl Registry {
    /// Whether both registries describe the same installed apps. The
    /// hub-level stamp is deliberately excluded: a hub rollback restores that
    /// stamp from its snapshot while app entries continue to describe the
    /// trees that are actually on disk.
    pub fn app_entries_match(&self, other: &Self) -> bool {
        self.apps == other.apps
    }

    pub fn get(&self, id: &str) -> Option<&RegistryEntry> {
        self.apps.iter().find(|entry| entry.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut RegistryEntry> {
        self.apps.iter_mut().find(|entry| entry.id == id)
    }

    /// The entry registered under `identifier`, if any — the question
    /// `install::check_identifier_free` and `purge <identifier>` (plan 023)
    /// both have to ask the same way, since a collision here is what tells
    /// either of them the identifier is not an orphan's to touch.
    pub fn by_identifier(&self, identifier: &str) -> Option<&RegistryEntry> {
        self.apps
            .iter()
            .find(|entry| entry.identifier == identifier)
    }

    /// Add `entry`, or replace the one already carrying its `id` in place —
    /// keeping the list in install order rather than moving an updated app to
    /// the end.
    ///
    /// The replaced entry's unknown fields are carried over onto the new one:
    /// a field a newer hub recorded per app must survive this hub updating that
    /// same app, exactly as top-level unknown fields survive a rewrite. A
    /// caller that genuinely wants to drop them can clear the map itself.
    pub fn upsert(&mut self, mut entry: RegistryEntry) {
        match self
            .apps
            .iter_mut()
            .find(|existing| existing.id == entry.id)
        {
            Some(existing) => {
                for (key, value) in existing.unknown.clone() {
                    entry.unknown.entry(key).or_insert(value);
                }
                *existing = entry;
            }
            None => self.apps.push(entry),
        }
    }

    /// Drop the entry for `id`, returning it — `remove <id>`'s half of the
    /// registry work.
    pub fn remove(&mut self, id: &str) -> Option<RegistryEntry> {
        let at = self.apps.iter().position(|entry| entry.id == id)?;
        Some(self.apps.remove(at))
    }

    /// Record which hub is writing this file, and what its PHP looks like.
    /// One call rather than two assignments so the pair cannot drift: a
    /// `hub_version` without the matching `platform` is what would make a later
    /// revalidation compare against the wrong baseline.
    pub fn stamp(&mut self, hub_version: &str, platform: Platform) {
        self.hub_version = Some(hub_version.to_string());
        self.platform = Some(platform);
    }
}

/// Read the registry. A missing file is an empty registry.
///
/// Takes no lock: every write lands by `rename`, so a reader either sees the
/// whole previous generation or the whole new one. Locking a read would only
/// make `list` wait behind an install for no gain in what it can observe.
pub fn load(paths: &Paths) -> Result<Registry, RegistryError> {
    let path = paths.registry_path();
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(source) => return Err(RegistryError::Io { path, source }),
    };

    serde_json::from_str(&contents).map_err(|error| RegistryError::Malformed {
        path,
        detail: error.to_string(),
    })
}

/// Write `registry`, atomically, under the exclusive lock.
///
/// Callers that need to read *and* write should use [`update`] instead: this
/// one takes the lock only for the write, so two racing read-modify-write
/// cycles built on top of it would lose one of the two modifications.
pub fn save(paths: &Paths, registry: &Registry) -> Result<(), RegistryError> {
    let _lock = lock(paths)?;
    write_locked(paths, registry)
}

/// Read, modify and write under one lock — the primitive an install wants.
///
/// Holding the lock across the whole cycle is what makes concurrent installs
/// safe rather than merely non-corrupting: without it both would read the same
/// starting state and the second write would erase the first app.
pub fn update<T>(
    paths: &Paths,
    change: impl FnOnce(&mut Registry) -> T,
) -> Result<T, RegistryError> {
    let _lock = lock(paths)?;
    let mut registry = load(paths)?;
    let outcome = change(&mut registry);
    write_locked(paths, &registry)?;
    Ok(outcome)
}

/// Take the registry's exclusive lock, waiting if another install holds it.
///
/// The lock file sits beside `registry.json` rather than being it: an atomic
/// write replaces the target inode, which would silently drop a lock taken on
/// the file itself.
fn lock(paths: &Paths) -> Result<fs::File, RegistryError> {
    let hub_root = paths.hub_root();
    fs::create_dir_all(&hub_root).map_err(|source| RegistryError::Io {
        path: hub_root,
        source,
    })?;

    let path = paths.registry_lock_path();
    tfsapp_core::process::lock_file_exclusive(&path)
        .map_err(|source| RegistryError::Io { path, source })
}

/// Temp file, `fsync`, rename — the caller must already hold the lock.
fn write_locked(paths: &Paths, registry: &Registry) -> Result<(), RegistryError> {
    let mut json =
        serde_json::to_vec_pretty(registry).map_err(|error| RegistryError::Unserialisable {
            detail: error.to_string(),
        })?;
    json.push(b'\n');
    write_bytes_locked(paths, &json)
}

/// The write half both [`write_locked`] and [`restore_from`] share: temp
/// file, `fsync`, rename, directory `fsync` — the caller must already hold
/// the lock. Factored out because a rollback restore writes bytes read
/// verbatim from an anchor snapshot rather than a freshly serialised
/// [`Registry`], and the two must land on disk the same careful way.
fn write_bytes_locked(paths: &Paths, bytes: &[u8]) -> Result<(), RegistryError> {
    let hub_root = paths.hub_root();
    let target = paths.registry_path();
    // A fixed temp name is safe because the lock is held: only one writer is
    // ever between `create` and `rename`. A name left behind by a crashed
    // writer is simply truncated by the next one.
    let temp = target.with_extension("json.tmp");

    let io_error = |path: &std::path::Path| {
        let path = path.to_path_buf();
        move |source| RegistryError::Io { path, source }
    };

    let mut file = fs::File::create(&temp).map_err(io_error(&temp))?;
    file.write_all(bytes).map_err(io_error(&temp))?;
    // Before the rename, not after: a rename that lands ahead of the content
    // is exactly the crash window this ordering closes.
    file.sync_all().map_err(io_error(&temp))?;
    drop(file);

    fs::rename(&temp, &target).map_err(io_error(&target))?;

    // The rename itself is metadata, and metadata needs its directory synced
    // for the new name to survive a power loss.
    fs::File::open(&hub_root)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error(&hub_root))?;

    Ok(())
}

/// Write the current registry to `destination`, under the exclusive lock —
/// the rollback anchor's registry half (`hub_update.rs`, `../plan/020-hub-
/// self-update-and-revalidation.md`'s Overview, step 7). Reads through
/// [`load`] rather than `fs::copy`-ing `registry.json` directly, so a
/// never-written registry (nothing installed yet) snapshots as the same
/// empty [`Registry::default`] a fresh read would answer with, instead of a
/// missing-file error.
///
/// `hub_update::run` (this plan's step 4) is its caller.
pub fn snapshot_to(paths: &Paths, destination: &std::path::Path) -> Result<(), RegistryError> {
    let _lock = lock(paths)?;
    let registry = load(paths)?;

    let mut json =
        serde_json::to_vec_pretty(&registry).map_err(|error| RegistryError::Unserialisable {
            detail: error.to_string(),
        })?;
    json.push(b'\n');

    fs::write(destination, json).map_err(|source| RegistryError::Io {
        path: destination.to_path_buf(),
        source,
    })
}

/// Restore `registry.json` from `source`, under the exclusive lock — the
/// rollback anchor's registry half restored (`hub_rollback.rs`, this plan's
/// step 7), the exact inverse of [`snapshot_to`]. Copies `source`'s bytes
/// verbatim rather than reading them through a [`Registry`] and
/// re-serialising: the restored file ends up byte-identical to the snapshot,
/// not merely equivalent under it.
pub fn restore_from(paths: &Paths, source: &std::path::Path) -> Result<(), RegistryError> {
    let bytes = fs::read(source).map_err(|read_error| RegistryError::Io {
        path: source.to_path_buf(),
        source: read_error,
    })?;
    let _lock = lock(paths)?;
    write_bytes_locked(paths, &bytes)
}

/// Read a registry snapshot without changing the live registry.
pub fn load_from(source: &std::path::Path) -> Result<Registry, RegistryError> {
    let contents = fs::read_to_string(source).map_err(|source_error| RegistryError::Io {
        path: source.to_path_buf(),
        source: source_error,
    })?;
    serde_json::from_str(&contents).map_err(|error| RegistryError::Malformed {
        path: source.to_path_buf(),
        detail: error.to_string(),
    })
}

/// Keep the live app entries, but restore the hub-level stamp from `source`.
///
/// The operation holds the same exclusive lock as [`restore_from`], including
/// while it reads the live registry and writes the merged result. It returns
/// the live registry that won, so callers can report the entries they kept.
pub fn restore_hub_stamp_from(
    paths: &Paths,
    source: &std::path::Path,
) -> Result<Registry, RegistryError> {
    let snapshot = load_from(source)?;
    let _lock = lock(paths)?;
    let mut live = load(paths)?;
    let kept = live.clone();
    live.hub_version = snapshot.hub_version;
    live.platform = snapshot.platform;
    write_locked(paths, &live)?;
    Ok(kept)
}

/// RFC 3339 in UTC, for `installed_at` / `updated_at`.
///
/// UTC rather than local time, and RFC 3339 rather than anything friendlier:
/// the value is written on one machine and read on the same one much later,
/// possibly across a timezone change, and it sorts lexicographically in this
/// form.
pub fn now_timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        // The only failure mode is a format description that cannot represent
        // the value, which is impossible for a well-known constant one.
        .unwrap_or_else(|_| String::new())
}

#[derive(Debug)]
pub enum RegistryError {
    Io { path: PathBuf, source: io::Error },
    Malformed { path: PathBuf, detail: String },
    Unserialisable { detail: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Malformed { path, detail } => write!(
                formatter,
                "{} is not a readable registry: {detail}. Nothing was changed.",
                path.display()
            ),
            Self::Unserialisable { detail } => {
                write!(formatter, "cannot serialise the registry: {detail}")
            }
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
