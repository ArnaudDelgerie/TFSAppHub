//! Hardened extraction of a verified source archive into scratch space
//! (`../plan/018-remote-sources-releases.md`'s step 3, the third leg after
//! `release::download_to` and `release::verify`).
//!
//! Generic tar handling, deliberately: by the time [`extract`] runs, the
//! bytes have already been checked against `SHA256SUMS.txt`, so nothing here
//! is GitHub-specific and nothing here re-derives that trust. What this
//! module adds is the trust an archive's own bytes cannot buy — a checksum
//! only proves the archive is the one the author published, not that the
//! author's tar tool (or a compromised release pipeline) wrote entries a
//! naive extractor would happily write outside the tree it was asked for.
//!
//! **Every entry is checked before it is written.** A path that is absolute,
//! that contains a `..` component, or a symlink/hard link whose target
//! resolves outside the extraction root, is a refusal — not a skip, not a
//! sanitised rewrite. The Overview's "the archive is a `.tar.gz` with exactly
//! one top-level directory" is enforced the same way: every entry's first
//! path component must name the same directory, and an archive with none (a
//! tarbomb of loose files) or more than one is rejected too.
//!
//! **Partial writes on a rejected archive are not a bug.** [`extract`] does
//! not undo what it already wrote before the entry that failed — the caller's
//! scratch directory is removed wholesale on any failure by whoever owns it
//! (`install`/`update`, this plan's step 4), which is the one place that
//! guarantee needs to live.

use std::{
    ffi::OsString,
    fmt, fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

/// Extract the `.tar.gz` at `archive_path` into a fresh directory under
/// `destination`, and return the path to its single top-level directory.
///
/// `destination` is created if missing; it must not already hold an
/// extraction, since nothing here merges with existing content.
pub fn extract(archive_path: &Path, destination: &Path) -> Result<PathBuf, ArchiveError> {
    fs::create_dir_all(destination).map_err(ArchiveError::Io)?;

    let file = fs::File::open(archive_path).map_err(ArchiveError::Io)?;
    let mut archive = Archive::new(GzDecoder::new(file));

    let mut top_level: Option<OsString> = None;
    let mut saw_nested_entry = false;

    for entry in archive.entries().map_err(ArchiveError::Io)? {
        let mut entry = entry.map_err(ArchiveError::Io)?;

        // `git archive` always writes a PAX global extended header (the
        // commit's hash, as a comment) ahead of the real entries — the most
        // ordinary way to build this plan's archive "by hand". The `tar`
        // crate applies a per-entry (local) pax header to the entry that
        // follows it transparently, but surfaces the *global* one as a
        // literal entry of its own, with no path of its own to check. Both
        // kinds are archive-format bookkeeping, never tree content, so
        // neither counts toward "exactly one top-level directory".
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() || entry_type.is_pax_local_extensions() {
            continue;
        }

        let path = entry.path().map_err(ArchiveError::Io)?.into_owned();

        let first = check_safe_path(&path)?;
        match &top_level {
            None => top_level = Some(first.to_os_string()),
            Some(existing) if existing.as_os_str() != first => {
                return Err(ArchiveError::MultipleTopLevelDirectories {
                    first: existing.to_string_lossy().into_owned(),
                    second: first.to_string_lossy().into_owned(),
                })
            }
            _ => {}
        }
        if path.components().count() > 1 {
            saw_nested_entry = true;
        }

        if entry_type.is_symlink() || entry_type.is_hard_link() {
            check_safe_link(&mut entry, &path, entry_type)?;
        }

        let out_path = destination.join(&path);
        // `Entry::unpack(dst)` writes exactly to `dst` and does not create
        // its parents — unlike `unpack_in`, which this function does not use
        // because it re-derives the entry's path from `dst` instead of
        // trusting the one already validated above. A well-formed archive
        // lists a directory before its contents, but nothing here should
        // depend on that ordering holding.
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(ArchiveError::Io)?;
        }
        entry.unpack(&out_path).map_err(ArchiveError::Io)?;
    }

    match top_level {
        Some(name) if saw_nested_entry => Ok(destination.join(name)),
        _ => Err(ArchiveError::NoTopLevelDirectory),
    }
}

