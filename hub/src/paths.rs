//! Where the hub keeps things.
//!
//! Two roots, deliberately siblings rather than nested:
//!
//! ```text
//! <OS data dir>/TFSApp/hub/apps/<id>/     the installed snapshot — what actually runs
//! <OS data dir>/TFSApp/hub/registry.json  what is installed, from where, at what version
//! <OS data dir>/TFSApp/<identifier>/      the app's own data dir, unchanged from the station
//! ```
//!
//! The last line is the load-bearing one. It derives from `identifier` alone —
//! not from the hub, not from `id`, not from where the binary lives — which is
//! the contract-defined app-data layout. The hub must never invent a separate
//! layout for installed *app data*; its own root holds the installed source and
//! the registry, and nothing else.
//!
//! Hence the two distinct keys per app, which this module keeps apart on
//! purpose: the hub-local `id` is a CLI handle and names a directory under the
//! hub's root; the app's `identifier` comes from its own `tfsapp.config.json`
//! and names the data dir, the keyring namespace and the window identity.
//!
//! Nothing here is global state: every path hangs off a [`Paths`] whose base is
//! resolved once. Tests build one on a temp dir; the hub builds one from
//! [`Paths::resolve`].

use std::{fmt, fs, io, path::PathBuf};

/// The vendor folder under the OS data dir that groups every TFSApp's data
/// directory (CONTRACT.md §6).
pub const DATA_DIR_VENDOR: &str = "TFSApp";

/// The hub's own root, a sibling of every app data dir rather than a parent of
/// them: `<OS data dir>/TFSApp/hub/`. A packaged app knows nothing of this
/// directory, and must not have to.
pub const HUB_DIR: &str = "hub";

/// Where installed snapshots live, one directory per `id`.
pub const APPS_DIR: &str = "apps";

/// The hub's own executables, as opposed to any app's.
pub const BIN_DIR: &str = "bin";

/// The `php` shim's name — the name is the point, since it exists to be found
/// by anything looking for a PHP interpreter (see `php.rs`).
pub const PHP_SHIM_FILE: &str = "php";

/// The hub's own stable executable name, beside the `php` shim.
///
/// Its existence is what keeps a generated `.desktop` entry's `Exec=` valid
/// after the user tidies `~/Downloads`: the entry never names the AppImage or
/// binary the hub happened to run from, only this copy (`hub_bin.rs`). Hub
/// self-update replaces this same file, which is the other half of the reason
/// it lives under the hub's own root rather than next to a per-app path.
pub const HUB_EXECUTABLE_FILE: &str = "tfsapp-hub";

/// The XDG applications directory's name, under the OS data dir.
pub const APPLICATIONS_DIR: &str = "applications";

/// Names that identify directories the hub or the desktop environment owns.
///
/// These values are valid path segments, but never valid app identifiers:
/// using one would make an app's data directory overlap infrastructure the
/// hub must not install into or purge.
pub const RESERVED_IDENTIFIERS: [&str; 3] = [HUB_DIR, DATA_DIR_VENDOR, APPLICATIONS_DIR];

/// Whether `identifier` would collide with a directory owned by the hub or
/// the desktop environment.
///
/// This is deliberately an exact, case-sensitive comparison. It is a value
/// rule for the callers that create or remove app data, distinct from
/// [`safe_segment`]'s path-shape validation.
pub fn is_reserved_identifier(identifier: &str) -> bool {
    RESERVED_IDENTIFIERS.contains(&identifier)
}

/// A generated `.desktop` entry's extension.
pub const DESKTOP_ENTRY_EXTENSION: &str = "desktop";

/// The registry file's name under the hub root.
pub const REGISTRY_FILE: &str = "registry.json";

/// Where a remote source's archive is downloaded, verified and extracted
/// before anything reaches `apps/` (`../plan/018-remote-sources-releases.md`'s
/// step 4). Under the hub's own root and not `/tmp`, so a multi-hundred-
/// megabyte archive lands on the same filesystem `apps/<id>/` is about to
/// receive it.
pub const SCRATCH_DIR: &str = "scratch";

