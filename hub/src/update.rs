//! `tfsapp-hub update <id>` — replace an installed app with a newer version of
//! its own source (the per-app update and rollback plan, `000-index.md`).
//!
//! This module holds two things that need no I/O and are worth getting right
//! in isolation before the command around them exists at all:
//!
//! - [`update_decision`] — the Overview's decision table, built on the same
//!   [`lifecycle::lifecycle_decision`] `open`, `run` and `install` already use.
//!   No second comparison, no second spelling of what "newer" means.
//! - the tree-anchor helpers ([`retain_tree`], [`restore_tree`],
//!   [`discard_tree`]) — the code half of the rollback anchor
//!   ([`lifecycle::previous_tree_path`]), which is a rename rather than a
//!   copy: an update pays for one generation of the tree, never a copy pass
//!   over it.
//!
//! The command's own imperative flow is [`run`]: resolve the source, guard
//! the data directory, snapshot the database, swap the trees, run the
//! event's commands, and commit or revert the whole thing.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    app_env::{self, EnvError},
    cli::{EXIT_FAILED, EXIT_OK},
    install::{self, InstallError},
    lifecycle::{self, LifecycleDecisionError, LifecycleError, LifecycleEvent},
    manifest::Manifest,
    paths::{Paths, PathsError},
    php::{self, PhpError},
    platform::{self, PlatformError},
    prompt,
    registry::{self, RegistryEntry, RegistryError, Source, SourceKind},
    source::{self, Origin, SourceError},
};

/// What `update <id>` does, decided from the registry's recorded `app_version`
/// against the freshly resolved source's own. No I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// The update event: snapshot, swap, `pre-update` then `post-update`,
    /// commit.
    Apply,
    /// `--force` on an equal record: re-sync the code and its dependencies,
    /// run no lifecycle command, take no snapshot, and leave the existing
    /// anchor alone.
    ResyncOnly,
}

/// Why [`update_decision`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRefusal {
    /// Nothing is recorded for this id at all — that moment is an install,
    /// and `install` owns it. Unreachable from the real command (a
    /// registered `id` always carries a registry `app_version`); kept as a
    /// row of the decision table anyway so it is one function to test rather
    /// than an assumption baked silently into the caller.
    NoRecord,
    /// The source is exactly what is already installed. `--force` unlocks
    /// [`UpdateAction::ResyncOnly`]; without it, this refuses.
    Equal { version: semver::Version },
    /// The source is *older* than what is installed — a downgrade, which
    /// `--force` does not unlock: it is for the case `source_revision` exists
    /// to catch (a tree edited without bumping the version), not for
    /// reverting to an older release.
    Downgrade {
        recorded: semver::Version,
        source: semver::Version,
    },
    /// The registry's own recorded `app_version` does not parse as semver.
    /// Unreachable in practice — `install::validate` already refused a
    /// non-canonical one at install time — carried as a message (`semver::Error`
    /// implements neither `Clone` nor `PartialEq`) so this function is total
    /// over its input rather than panicking on a corrupt registry.
    InvalidRecordedVersion(String),
}

/// The Overview's decision table:
///
/// | record vs source | action |
/// | --- | --- |
/// | source newer | [`UpdateAction::Apply`] |
/// | equal | [`UpdateRefusal::Equal`], naming `--force` |
/// | equal, with `--force` | [`UpdateAction::ResyncOnly`] |
/// | source older | [`UpdateRefusal::Downgrade`] — `--force` does not unlock it |
/// | no record at all | [`UpdateRefusal::NoRecord`] |
///
/// `recorded` is the registry entry's own `app_version`, straight from the
/// caller's already-loaded entry (never [`lifecycle::read_data_version`] —
/// that record belongs to `install`'s own gate, not this one). `source` is
/// the freshly re-resolved source's `app_version`, already validated as
/// semver by whichever `validate` the caller ran.
pub fn update_decision(
    recorded: Option<&str>,
    source: &semver::Version,
    force: bool,
) -> Result<UpdateAction, UpdateRefusal> {
    match lifecycle::lifecycle_decision(recorded, source) {
        Ok(LifecycleEvent::Install) => Err(UpdateRefusal::NoRecord),
        Ok(LifecycleEvent::None) => match force {
            true => Ok(UpdateAction::ResyncOnly),
            false => Err(UpdateRefusal::Equal {
                version: source.clone(),
            }),
        },
        Ok(LifecycleEvent::Update) => Ok(UpdateAction::Apply),
        Err(LifecycleDecisionError::Downgrade { recorded, current }) => {
            Err(UpdateRefusal::Downgrade {
                recorded,
                source: current,
            })
        }
        Err(LifecycleDecisionError::InvalidVersion(error)) => {
            Err(UpdateRefusal::InvalidRecordedVersion(error.to_string()))
        }
    }
}

