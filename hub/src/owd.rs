//! The directory the user invoked the hub from.
//!
//! The AppImage runtime `cd`s into its own mount before the hub's `main`
//! runs, so the process's working directory is inside the AppImage rather
//! than the directory the command was typed in — a relative path read
//! against the process's cwd resolves into the mount, where nothing the user
//! named exists. `AppRun` exports `OWD` with the invoking directory before
//! changing it, so that is where a user's relative path starts. A dev build
//! has no `OWD` and its cwd is the user's own, so falling back to the
//! process's cwd keeps the same behaviour outside an AppImage.
//!
//! Every user-typed path resolves through [`resolve_argument`] — `--update
//! --from` included, which read `OWD` on its own before this module existed:
//! one rule, stated once here instead of a per-caller copy that could drift.

use std::{
    env,
    path::{Path, PathBuf},
};

/// Anchor a user-typed path to the invoking directory. Absolute paths pass
/// through untouched, so a location recorded by an earlier install and a
/// child argument the parent already made absolute are both immune.
pub fn resolve_argument(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match invoking_dir() {
        Some(dir) => dir.join(path),
        None => path.to_path_buf(),
    }
}

/// The directory the command was typed in: `OWD` when the AppImage runtime
/// set it, the process's own cwd otherwise.
fn invoking_dir() -> Option<PathBuf> {
    env::var_os("OWD")
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
}

/// Point `invoking_dir()` at `path` for the duration of this guard,
/// restoring the previous value (or its absence) on drop. `make check`'s
/// `--test-threads=1` is what makes borrowing a process-global variable
/// safe here: nothing else in the test binary runs concurrently, and the
/// fixture owns the whole window.
#[cfg(test)]
pub(crate) struct RedirectedOwd {
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl RedirectedOwd {
    pub(crate) fn point_at(path: &Path) -> Self {
        let previous = env::var_os("OWD");
        env::set_var("OWD", path);
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for RedirectedOwd {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => env::set_var("OWD", previous),
            None => env::remove_var("OWD"),
        }
    }
}

/// The absence of `OWD`, for the duration of this guard — a dev build's own
/// case, and also this test binary's whenever the ambient environment does
/// not carry it.
#[cfg(test)]
struct RemovedOwd {
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl RemovedOwd {
    fn remove() -> Self {
        let previous = env::var_os("OWD");
        env::remove_var("OWD");
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for RemovedOwd {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => env::set_var("OWD", previous),
            None => env::remove_var("OWD"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_path_is_untouched_whatever_owd_says() {
        let _guard = RedirectedOwd::point_at(Path::new("/typed/in/here"));
        assert_eq!(
            resolve_argument(Path::new("/absolutely/elsewhere/app")),
            PathBuf::from("/absolutely/elsewhere/app")
        );
    }

    #[test]
    fn a_relative_path_is_anchored_at_owd_when_the_appimage_set_it() {
        let _guard = RedirectedOwd::point_at(Path::new("/typed/in/here"));
        assert_eq!(
            resolve_argument(Path::new("myapp")),
            PathBuf::from("/typed/in/here/myapp")
        );
        // `.` itself — the `dev .` form — names the invoking directory.
        assert_eq!(
            resolve_argument(Path::new(".")),
            PathBuf::from("/typed/in/here")
        );
    }

    #[test]
    fn without_owd_the_process_cwd_is_the_base() {
        let _guard = RemovedOwd::remove();
        let cwd = env::current_dir().expect("a working directory");
        assert_eq!(resolve_argument(Path::new("myapp")), cwd.join("myapp"));
    }
}