/// Reject an absolute path or one carrying a `..` component, and return its
/// first (top-level) component on success — every entry's path is checked
/// this way before anything about it is trusted further.
fn check_safe_path(path: &Path) -> Result<&std::ffi::OsStr, ArchiveError> {
    let mut components = path.components();
    let first = match components.next() {
        Some(Component::Normal(first)) => first,
        _ => {
            return Err(ArchiveError::UnsafePath {
                path: path.to_path_buf(),
            })
        }
    };
    if components.any(|component| !matches!(component, Component::Normal(_))) {
        return Err(ArchiveError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    Ok(first)
}

/// Reject a symlink or hard link whose recorded target, resolved lexically
/// against `path`'s own directory, would leave the extraction root — an
/// absolute target is rejected outright, since nothing about the archive's
/// own layout can vouch for what lives at an absolute path on the machine
/// doing the extracting.
fn check_safe_link<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &Path,
    entry_type: EntryType,
) -> Result<(), ArchiveError> {
    let target = entry.link_name().map_err(ArchiveError::Io)?;
    let escapes = match &target {
        Some(target) => link_target_escapes(path, target),
        None => true,
    };
    if escapes {
        return Err(ArchiveError::UnsafeLink {
            path: path.to_path_buf(),
            kind: if entry_type.is_symlink() {
                "symlink"
            } else {
                "hard link"
            },
        });
    }
    Ok(())
}

/// Whether a link recorded at `path` (its own location within the tree),
/// pointing at `target`, resolves — lexically, with no filesystem access —
/// outside the tree `path` is rooted in. An absolute target always escapes,
/// since nothing about the tree's own layout can vouch for what lives at an
/// absolute path on the machine doing the extracting.
///
/// `pub(crate)` so `publish.rs`'s archive builder applies the exact same rule
/// before writing a symlink entry that `archive::extract` applies before
/// trusting one: an archive `publish` would build and this module would then
/// refuse to extract is the one bug `../plan/019-publish-an-app.md` names as
/// unshippable.
pub(crate) fn link_target_escapes(path: &Path, target: &Path) -> bool {
    let mut depth: i64 = path.components().count() as i64 - 1;
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

#[derive(Debug)]
pub enum ArchiveError {
    /// Reading the archive, or writing an entry to disk, failed.
    Io(io::Error),
    /// An entry's path is absolute or contains a `..` component.
    UnsafePath { path: PathBuf },
    /// A symlink or hard link whose target resolves outside the extraction
    /// root.
    UnsafeLink { path: PathBuf, kind: &'static str },
    /// The archive has no top-level directory: it is empty, or every entry
    /// sits directly at the root with nothing nested under it.
    NoTopLevelDirectory,
    /// Entries disagree on the archive's top-level directory name.
    MultipleTopLevelDirectories { first: String, second: String },
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "cannot extract archive: {source}"),
            Self::UnsafePath { path } => write!(
                formatter,
                "refusing to extract {}: absolute paths and \"..\" are not allowed in a \
                 source archive",
                path.display()
            ),
            Self::UnsafeLink { path, kind } => write!(
                formatter,
                "refusing to extract {}: this {kind} points outside the archive",
                path.display()
            ),
            Self::NoTopLevelDirectory => write!(
                formatter,
                "this archive has no single top-level directory — a source archive must hold \
                 exactly one"
            ),
            Self::MultipleTopLevelDirectories { first, second } => write!(
                formatter,
                "this archive has more than one top-level directory ({first} and {second}) — a \
                 source archive must hold exactly one"
            ),
        }
    }
}

impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
