//! `tfsapp-hub install <source>` — the command that makes the hub a hub.
//!
//! The pipeline, in the order it runs and the order this module reads:
//!
//! 1. resolve the source into a directory (`source::resolve`),
//! 2. validate that directory is an app at all,
//! 3. settle the hub-local `id` and refuse a collision,
//! 4. copy the tree into `apps/<id>/`,
//! 5. run the app's dependency install and lifecycle hooks with the bundled PHP,
//! 6. register it — last, and only once everything above succeeded.
//!
//! Two rules shape every step.
//!
//! **Install is a snapshot, always.** The tree is *copied*; editing the original
//! source afterwards changes nothing until an explicit `update`. There is no
//! symlink into the working tree and no watcher — that is what makes `composer
//! install`, migrations and cache warm-up mean anything, since they ran against
//! *this* tree and it cannot move underneath them. Live editing is the
//! station's `make tauri-dev`, and the hub must not grow a second, worse
//! version of it.
//!
//! **Every step is restartable.** A failure leaves no half-installed app: the
//! copied directory is removed, and the registry is written only at the very
//! end. Which is also why registration is step 6 and not step 4 — an entry
//! pointing at a tree whose `composer install` failed would be a `ready` app
//! that cannot run.

// The pipeline is assembled step by step across this plan, and until [`run`]
// exists the module's own tests are its only callers. Removed as soon as the
// CLI routes `install` here, rather than left to linger.
#![allow(dead_code)]

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    manifest::{self, Loaded, Manifest, ManifestError, MANIFEST_FILE},
    paths::{Paths, PathsError},
    registry::{Registry, RegistryError},
    source::SourceError,
};

/// Paths, relative to the project root, the snapshot never copies.
///
/// Two of them are the app's own runtime droppings (`var/cache` is rewritten on
/// every request, `var/log` grows without bound) and the third is the station's
/// `make build` output — a ~170 MB AppImage that has no business being
/// installed as source. Matched as whole relative paths, so a project with its
/// own `src/var/log/` keeps it.
const EXCLUDED_PATHS: &[&str] = &["var/cache", "var/log", "tfsapp_build"];

/// Directory names the snapshot never copies, at any depth.
///
/// By name rather than by path because both are legitimately nested: a `.git`
/// below the root is a vendored repository or a submodule, and `node_modules/`
/// sits wherever a package.json does. Neither is source, and both are
/// regenerable from something that is.
const EXCLUDED_NAMES: &[&str] = &[".git", "node_modules"];

/// Files an app must carry for the hub to be able to run it at all
/// (CONTRACT.md §1), plus the `composer.json` the dependency install needs.
///
/// Checked before the copy rather than discovered during it: the alternative is
/// a hundred-megabyte copy followed by "failed to start bin/console", which
/// names the symptom and not the cause.
const REQUIRED_FILES: &[(&str, &str)] = &[
    (
        "composer.json",
        "the hub installs the app's dependencies with its own PHP, and Composer \
         needs one",
    ),
    (
        "bin/console",
        "the app's lifecycle commands are `bin/console` invocations (CONTRACT.md §1)",
    ),
    (
        "public/index.php",
        "it is the front controller the app is served from (CONTRACT.md §1)",
    ),
];

/// Settle the hub-local handle this app is installed under.
///
/// `--as` wins; otherwise the manifest's `project_name`, which is already the
/// machine-friendly slug of the three name fields. Never `identifier` (a
/// reverse-DNS string is a poor thing to type) and never `product_name` (it has
/// spaces).
///
/// The `id` names a directory under the hub's root *and* is what a user types,
/// so it is held to a tighter charset than the app's `identifier` — which the
/// hub must keep accepting exactly as the station does (see `paths.rs`).
/// Refusing rather than sanitising is deliberate: a silent transformation
/// leaves the user with an app under a name they never chose and cannot guess.
pub fn resolve_id(explicit: Option<&str>, manifest: &Manifest) -> Result<String, InstallError> {
    let (id, derived) = match explicit {
        Some(id) => (id, false),
        None => (manifest.project_name.as_str(), true),
    };

    match is_usable_id(id) {
        true => Ok(id.to_string()),
        false => Err(InstallError::UnusableId {
            id: id.to_string(),
            derived,
        }),
    }
}