/// Rename `app_dir` to its `.previous` sibling
/// ([`lifecycle::previous_tree_path`]), removing any older one first.
///
/// An update never copies the outgoing tree, it moves it (the plan's
/// Overview): the cost of holding an anchor is one generation of `apps/<id>`,
/// not a copy pass over it. A leftover `.previous` from an interrupted
/// earlier update is replaced rather than appended to — there is only ever
/// one generation to roll back to.
pub fn retain_tree(app_dir: &Path) -> io::Result<()> {
    let previous = lifecycle::previous_tree_path(app_dir);
    if previous.is_dir() {
        fs::remove_dir_all(&previous)?;
    }
    fs::rename(app_dir, &previous)
}

/// Rename the `.previous` tree back to `app_dir` — a failed update's undo of
/// [`retain_tree`], and `rollback <id>`'s own restoration of the anchor's
/// tree half.
pub fn restore_tree(app_dir: &Path) -> io::Result<()> {
    fs::rename(lifecycle::previous_tree_path(app_dir), app_dir)
}

/// Delete the `.previous` tree without touching `app_dir` — `remove <id>`'s
/// own anchor cleanup, and `rollback <id>`'s consumption of the anchor's tree
/// half once [`restore_tree`] has already put it back as `app_dir`.
/// Best-effort: an already-missing tree is not an error.
pub fn discard_tree(app_dir: &Path) {
    let _ = fs::remove_dir_all(lifecycle::previous_tree_path(app_dir));
}

/// The whole command: update `id`, or say why not. Returns the process's
/// exit code.
pub fn run(
    id: &str,
    reference: Option<&str>,
    force: bool,
    assume_yes: bool,
    hub_version: &str,
) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match update(&paths, id, reference, force, assume_yes, hub_version) {
        Ok(true) => EXIT_OK,
        // Declining is not a failure of the command, but nothing changed
        // either — a script reading 0 would conclude it did.
        Ok(false) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The pipeline, in order (the plan's Overview): load the registry entry,
/// re-resolve and validate its source, decide the event, guard the data
/// directory, confirm, then apply or resync. `false` means the user declined.
///
/// Takes its `Paths` rather than resolving them, matching `install::install`
/// — what lets the whole pipeline run against a throwaway root in a test.
fn update(
    paths: &Paths,
    id: &str,
    reference: Option<&str>,
    force: bool,
    assume_yes: bool,
    hub_version: &str,
) -> Result<bool, UpdateError> {
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| UpdateError::NotInstalled { id: id.to_string() })?
        .clone();

    let resolved = source::resolve(&origin(&entry.source), reference)?;
    let loaded = install::validate(&resolved.root)?;
    loaded.report_warnings();
    let manifest = &loaded.manifest;

    let current = semver::Version::parse(&manifest.app_version)
        .expect("validate() already refused a non-canonical app_version");
    let action = update_decision(Some(&entry.app_version), &current, force)
        .map_err(|refusal| UpdateError::refused(id, refusal))?;

    let data_dir = paths.app_data_dir(&entry.identifier)?;
    install::check_data_dir_available(id, &data_dir)?;

    // Resolved before the question, since an update nothing could finish is
    // not worth asking about — the same reasoning `install::install` uses.
    let toolchain = php::toolchain(paths)?;
    let platform = platform::probe(&toolchain.frankenphp)?.fingerprint();

    let app_dir = paths.app_dir(id)?;
    announce(id, &entry, manifest, &resolved, action);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(false);
    }

    match action {
        UpdateAction::Apply => apply(
            paths,
            &toolchain,
            id,
            &entry,
            manifest,
            &resolved,
            &app_dir,
            &data_dir,
            platform,
            hub_version,
        )?,
        UpdateAction::ResyncOnly => {
            resync_only(paths, &toolchain, &entry, manifest, &resolved, &app_dir)?
        }
    }

    Ok(true)
}

/// The registry's own `Source` turned back into the [`Origin`] `source::resolve`
/// takes. Reads the recorded `kind` rather than re-classifying the location
/// string: a canonicalised local path would classify the same way regardless,
/// but reading the kind is what this will need the day a git `location` (a
/// URL) has to resolve as a git source and not be re-guessed from its string.
fn origin(source: &Source) -> Origin {
    match source.kind {
        SourceKind::LocalPath => Origin::LocalPath(PathBuf::from(&source.location)),
        SourceKind::Git => Origin::Git(source.location.clone()),
    }
}

