//! `actions.paths`: resolving a declared OS user directory to
//! `TFS_USER_<NAME>_DIR` (CONTRACT.md §3/§7, decision 008).
//!
//! One table, in the same order CONTRACT.md §7 spells the eight members,
//! pairing each `PathsActions` field with the `glib::UserDirectory` variant
//! that resolves it and the environment variable name it reports as.
//! [`resolve`] takes the directory lookup as a parameter rather than calling
//! `glib::user_special_dir` itself, so its own tests supply fake paths
//! instead of depending on the host account's real `~/.config/user-dirs.dirs`
//! — `app_env.rs` is what passes the real function in.
//!
//! A declared member the lookup cannot resolve contributes nothing: no empty
//! string, no fallback guess, the same shape §3 already uses for
//! `TFS_KEYRING_AVAILABLE`/`TFS_MEDIA_MICROPHONE` (CONTRACT.md §8).

use std::path::PathBuf;

use tfsapp_core::sidecar::path_to_string;

use crate::manifest::PathsActions;

struct Entry {
    declared: fn(&PathsActions) -> bool,
    directory: glib::UserDirectory,
    var_name: &'static str,
}

const ENTRIES: [Entry; 8] = [
    Entry {
        declared: |p| p.desktop,
        directory: glib::UserDirectory::Desktop,
        var_name: "TFS_USER_DESKTOP_DIR",
    },
    Entry {
        declared: |p| p.documents,
        directory: glib::UserDirectory::Documents,
        var_name: "TFS_USER_DOCUMENTS_DIR",
    },
    Entry {
        declared: |p| p.downloads,
        directory: glib::UserDirectory::Downloads,
        var_name: "TFS_USER_DOWNLOADS_DIR",
    },
    Entry {
        declared: |p| p.music,
        directory: glib::UserDirectory::Music,
        var_name: "TFS_USER_MUSIC_DIR",
    },
    Entry {
        declared: |p| p.pictures,
        directory: glib::UserDirectory::Pictures,
        var_name: "TFS_USER_PICTURES_DIR",
    },
    Entry {
        declared: |p| p.public_share,
        directory: glib::UserDirectory::PublicShare,
        var_name: "TFS_USER_PUBLIC_SHARE_DIR",
    },
    Entry {
        declared: |p| p.templates,
        directory: glib::UserDirectory::Templates,
        var_name: "TFS_USER_TEMPLATES_DIR",
    },
    Entry {
        declared: |p| p.videos,
        directory: glib::UserDirectory::Videos,
        var_name: "TFS_USER_VIDEOS_DIR",
    },
];

/// Every declared member of `paths` that `resolve_dir` can resolve, as
/// `(TFS_USER_<NAME>_DIR, path)` pairs in table order. An undeclared member is
/// skipped outright; a declared member `resolve_dir` answers `None` for is
/// skipped too, rather than reported empty.
pub fn resolve(
    paths: &PathsActions,
    resolve_dir: impl Fn(glib::UserDirectory) -> Option<PathBuf>,
) -> Vec<(&'static str, String)> {
    ENTRIES
        .iter()
        .filter(|entry| (entry.declared)(paths))
        .filter_map(|entry| {
            resolve_dir(entry.directory).map(|path| (entry.var_name, path_to_string(&path)))
        })
        .collect()
}

#[cfg(test)]
#[path = "user_dirs_tests.rs"]
mod tests;
