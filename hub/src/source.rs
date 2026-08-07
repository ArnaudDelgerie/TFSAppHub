//! What a source looks like *right now*, as opposed to when it was installed.
//!
//! The registry records a `source_revision` at install time — the git sha for a
//! git source, a content hash of the tree for a plain directory (design source:
//! `../TFSAppWorkstation/.project/hub/004-app-sources-and-versioning.md` §4).
//! This module computes the other half of that comparison, so `list` can say
//! *"source changed since install"* for the very common case of a developer who
//! edited their project and forgot to bump `app_version`.
//!
//! One rule governs the whole module: **an answer it cannot give is silence,
//! never a guess.** A source directory that has been moved, renamed or unplugged
//! is [`Revision::Unreachable`], and a caller must then say nothing rather than
//! report a difference it did not measure — a false "changed" would send a
//! developer looking for an edit they never made.
//!
//! The resolver half — `resolve(source) -> PathBuf`, which fetches a git source
//! into a local directory — belongs to the installer (plan 006) and to the git
//! sources plan after it. This module deliberately stops at *observing* a
//! source, so `list` never fetches anything.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::registry::{Source, SourceKind};

/// Top-level directories left out of a local source's content hash.
///
/// Dependency trees and build output, none of it the developer's source: they
/// would make the hash both enormous and noisy — `var/cache` alone changes on
/// every request the app serves, which would report "changed since install"
/// forever. Top-level only, so a legitimately named `src/var/` still counts.
const EXCLUDED_FROM_HASH: &[&str] = &[".git", "vendor", "var", "node_modules"];

/// Where a source stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    /// The source was read, and this is what it hashes to.
    At(String),
    /// The source could not be read: moved, renamed, offline, or a kind
    /// nothing resolves yet. Callers stay silent on it.
    Unreachable,
}

/// Observe `source` without fetching or changing anything.
pub fn current_revision(source: &Source) -> Revision {
    match source.kind {
        SourceKind::LocalPath => match tree_hash(Path::new(&source.location)) {
            Ok(hash) => Revision::At(hash),
            // Every failure lands here on purpose — a missing directory and an
            // unreadable one are the same answer to the only question asked:
            // can this source be compared against what was installed?
            Err(_) => Revision::Unreachable,
        },
        // No git source can exist yet (the installer resolves local paths
        // only), and asking a remote for its head would put a network call
        // inside `list`. The git sources plan gives this arm a real answer.
        SourceKind::Git => Revision::Unreachable,
    }
}

/// Hash a directory tree into a stable, comparable string.
///
/// The definition, since it is the value's whole meaning: every file under
/// `root` outside [`EXCLUDED_FROM_HASH`], in sorted path order, contributing
/// its relative path, whether it is executable, and its bytes. Sorted rather
/// than in directory order because readdir order is a filesystem detail, and
/// the same tree copied elsewhere must hash the same.
///
/// A symlink contributes its target, not the target's content: what changed
/// when a link is repointed is the source tree, and following it would let a
/// link out of the tree pull unrelated bytes into the hash.
pub fn tree_hash(root: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    hash_directory(root, Path::new(""), 0, &mut hasher)?;
    Ok(format!(
        "sha256:{}",
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn hash_directory(
    directory: &Path,
    relative: &Path,
    depth: usize,
    hasher: &mut Sha256,
) -> io::Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();

    for entry in entries {
        let Some(name) = entry.file_name() else {
            continue;
        };
        if depth == 0 && EXCLUDED_FROM_HASH.iter().any(|excluded| name == *excluded) {
            continue;
        }

        let relative = relative.join(name);
        let metadata = fs::symlink_metadata(&entry)?;

        // The path goes in before anything else, so that moving a file's
        // content to another name changes the hash even when the bytes are
        // identical.
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");

        if metadata.is_symlink() {
            hasher.update(b"symlink\0");
            hasher.update(fs::read_link(&entry)?.as_os_str().as_encoded_bytes());
        } else if metadata.is_dir() {
            hasher.update(b"dir\0");
            hash_directory(&entry, &relative, depth + 1, hasher)?;
        } else {
            use std::os::unix::fs::PermissionsExt;

            // The executable bit is part of the source: a hook or a binary in
            // `bin/` that lost it is a changed tree, and the bytes alone would
            // not say so.
            let executable = metadata.permissions().mode() & 0o111 != 0;
            hasher.update(match executable {
                true => b"file+x\0".as_slice(),
                false => b"file\0".as_slice(),
            });
            // Length-prefixed, because file content is the one thing here that
            // can contain the NUL the other fields are separated by.
            let content = fs::read(&entry)?;
            hasher.update((content.len() as u64).to_le_bytes());
            hasher.update(content);
        }
        hasher.update(b"\0");
    }

    Ok(())
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
