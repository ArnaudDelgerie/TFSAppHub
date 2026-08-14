//! Keeping a stable, runnable copy of the hub at `Paths::hub_executable_path()`.
//!
//! A generated `.desktop` entry's `Exec=` must never name
//! `~/Downloads/tfsapp-hub-0.3.0.AppImage` — a launcher that breaks when the
//! user tidies their downloads folder is worse than no launcher. So every
//! entry this hub writes points at `<hub root>/bin/tfsapp-hub` instead, and
//! this module is what keeps that copy current.
//!
//! **The image to copy is `$APPIMAGE` when set, `current_exe()` otherwise.**
//! Inside an AppImage, `std::env::current_exe()` resolves to
//! `/tmp/.mount_XXXX/usr/bin/tfsapp-hub` — a FUSE mount that disappears the
//! moment the process exits. Copying *that* would produce a launcher pointing
//! at nothing. `$APPIMAGE` is what the AppImage runtime exports for exactly
//! this purpose; `current_exe()` is the right answer for a plain binary, where
//! no such mount exists.
//!
//! **Two races, one mechanism each.** The running image may already *be* the
//! target — someone runs the stable copy and installs a second app — in which
//! case there is nothing to copy: skipped after comparing both sides
//! `canonicalize`d. And the copy may be running *while* being refreshed — an
//! app is open from it, while a second install refreshes it for a newer hub —
//! solved the same way `php.rs`'s shim solves it: write a temp file in the
//! same directory and `rename` it over, an atomic replacement of the inode, so
//! a concurrent execution keeps running the generation it started with and
//! never meets `ETXTBSY` or a half-written 170 MB launcher.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process,
};

use crate::paths::Paths;

/// What [`ensure_current`] did, so the caller can print one honest line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No copy existed yet.
    Written,
    /// A copy existed but did not match the running image.
    Refreshed,
    /// A copy existed and already matched — nothing touched.
    Current,
    /// The running image already *is* the target; nothing to copy.
    WeAreIt,
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            Self::Written => "written",
            Self::Refreshed => "refreshed",
            Self::Current => "already current",
            Self::WeAreIt => "is the running process",
        };
        write!(formatter, "{word}")
    }
}

/// Make `paths.hub_executable_path()` a current, runnable copy of the hub
/// this process is running as.
pub fn ensure_current(paths: &Paths) -> Result<Outcome, HubBinError> {
    let source = resolve_running_image(
        std::env::var_os("APPIMAGE").map(PathBuf::from),
        std::env::current_exe,
    )?;
    ensure_current_at(&source, &paths.hub_executable_path())
}

/// `<hub root>/bin/tfsapp-hub.previous` — the rollback anchor's binary half:
/// the outgoing stable copy renamed aside before a verified download
/// replaces it (`../plan/020-hub-self-update-and-revalidation.md`'s
/// Overview, step 8), the hub's own equivalent of `lifecycle::
/// previous_tree_path` for an app. One generation, like that one: a second
/// `--update` before a `--rollback` overwrites it rather than keeping two.
pub fn anchor_path(paths: &Paths) -> PathBuf {
    let mut previous = paths.hub_executable_path().into_os_string();
    previous.push(".previous");
    PathBuf::from(previous)
}

/// `<hub root>/bin/registry.json.previous` — the rollback anchor's registry
/// half, snapshotted beside the binary (Overview step 7) under the registry
/// lock, so `--rollback` can restore the app states a hub update mutated
/// along with the binary that mutated them (correction 2).
pub fn anchor_registry_path(paths: &Paths) -> PathBuf {
    paths
        .hub_executable_path()
        .with_file_name("registry.json.previous")
}

/// The image to copy: `$APPIMAGE` if the process was started as one,
/// `current_exe()` otherwise — see the module header.
///
/// `current_exe` is a closure rather than a direct call so the choice
/// between the two is testable without touching the real process
/// environment or the real `current_exe()` — mutating either from a test is
/// either unsafe or answers a question this module does not ask.
fn resolve_running_image(
    appimage: Option<PathBuf>,
    current_exe: impl FnOnce() -> io::Result<PathBuf>,
) -> Result<PathBuf, HubBinError> {
    match appimage {
        Some(path) => Ok(path),
        None => current_exe().map_err(HubBinError::NoRunningImage),
    }
}