/// The registry's lock file, kept beside it rather than being the file itself
/// so the lock's lifetime never depends on the registry being rewritten — the
/// same separation `core`'s `process::lock_path` makes for the sidecar pid
/// file, and for the same reason: an atomic write replaces the target inode,
/// which would drop an flock taken on it.
pub const REGISTRY_LOCK_FILE: &str = "registry.lock";

/// The update-check feed cache's file name, under the hub root — beside the
/// registry, not under any app's `data/cache` (`app_env` empties that
/// directory on every launch, and `update.rs` empties it again), since this
/// file is exactly what has to survive a launch (`../plan/021-the-update-
/// check-an-app-can-read.md`).
pub const UPDATE_CACHE_FILE: &str = "update_cache.json";

/// The update cache's lock file, kept beside it for the same reason
/// [`REGISTRY_LOCK_FILE`] is kept beside the registry: an atomic write
/// replaces the target inode, which would drop an flock taken on the file
/// itself.
pub const UPDATE_CACHE_LOCK_FILE: &str = "update_cache.lock";

/// The per-installed-app lifecycle gates. This belongs to the hub rather than
/// an app data directory because `install` needs it before that directory
/// exists and `purge` must retain it while removing the directory.
pub const LIFECYCLE_LOCKS_DIR: &str = "locks";

/// Every path the hub resolves, hanging off one base — the OS data dir.
///
/// The base is a field rather than a call so the whole module is testable
/// against a temp dir. Real callers use [`Paths::resolve`], which reads the
/// same OS data-directory base specified by CONTRACT.md.
pub struct Paths {
    data_dir_base: PathBuf,
}

impl Paths {
    /// Resolve from the OS data dir, without an `AppHandle`: the hub needs
    /// paths in `main()`, before `tauri::Builder` — and therefore GTK —
    /// exists.
    pub fn resolve() -> Result<Self, PathsError> {
        Ok(Self {
            data_dir_base: dirs::data_dir().ok_or(PathsError::NoDataDir)?,
        })
    }

    /// Build on an arbitrary base. For tests, and for nothing else.
    #[cfg(test)]
    pub fn rooted_at(data_dir_base: impl Into<PathBuf>) -> Self {
        Self {
            data_dir_base: data_dir_base.into(),
        }
    }

    /// `<OS data dir>/TFSApp/` — shared with every packaged app, so the hub
    /// creates it if needed but never claims it, cleans it, or chmods it.
    pub fn vendor_dir(&self) -> PathBuf {
        self.data_dir_base.join(DATA_DIR_VENDOR)
    }

    /// `<OS data dir>/TFSApp/hub/`.
    pub fn hub_root(&self) -> PathBuf {
        self.vendor_dir().join(HUB_DIR)
    }

    /// `<OS data dir>/TFSApp/hub/apps/`.
    pub fn apps_root(&self) -> PathBuf {
        self.hub_root().join(APPS_DIR)
    }

    /// `<OS data dir>/TFSApp/hub/apps/<id>/` — the installed snapshot, the
    /// stable real path the app runs from.
    ///
    /// Stable and real is not incidental: it is what removes the cause of the
    /// station's wipe-cache-per-launch, where a random `/tmp/.mount_*` FUSE
    /// path baked itself into the compiled Symfony container. This function
    /// only makes a warm cache possible; plan 024 (`app_env::resolve`'s
    /// `Mode::Launch`, and the stamp in `lifecycle.rs`) is what claims it.
    pub fn app_dir(&self, id: &str) -> Result<PathBuf, PathsError> {
        Ok(self.apps_root().join(safe_segment("app id", id)?))
    }

    /// `<OS data dir>/TFSApp/hub/bin/php` — the shim that keeps an app's own
    /// PHP subprocesses on the bundled interpreter (see `php.rs`).
    ///
    /// Under the hub's root rather than anywhere on the user's `PATH`: it is
    /// the hub's plumbing, exported only to the commands the hub itself starts,
    /// and a `php` appearing in someone's shell because they installed an app
    /// would be an unpleasant surprise.
    pub fn php_shim_path(&self) -> PathBuf {
        self.hub_root().join(BIN_DIR).join(PHP_SHIM_FILE)
    }