/// Say what is about to happen, in the terms the user will have to reason
/// about afterwards — `install::announce`'s counterpart for `update`.
fn announce(
    id: &str,
    entry: &RegistryEntry,
    manifest: &Manifest,
    resolved: &source::Resolved,
    action: UpdateAction,
) {
    println!(
        "Update {id}: {} -> {}",
        entry.app_version, manifest.app_version
    );
    println!("  source    {}", resolved.root.display());
    match action {
        UpdateAction::Apply => {
            println!(
                "  will run  pre-update, then post-update — its database is snapshotted \
                 first, and put back if anything fails"
            );
            println!();
            println!(
                "This runs the app's own PHP on your machine: Composer's dependency\n\
                 resolution, the scripts it fires, and the app's own update commands.\n\
                 There is no sandbox — it is the same trust you give `composer require`."
            );
        }
        UpdateAction::ResyncOnly => {
            println!(
                "  will run  composer install only — {id} is already recorded at {}. \
                 --force re-syncs a source that changed without a version bump: no \
                 lifecycle command runs, no database snapshot is taken, and the \
                 existing rollback point (if any) is left alone.",
                manifest.app_version
            );
        }
    }
}

/// The update event: snapshot the database, retain the outgoing tree, copy
/// the new one in, empty the caches a stale compiled container would leave
/// behind, then run [`install::prepare`] under [`LifecycleEvent::Update`].
///
/// On any failure from the tree swap onwards, the whole attempt is reverted
/// — the database snapshot restored, the new tree removed, the outgoing tree
/// renamed back — and the registry is never touched: the installation is
/// exactly what it was, and the next `open` does not know an update was
/// attempted (the plan's "What a failed update leaves").
#[allow(clippy::too_many_arguments)]
fn apply(
    paths: &Paths,
    toolchain: &php::Toolchain,
    id: &str,
    entry: &RegistryEntry,
    manifest: &Manifest,
    resolved: &source::Resolved,
    app_dir: &Path,
    data_dir: &Path,
    platform: registry::Platform,
    hub_version: &str,
) -> Result<(), UpdateError> {
    let data_subdir = data_dir.join("data");

    lifecycle::snapshot_db(&data_subdir).map_err(|source| UpdateError::Io {
        path: data_subdir.clone(),
        source,
    })?;
    retain_tree(app_dir).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;

    let revert = || {
        let _ = lifecycle::restore_db_snapshot(&data_subdir);
        let _ = fs::remove_dir_all(app_dir);
        let _ = restore_tree(app_dir);
    };

    if let Err(error) = install::snapshot(&resolved.root, app_dir) {
        revert();
        return Err(UpdateError::Reverted {
            detail: error.to_string(),
        });
    }

    // The update commands are about to boot a container compiled from the
    // code that is no longer there — best-effort, matching `app_env`'s own
    // launch-time wipe (`prepare`'s `app_env::resolve` recreates both, empty,
    // right after).
    let _ = fs::remove_dir_all(data_dir.join("cache"));
    let _ = fs::remove_dir_all(data_dir.join("build"));

    if let Err(error) =
        install::prepare(paths, toolchain, manifest, app_dir, LifecycleEvent::Update)
    {
        revert();
        return Err(UpdateError::Reverted {
            detail: error.to_string(),
        });
    }

    // Success, in order (the plan's "On success"): the event's success point
    // already ran inside `prepare`, then the anchor's registry half, from the
    // *outgoing* entry — this is what `rollback <id>` restores to — then the
    // registry entry itself, then the desktop entry, since a re-snapshot can
    // change `product_name` or `icon_path`.
    lifecycle::write_rollback_anchor(
        &data_subdir,
        &lifecycle::RollbackAnchor {
            app_version: entry.app_version.clone(),
            source_revision: entry.source_revision.clone(),
            created_at: registry::now_timestamp(),
        },
    )?;

    let now = registry::now_timestamp();
    registry::update(paths, |registry| {
        registry.stamp(hub_version, platform.clone());
        if let Some(existing) = registry.get_mut(id) {
            existing.app_version = manifest.app_version.clone();
            existing.source_revision = resolved.revision.clone();
            existing.platform = platform;
            existing.updated_at = now;
        }
    })?;

    install::write_desktop_entry(paths, id, manifest, app_dir);
    println!("Updated {id} to {}.", manifest.app_version);
    Ok(())
}