/// Whether `a` and `b` name the same file on disk, compared by canonical path
/// rather than by content or by string equality — a symlink or a relative
/// path must not fool it. [`ensure_current_at`]'s own "are we already the
/// target" check, `pub(crate)` so hub self-update (`hub_update.rs`) reuses it
/// for the same question about `$APPIMAGE` against the stable copy (the
/// plan's "Two files, not one") rather than a second canonicalize-and-compare.
///
/// Either side missing (not canonicalizable) answers `false` rather than
/// erroring: "not found" is never "same file".
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The pure half of [`ensure_current`]: given the image actually running and
/// the path a stable copy belongs at, make the second a current copy of the
/// first.
///
/// `pub(crate)`: hub self-update (`hub_update.rs`, this plan's step 4) calls
/// it directly to install a verified download at the stable path — the same
/// atomic copy-then-rename an ordinary install already trusts, rather than a
/// second implementation of it.
pub(crate) fn ensure_current_at(source: &Path, target: &Path) -> Result<Outcome, HubBinError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| HubBinError::Io { path, source }
    };

    // The ordinary case of installing a second app from the stable copy must
    // not try to overwrite the file it is executing.
    if same_file(source, target) {
        return Ok(Outcome::WeAreIt);
    }

    let source_meta = fs::metadata(source).map_err(io_error(source))?;
    let outcome = match fs::metadata(target) {
        Ok(target_meta) if is_current(&source_meta, &target_meta) => return Ok(Outcome::Current),
        Ok(_) => Outcome::Refreshed,
        Err(_) => Outcome::Written,
    };

    write_copy(source, &source_meta, target)?;
    Ok(outcome)
}

/// Whether `target` already carries `source`'s bytes — checked by length and
/// modification time, not by hashing 170 megabytes on every install.
fn is_current(source: &fs::Metadata, target: &fs::Metadata) -> bool {
    match (source.modified(), target.modified()) {
        (Ok(source_modified), Ok(target_modified)) => {
            source.len() == target.len() && source_modified == target_modified
        }
        // A filesystem that cannot report mtimes at all cannot answer this
        // question; refreshing is the safe default, and it is cheap.
        _ => false,
    }
}

/// Copy `source` to `target`, atomically: a temp file in `target`'s own
/// directory, `0755` before the rename, then `rename` over `target`. The
/// same-directory requirement is what keeps the rename inside one filesystem,
/// which is what makes it atomic at all.
///
/// The copy's mtime is set to `source`'s, explicitly — `fs::copy` does not
/// preserve it — so the next call's [`is_current`] check recognises this copy
/// as current instead of refreshing it again on every single install.
fn write_copy(source: &Path, source_meta: &fs::Metadata, target: &Path) -> Result<(), HubBinError> {
    use std::os::unix::fs::PermissionsExt;

    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |error| HubBinError::Io {
            path: path.clone(),
            source: error,
        }
    };

    let parent = target
        .parent()
        .expect("hub_executable_path always names a file inside a directory");
    fs::create_dir_all(parent).map_err(io_error(parent))?;

    let temporary = temporary_path(target);
    let result = (|| {
        fs::copy(source, &temporary).map_err(io_error(&temporary))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))
            .map_err(io_error(&temporary))?;
        if let Ok(modified) = source_meta.modified() {
            let file = fs::File::open(&temporary).map_err(io_error(&temporary))?;
            let _ = file.set_modified(modified);
        }
        fs::rename(&temporary, target).map_err(io_error(target))?;

        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// A temp beside `target`, retaining its full name so a dotted filename keeps
/// its extension, and separated by process so two hubs do not copy through
/// the same staging inode.
fn temporary_path(target: &Path) -> PathBuf {
    let mut temporary = target.as_os_str().to_os_string();
    temporary.push(format!(".{}.tmp", process::id()));
    PathBuf::from(temporary)
}

#[derive(Debug)]
pub enum HubBinError {
    /// Neither `$APPIMAGE` nor `current_exe()` named a running image.
    NoRunningImage(io::Error),
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for HubBinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRunningImage(source) => write!(
                formatter,
                "cannot find the hub's own running image to copy: {source}"
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for HubBinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoRunningImage(source) => Some(source),
            Self::Io { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
#[path = "hub_bin_tests.rs"]
mod tests;