/// Whether `id` can be both a CLI word and a directory name.
fn is_usable_id(id: &str) -> bool {
    let mut characters = id.chars();
    let starts_well = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    starts_well
        && id.len() <= 64
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// Refuse `id` if anything already answers to it.
///
/// Both halves are checked, not just the registry: a directory left behind by
/// an install that was interrupted before it could register is exactly the case
/// where silently copying over it would destroy something. The two get
/// different messages because they need different answers from the user.
pub fn check_id_free(registry: &Registry, paths: &Paths, id: &str) -> Result<(), InstallError> {
    if let Some(entry) = registry.get(id) {
        return Err(InstallError::IdTaken {
            id: id.to_string(),
            location: entry.source.location.clone(),
        });
    }

    let app_dir = paths.app_dir(id)?;
    if app_dir.exists() {
        return Err(InstallError::DirectoryInTheWay { path: app_dir });
    }

    Ok(())
}

/// Read `root`'s manifest and check the tree is an app the hub can install.
///
/// The `app_version` semver check is the hub's half of the station's
/// build-time validation (CONTRACT.md §2 states the rule through
/// `build-app.sh`, which does not exist here — see
/// `.project/contract-amendments.md` #5). Install is the hub's equivalent
/// moment: the value is what `update` will later compare against to choose
/// install / update / downgrade, and a value nothing can compare is a broken
/// app whose first symptom would appear months later, at the update that needed
/// it.
pub fn validate(root: &Path) -> Result<Loaded, InstallError> {
    let loaded = manifest::load(root)?;

    if let Err(error) = semver::Version::parse(&loaded.manifest.app_version) {
        return Err(InstallError::UnusableVersion {
            path: root.join(MANIFEST_FILE),
            version: loaded.manifest.app_version.clone(),
            detail: error.to_string(),
        });
    }

    for (relative, why) in REQUIRED_FILES {
        if !root.join(relative).is_file() {
            return Err(InstallError::MissingFile {
                path: root.join(relative),
                why,
            });
        }
    }

    Ok(loaded)
}

/// Copy `from` into `to`, minus what must not be installed.
///
/// `to` must not exist: an install never merges into a tree it did not write,
/// because a leftover file from a previous version — a migration, a compiled
/// container, a route — is indistinguishable from one this version meant to
/// ship. On any failure the partial copy is removed, so the caller's next
/// attempt meets a clean root rather than half of the last one.
pub fn snapshot(from: &Path, to: &Path) -> Result<(), InstallError> {
    if to.exists() {
        return Err(InstallError::DirectoryInTheWay {
            path: to.to_path_buf(),
        });
    }

    let copied = copy_tree(from, to, Path::new(""));
    if copied.is_err() {
        // Best-effort, and deliberately not reported: the caller is already
        // being told why the install failed, and "…and the cleanup failed too"
        // would bury it.
        let _ = fs::remove_dir_all(to);
    }
    copied
}

/// Whether `relative` — a path below the project root — is left out of the
/// snapshot.
fn is_excluded(relative: &Path) -> bool {
    EXCLUDED_PATHS
        .iter()
        .any(|excluded| relative == Path::new(excluded))
        || relative
            .file_name()
            .is_some_and(|name| EXCLUDED_NAMES.iter().any(|excluded| name == *excluded))
}

fn copy_tree(from: &Path, to: &Path, relative: &Path) -> Result<(), InstallError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| InstallError::Io { path, source }
    };

    fs::create_dir_all(to).map_err(io_error(to))?;
    // The source's own mode, so a `0700` directory does not become `0755`
    // because the hub's umask happened to be looser.
    let mode = fs::metadata(from).map_err(io_error(from))?.permissions();
    fs::set_permissions(to, mode).map_err(io_error(to))?;

    for entry in fs::read_dir(from).map_err(io_error(from))? {
        let entry = entry.map_err(io_error(from))?;
        let name = entry.file_name();
        let relative = relative.join(&name);
        if is_excluded(&relative) {
            continue;
        }

        let source_path = from.join(&name);
        let target_path = to.join(&name);
        let metadata = fs::symlink_metadata(&source_path).map_err(io_error(&source_path))?;

        if metadata.is_symlink() {
            // Recreated, never followed: a link is part of the tree, and
            // resolving it here would either duplicate what it points at or
            // pull in something outside the project entirely.
            let target = fs::read_link(&source_path).map_err(io_error(&source_path))?;
            std::os::unix::fs::symlink(target, &target_path).map_err(io_error(&target_path))?;
        } else if metadata.is_dir() {
            copy_tree(&source_path, &target_path, &relative)?;
        } else {
            // `fs::copy` carries the permission bits over, which is what keeps
            // `bin/console` executable on the other side.
            fs::copy(&source_path, &target_path).map_err(io_error(&target_path))?;
        }
    }

    Ok(())
}

/// Everything that can stop an install, in one type so the command has one
/// place to print from.
#[derive(Debug)]
pub enum InstallError {
    Source(SourceError),
    Manifest(ManifestError),
    Paths(PathsError),
    Registry(RegistryError),
    /// The derived or given `id` cannot name a directory or be typed as one
    /// word.
    UnusableId {
        id: String,
        derived: bool,
    },
    /// Another installed app already answers to this `id`.
    IdTaken {
        id: String,
        location: String,
    },
    /// `apps/<id>/` exists with no registry entry to explain it.
    DirectoryInTheWay {
        path: PathBuf,
    },
    MissingFile {
        path: PathBuf,
        why: &'static str,
    },
    UnusableVersion {
        path: PathBuf,
        version: String,
        detail: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(formatter, "{error}"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::UnusableId { id, derived } => {
                let source = match derived {
                    true => format!(
                        "\"project_name\" is {id:?} in the app's {MANIFEST_FILE}, and the hub \
                         installs under that name by default"
                    ),
                    false => format!("--as {id:?}"),
                };
                write!(
                    formatter,
                    "{source} — an app id has to start with a letter or a digit and hold \
                     nothing but letters, digits, \"-\", \"_\" and \".\": it names a \
                     directory and is typed as one word. Pass --as <id> to pick another."
                )
            }
            Self::IdTaken { id, location } => write!(
                formatter,
                "{id} is already installed, from {location}. Pick another handle with \
                 --as <id>, or remove that one first."
            ),
            Self::DirectoryInTheWay { path } => write!(
                formatter,
                "{} already exists but nothing is registered under it — an earlier install \
                 was probably interrupted. Remove that directory by hand and try again; \
                 the hub will not copy over a tree it cannot account for.",
                path.display()
            ),
            Self::MissingFile { path, why } => {
                write!(formatter, "{} is missing — {why}.", path.display())
            }
            Self::UnusableVersion {
                path,
                version,
                detail,
            } => write!(
                formatter,
                "\"app_version\" is {version:?} in {}, which is not canonical semver \
                 ({detail}). It is what decides install / update / downgrade later on, so \
                 it has to be comparable (CONTRACT.md §2).",
                path.display()
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<SourceError> for InstallError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

impl From<ManifestError> for InstallError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<PathsError> for InstallError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<RegistryError> for InstallError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