    /// `<OS data dir>/TFSApp/hub/bin/tfsapp-hub` — a stable, runnable copy of
    /// the hub, kept current by `hub_bin::ensure_current` and named by every
    /// generated `.desktop` entry's `Exec=`.
    ///
    /// Beside [`Paths::php_shim_path`] under the hub's own `bin/`, for the
    /// same reason that shim lives there: it is the hub's plumbing, not an
    /// app's, and it must outlive whatever download directory the running
    /// image came from.
    pub fn hub_executable_path(&self) -> PathBuf {
        self.hub_root().join(BIN_DIR).join(HUB_EXECUTABLE_FILE)
    }

    /// `<OS data dir>/applications/` — the XDG directory a `.desktop` entry
    /// has to live in to be found by the shell's grid and search.
    ///
    /// A **sibling** of `TFSApp/`, like [`Paths::webkit_data_dir`] and for the
    /// same class of reason: it belongs to the desktop environment, not to
    /// this project. Pure, like that function — the hub creates it on write
    /// and never cleans, chmods or enumerates it.
    pub fn applications_dir(&self) -> PathBuf {
        self.data_dir_base.join(APPLICATIONS_DIR)
    }

    /// `<applications dir>/<identifier>.desktop`.
    ///
    /// Named after `identifier` rather than the hub-local `id`: it is the
    /// filename GNOME matches first against `_GTK_APPLICATION_ID`
    /// (`.project/plan/003-runtime-identity.md` step 5), and a
    /// `tfsapp-`-prefixed name would miss that key entirely.
    pub fn desktop_entry_path(&self, identifier: &str) -> Result<PathBuf, PathsError> {
        Ok(self.applications_dir().join(format!(
            "{}.{DESKTOP_ENTRY_EXTENSION}",
            safe_segment("app identifier", identifier)?
        )))
    }

    /// `<OS data dir>/TFSApp/hub/registry.json`.
    pub fn registry_path(&self) -> PathBuf {
        self.hub_root().join(REGISTRY_FILE)
    }

    /// `<OS data dir>/TFSApp/hub/registry.lock`.
    pub fn registry_lock_path(&self) -> PathBuf {
        self.hub_root().join(REGISTRY_LOCK_FILE)
    }

    /// `<OS data dir>/TFSApp/hub/update_cache.json` — every repository's
    /// resolved release feed, one entry per `owner/repo`, shared by every app
    /// installed from that repository.
    pub fn update_cache_path(&self) -> PathBuf {
        self.hub_root().join(UPDATE_CACHE_FILE)
    }

    /// `<OS data dir>/TFSApp/hub/update_cache.lock`.
    pub fn update_cache_lock_path(&self) -> PathBuf {
        self.hub_root().join(UPDATE_CACHE_LOCK_FILE)
    }

    /// `<OS data dir>/TFSApp/hub/locks/<identifier>.lifecycle.lock`.
    pub fn lifecycle_gate_path(&self, identifier: &str) -> Result<PathBuf, PathsError> {
        Ok(self.hub_root().join(LIFECYCLE_LOCKS_DIR).join(format!(
            "{}.lifecycle.lock",
            safe_segment("app identifier", identifier)?
        )))
    }

    /// `<OS data dir>/TFSApp/hub/scratch/<pid>/` — this process's own
    /// scratch space for one `install` or `update` of a remote source.
    ///
    /// Keyed by this process's pid rather than the `id` being installed:
    /// resolving a remote source runs *before* an `id` is even chosen — the
    /// manifest that would name one lives inside the archive being fetched —
    /// so a pid is the uniqueness two concurrently running `install`/`update`
    /// invocations already have for free, at no extra cost. The caller
    /// creates it if a remote fetch needs it and removes it when the command
    /// ends, on success or on failure; nothing here creates it, matching
    /// every other path-only method above.
    pub fn scratch_dir(&self) -> PathBuf {
        self.hub_root()
            .join(SCRATCH_DIR)
            .join(std::process::id().to_string())
    }

