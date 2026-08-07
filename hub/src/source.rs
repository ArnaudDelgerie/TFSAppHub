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
//! The resolver half — [`resolve`], which turns a `<source>` argument into a
//! local directory the installer can copy — lands here too (plan 006), and it
//! is the *only* half that may fetch anything. [`current_revision`] never does:
//! `list` must stay a read of what is already on this machine.

// The observing half has `list` as its caller; the resolving half waits for the
// installer's own pipeline, later in this same plan. Removed then.
#![allow(dead_code)]

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::registry::{Source, SourceKind};

/// Top-level directories left out of a local source's content hash.
///
/// Dependency trees and build output, none of it the developer's source: they
/// would make the hash both enormous and noisy — `var/cache` alone changes on
/// every request the app serves, which would report "changed since install"
/// forever. `tfsapp_build/` is the same story one host over: it holds the
/// station's `make build` AppImage, ~170 MB of output that changes on every
/// build, and hashing it would report "changed since install" for a build that
/// touched no source at all. Top-level only, so a legitimately named `src/var/`
/// still counts.
const EXCLUDED_FROM_HASH: &[&str] = &[".git", "vendor", "var", "node_modules", "tfsapp_build"];

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

/// What a `<source>` argument names, decided from the string alone.
///
/// An enum from day one, with the git variant left **unimplemented rather than
/// unanticipated**: the git sources plan then reduces to "fetch a remote into a
/// local directory" in front of a pipeline that already works, and neither the
/// registry's shape nor the installer's steps have to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    LocalPath(PathBuf),
    Git(String),
}

/// Which kind of source `spec` is, without touching the filesystem.
///
/// The test is on the *string*, not on what exists on disk: a URL that happens
/// to name no directory must still be reported as a git source someone cannot
/// use yet, never as a missing local path — the two errors send a reader in
/// opposite directions. Anything that is not recognisably a remote is a local
/// path, so a plain relative directory needs no scheme and no flag.
pub fn classify(spec: &str) -> Origin {
    let remote = spec.contains("://") || spec.starts_with("git@") || spec.ends_with(".git");
    match remote {
        true => Origin::Git(spec.to_string()),
        false => Origin::LocalPath(PathBuf::from(spec)),
    }
}

/// A source turned into a directory on this machine, plus what the registry
/// has to record about where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The directory holding the app's tree, ready to be copied. For a local
    /// path this *is* the source; for a git source it will be a checkout.
    pub root: PathBuf,
    /// What `update` needs to resolve this same source again.
    pub source: Source,
    /// The tree as it stood at this moment — the value `list` later compares
    /// against to say "changed since install".
    pub revision: String,
}

/// Turn `origin` into a directory, or say why it cannot be one.
///
/// The local variant resolves to an **absolute** path, deliberately: the
/// recorded `location` is re-read much later, by a `list` or an `update` run
/// from some other working directory, and a relative path recorded from today's
/// cwd would silently point at nothing.
pub fn resolve(origin: &Origin, reference: Option<&str>) -> Result<Resolved, SourceError> {
    match origin {
        Origin::LocalPath(path) => {
            if let Some(reference) = reference {
                return Err(SourceError::ReferenceOnLocalPath {
                    reference: reference.to_string(),
                });
            }

            let root = fs::canonicalize(path).map_err(|source| match source.kind() {
                io::ErrorKind::NotFound => SourceError::Missing { path: path.clone() },
                _ => SourceError::Unreadable {
                    path: path.clone(),
                    source,
                },
            })?;
            if !root.is_dir() {
                return Err(SourceError::NotADirectory { path: root });
            }

            let revision = tree_hash(&root).map_err(|source| SourceError::Unreadable {
                path: root.clone(),
                source,
            })?;

            Ok(Resolved {
                source: Source {
                    kind: SourceKind::LocalPath,
                    location: root.display().to_string(),
                    // A plain directory has no selector and nothing that
                    // selected it: recording either would be inventing a
                    // provenance nobody asked for.
                    reference: None,
                    reference_kind: None,
                },
                root,
                revision,
            })
        }
        Origin::Git(url) => Err(SourceError::GitNotImplemented { url: url.clone() }),
    }
}

/// Why a source could not be turned into a directory.
#[derive(Debug)]
pub enum SourceError {
    Missing { path: PathBuf },
    NotADirectory { path: PathBuf },
    Unreadable { path: PathBuf, source: io::Error },
    ReferenceOnLocalPath { reference: String },
    GitNotImplemented { url: String },
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => write!(formatter, "no such directory: {}", path.display()),
            Self::NotADirectory { path } => write!(
                formatter,
                "{} is not a directory — a local source is the project root, the \
                 directory holding tfsapp.config.json",
                path.display()
            ),
            Self::Unreadable { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::ReferenceOnLocalPath { reference } => write!(
                formatter,
                "--ref {reference} selects a revision of a git source; a local \
                 directory is installed as it stands"
            ),
            Self::GitNotImplemented { url } => write!(
                formatter,
                "{url} is a git source, and the hub cannot resolve one yet. \
                 Clone it yourself and install the clone's directory."
            ),
        }
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            _ => None,
        }
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