/// `--force` on an equal record: re-copy the source and re-run its
/// dependency install, and stop there. No hooks, no database snapshot, no
/// anchor rotation — the existing rollback point, if any, is left exactly as
/// it was.
fn resync_only(
    paths: &Paths,
    toolchain: &php::Toolchain,
    entry: &RegistryEntry,
    manifest: &Manifest,
    resolved: &source::Resolved,
    app_dir: &Path,
) -> Result<(), UpdateError> {
    // Never through `retain_tree`/`lifecycle::previous_tree_path`: that name
    // is the rollback anchor's, and a resync must not rotate it (the plan's
    // decision table). A plain re-copy in place — `install::snapshot` refuses
    // an existing target, so the stale tree goes first.
    fs::remove_dir_all(app_dir).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;
    install::snapshot(&resolved.root, app_dir)?;

    let state_root = paths.create_app_data_dir(&entry.identifier)?;
    let environment = app_env::resolve(
        manifest,
        app_dir,
        &entry.identifier,
        &state_root,
        app_env::Mode::Install,
    )?;
    toolchain.composer_install(app_dir, &environment.vars)?;

    let now = registry::now_timestamp();
    registry::update(paths, |registry| {
        if let Some(existing) = registry.get_mut(&entry.id) {
            existing.source_revision = resolved.revision.clone();
            existing.updated_at = now;
        }
    })?;

    println!("Resynced {} from {}.", entry.id, resolved.root.display());
    Ok(())
}

/// Everything that can stop an update, in one type so the command has one
/// place to print from.
#[derive(Debug)]
pub enum UpdateError {
    Paths(PathsError),
    Registry(RegistryError),
    Source(SourceError),
    Install(InstallError),
    Env(EnvError),
    Php(PhpError),
    Platform(PlatformError),
    Lifecycle(LifecycleError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// No app is registered under this id at all.
    NotInstalled {
        id: String,
    },
    /// The registry's own recorded `app_version` names an update this hub
    /// does not own — unreachable from the real command (a registered `id`
    /// always carries one), kept so the type is total.
    NoRecord {
        id: String,
    },
    /// The source is exactly what is already installed.
    Equal {
        id: String,
        version: semver::Version,
    },
    /// The source is older than what is installed.
    Downgrade {
        id: String,
        recorded: semver::Version,
        source: semver::Version,
    },
    /// The registry's own recorded `app_version` does not parse as semver.
    InvalidRecordedVersion {
        id: String,
        detail: String,
    },
    /// A failure from the tree swap onwards: the original error, with the
    /// installation already put back to what it was.
    Reverted {
        detail: String,
    },
}

impl UpdateError {
    fn refused(id: &str, refusal: UpdateRefusal) -> Self {
        match refusal {
            UpdateRefusal::NoRecord => Self::NoRecord { id: id.to_string() },
            UpdateRefusal::Equal { version } => Self::Equal {
                id: id.to_string(),
                version,
            },
            UpdateRefusal::Downgrade { recorded, source } => Self::Downgrade {
                id: id.to_string(),
                recorded,
                source,
            },
            UpdateRefusal::InvalidRecordedVersion(detail) => Self::InvalidRecordedVersion {
                id: id.to_string(),
                detail,
            },
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Source(error) => write!(formatter, "{error}"),
            Self::Install(error) => write!(formatter, "{error}"),
            Self::Env(error) => write!(formatter, "{error}"),
            Self::Php(error) => write!(formatter, "{error}"),
            Self::Platform(error) => write!(formatter, "{error}"),
            Self::Lifecycle(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::NotInstalled { id } => write!(
                formatter,
                "no app is installed as {id} — `tfsapp-hub list` shows the ones that are."
            ),
            Self::NoRecord { id } => write!(
                formatter,
                "{id} has no recorded version to update from — this should not happen for \
                 an installed app. Reinstall it."
            ),
            Self::Equal { id, version } => write!(
                formatter,
                "{id} is already at version {version} — nothing to update. Pass --force to \
                 re-sync its code and dependencies anyway, for a source that changed \
                 without a version bump."
            ),
            Self::Downgrade {
                id,
                recorded,
                source,
            } => write!(
                formatter,
                "{id}'s resolved source is version {source}, older than the {recorded} \
                 already recorded for it — that would be a downgrade, and update does not \
                 apply one. --force does not unlock this: it is for a tree edited without \
                 bumping the version, not for going backwards."
            ),
            Self::InvalidRecordedVersion { id, detail } => write!(
                formatter,
                "the version recorded for {id} does not parse as semver ({detail}) — \
                 reinstall it."
            ),
            Self::Reverted { detail } => write!(
                formatter,
                "{detail} The installation was put back to its previous state."
            ),
        }
    }
}

impl std::error::Error for UpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Source(error) => Some(error),
            Self::Install(error) => Some(error),
            Self::Env(error) => Some(error),
            Self::Php(error) => Some(error),
            Self::Platform(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<PathsError> for UpdateError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<RegistryError> for UpdateError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<SourceError> for UpdateError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

impl From<InstallError> for UpdateError {
    fn from(error: InstallError) -> Self {
        Self::Install(error)
    }
}

impl From<EnvError> for UpdateError {
    fn from(error: EnvError) -> Self {
        Self::Env(error)
    }
}

impl From<PhpError> for UpdateError {
    fn from(error: PhpError) -> Self {
        Self::Php(error)
    }
}

impl From<PlatformError> for UpdateError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl From<LifecycleError> for UpdateError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