    /// `<OS data dir>/TFSApp/<identifier>/` — the app's own data dir, byte for
    /// byte the path the station's `packaged_data_dir` resolves.
    ///
    /// Pure: it neither creates nor chmods, so callers that only need to *name*
    /// the directory (a `list` line, a removal plan) cause no side effect on an
    /// app that is not being launched. [`Paths::create_app_data_dir`] is the
    /// half that touches the disk.
    pub fn app_data_dir(&self, identifier: &str) -> Result<PathBuf, PathsError> {
        Ok(self
            .vendor_dir()
            .join(safe_segment("app identifier", identifier)?))
    }

    /// `<OS data dir>/<identifier>/` — WebKitGTK's own per-`identifier` website
    /// data (CONTRACT.md §5/§6).
    ///
    /// A **sibling** of `TFSApp/`, not a child of it: WebKit derives it from
    /// the GTK application id, which plan 003 sets to the app's `identifier`,
    /// and it has never heard of this project's vendor folder. Named here so
    /// that the one command which has to clean it up — `remove --purge` — reads
    /// it from the same place as every other path instead of rebuilding it, and
    /// so nobody later "fixes" it into the vendor dir.
    pub fn webkit_data_dir(&self, identifier: &str) -> Result<PathBuf, PathsError> {
        Ok(self
            .data_dir_base
            .join(safe_segment("app identifier", identifier)?))
    }

    /// [`Paths::app_data_dir`], created if missing and `0700` on every call.
    ///
    /// Both halves match the station's `packaged_data_dir` deliberately. The
    /// permissions are re-applied rather than only set at creation because the
    /// directory holds the app's SQLite DB and its generated `APP_SECRET`
    /// (CONTRACT.md §6): an installation created by an older host, or
    /// recreated by hand, gets tightened on its next use instead of staying
    /// lax forever. Only the `<identifier>` directory is touched — the
    /// `TFSApp/` vendor dir above it is shared across apps and left alone.
    pub fn create_app_data_dir(&self, identifier: &str) -> Result<PathBuf, PathsError> {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = self.app_data_dir(identifier)?;
        fs::create_dir_all(&data_dir).map_err(|source| PathsError::Io {
            path: data_dir.clone(),
            source,
        })?;
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
            PathsError::Io {
                path: data_dir.clone(),
                source,
            }
        })?;
        Ok(data_dir)
    }
}

/// Accept `value` as a single path component, or say why not.
///
/// Deliberately minimal — empty, `.`, `..`, and anything carrying a separator
/// or a NUL. It rejects exactly the values that would let a manifest, or a
/// mistyped CLI argument, name a directory outside the root it was meant to
/// stay in, and nothing else.
///
/// Being no stricter than that is a contract rule, not taste: `identifier` is
/// the app's to choose, while the hub generates its own local `id`. A tighter
/// charset for `id` must never spread to `identifier`.
fn safe_segment<'a>(kind: &'static str, value: &'a str) -> Result<&'a str, PathsError> {
    let unsafe_segment = value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0');

    if unsafe_segment {
        return Err(PathsError::UnsafeSegment {
            kind,
            value: value.to_string(),
        });
    }
    Ok(value)
}

#[derive(Debug)]
pub enum PathsError {
    /// `dirs::data_dir()` came back empty — no `$HOME`, no `XDG_DATA_HOME`.
    NoDataDir,
    /// A value that was going to name a directory cannot be one component.
    UnsafeSegment { kind: &'static str, value: String },
    /// Creating or tightening a directory failed; carries which one.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for PathsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDir => write!(
                formatter,
                "cannot resolve the OS data directory — is HOME set?"
            ),
            Self::UnsafeSegment { kind, value } => write!(
                formatter,
                "invalid {kind} {value:?}: it names a directory, so it cannot be empty, \
                 \".\", \"..\", or contain a path separator"
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for PathsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
