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
    lifecycle_gate::{self, GateError},
    manifest::Manifest,
    paths::{Paths, PathsError},
    php::{self, PhpError},
    platform::{self, PlatformError},
    prompt,
    registry::{self, RegistryEntry, RegistryError, Source, SourceKind},
    release,
    source::{self, Origin, SourceError},
    update_transaction::{self, Journal, Phase, TransactionKind},
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
#[allow(dead_code)] // Kept for the focused legacy-anchor tests during the protocol migration.
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

/// The temporary sibling used while a forced equal-version resync replaces an
/// app tree. It is deliberately not the `.previous` rollback anchor: a resync
/// must leave that anchor alone.
#[allow(dead_code)]
fn resync_aside_path(app_dir: &Path) -> PathBuf {
    let mut aside = app_dir.as_os_str().to_os_string();
    aside.push(".resync-aside");
    PathBuf::from(aside)
}

/// Move the current tree aside, copy its replacement, and put the current
/// tree back if the copy fails. A stale aside can only be from an interrupted
/// earlier resync, so the next resync clears it before creating its own.
#[allow(dead_code)]
fn resync_snapshot(source_root: &Path, app_dir: &Path) -> Result<(), UpdateError> {
    let aside = resync_aside_path(app_dir);
    remove_dir_if_present(&aside).map_err(|source| UpdateError::Io {
        path: aside.clone(),
        source,
    })?;
    fs::rename(app_dir, &aside).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;

    if let Err(error) = install::snapshot(source_root, app_dir) {
        restore_resync_tree(app_dir)?;
        return Err(UpdateError::Reverted {
            detail: error.to_string(),
            outcome: RevertOutcome::default(),
        });
    }

    Ok(())
}

/// Discard the newly copied tree and restore the pre-resync one. This is the
/// undo for [`resync_snapshot`] when Composer cannot finish the replacement.
#[allow(dead_code)]
fn restore_resync_tree(app_dir: &Path) -> Result<(), UpdateError> {
    remove_dir_if_present(app_dir).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;
    let aside = resync_aside_path(app_dir);
    fs::rename(&aside, app_dir).map_err(|source| UpdateError::Io {
        path: aside,
        source,
    })
}

#[allow(dead_code)]
fn discard_resync_aside(app_dir: &Path) -> Result<(), UpdateError> {
    let aside = resync_aside_path(app_dir);
    remove_dir_if_present(&aside).map_err(|source| UpdateError::Io {
        path: aside,
        source,
    })
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
///
/// `pub(crate)` rather than private: `rollback_tests.rs`'s own round-trip
/// test reuses it to seed an updated app to roll back, rather than
/// reimplementing an update fixture a second time.
pub(crate) fn update(
    paths: &Paths,
    id: &str,
    reference: Option<&str>,
    force: bool,
    assume_yes: bool,
    hub_version: &str,
) -> Result<bool, UpdateError> {
    // Same reasoning as `install::install`'s own wrapper: a release
    // downloads and extracts into it, and it is removed on the way out
    // regardless of how this call ends. A local source never touches it.
    let scratch = paths.scratch_dir();
    let result = update_into(
        paths,
        &scratch,
        release::GITHUB_API_BASE,
        id,
        reference,
        force,
        assume_yes,
        hub_version,
    );
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// [`update`]'s pipeline, against `base_url` instead of GitHub's real API —
/// the seam `update_tests.rs` uses to run a whole update against a local
/// stub. Production always calls this with [`release::GITHUB_API_BASE`],
/// through [`update`] above.
#[allow(clippy::too_many_arguments)]
fn update_into(
    paths: &Paths,
    scratch: &Path,
    base_url: &str,
    id: &str,
    reference: Option<&str>,
    force: bool,
    assume_yes: bool,
    hub_version: &str,
) -> Result<bool, UpdateError> {
    // The first lookup only discovers the stable identifier needed to name
    // its gate. No mutable premise from it is trusted after the lease.
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| UpdateError::NotInstalled { id: id.to_string() })?
        .clone();

    let _maintenance = lifecycle_gate::acquire_maintenance(paths, &entry.identifier, "update")?;

    // Another command could have completed between the discovery above and
    // our acquisition. Re-read every stateful input while owning the gate.
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| UpdateError::NotInstalled { id: id.to_string() })?
        .clone();

    let resolved = source::resolve(&origin(&entry.source), reference, scratch, base_url)?;
    let install::Validated {
        loaded,
        app_version,
    } = install::validate(&resolved.root)?;
    loaded.report_warnings();
    let manifest = &loaded.manifest;

    let action = update_decision(Some(&entry.app_version), &app_version, force)
        .map_err(|refusal| UpdateError::refused(id, refusal))?;

    let data_dir = paths.app_data_dir(&entry.identifier)?;
    install::check_data_dir_available(id, &entry.identifier, &data_dir)?;

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
/// but a release's `location` is `owner/repo`, not a spec `classify` has ever
/// seen again — this is the day that needed a real read of the recorded kind,
/// not a re-guess from a string.
fn origin(source: &Source) -> Origin {
    match source.kind {
        SourceKind::LocalPath => Origin::LocalPath(PathBuf::from(&source.location)),
        SourceKind::Release => Origin::Release {
            index: source.index.clone(),
            repo: source.location.clone(),
        },
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
/// exactly what it was unless an undo itself fails, in which case every undo
/// is still attempted and the error names the failed paths.
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
    let mut transaction = Journal::prepared(TransactionKind::Apply, entry.clone());
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;

    transaction.database_members = update_transaction::snapshot_db(&data_subdir, data_dir)
        .map_err(|source| UpdateError::Io {
            path: data_subdir.clone(),
            source,
        })?;
    transaction.advance(Phase::SnapshotComplete);
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;
    update_transaction::retain_tree(app_dir).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;
    transaction.advance(Phase::TreeRetained);
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;

    if let Err(error) = install::snapshot(&resolved.root, app_dir) {
        return Err(recover_after_failure(
            paths,
            data_dir,
            app_dir,
            &transaction,
            error,
        ));
    }
    transaction.advance(Phase::ReplacementInstalled);
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;

    // The update commands are about to boot a container compiled from the
    // code that is no longer there — best-effort, matching `app_env`'s own
    // launch-time wipe (`prepare`'s `app_env::resolve` recreates both, empty,
    // right after).
    let _ = fs::remove_dir_all(data_dir.join("cache"));
    let _ = fs::remove_dir_all(data_dir.join("build"));

    if let Err(error) = install::prepare(
        paths,
        toolchain,
        manifest,
        app_dir,
        LifecycleEvent::Update,
        &platform,
    ) {
        return Err(recover_after_failure(
            paths,
            data_dir,
            app_dir,
            &transaction,
            error,
        ));
    }
    transaction.advance(Phase::LifecycleComplete);
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;

    // The event's success point already ran inside `prepare`; the complete
    // anchor already records the outgoing entry. Commit the registry entry,
    // then the desktop entry, since a re-snapshot can change `product_name`
    // or `icon_path`.
    let now = registry::now_timestamp();
    if let Err(error) = registry::update(paths, |registry| {
        registry.stamp(hub_version, platform.clone());
        if let Some(existing) = registry.get_mut(id) {
            existing.app_version = manifest.app_version.clone();
            existing.source_revision = resolved.revision.clone();
            // The freshly resolved `Source`, not just its revision: a remote
            // source's `reference` is the tag this update actually landed
            // on, and it moves on every successful update even when
            // `location`/`index` do not — leaving the old tag recorded would
            // have `list` and the next `update` both reasoning from a lie.
            existing.source = resolved.source.clone();
            existing.platform = platform;
            existing.updated_at = now;
        }
    }) {
        return Err(recover_after_failure(
            paths,
            data_dir,
            app_dir,
            &transaction,
            error,
        ));
    }
    transaction.advance(Phase::RegistryCommitted);
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;
    if let Err(error) = update_transaction::finalise_anchor(&data_subdir, data_dir, app_dir, entry)
    {
        return Err(recover_after_failure(
            paths,
            data_dir,
            app_dir,
            &transaction,
            error,
        ));
    }
    transaction.advance(Phase::AnchorFinalised);
    update_transaction::write(data_dir, &transaction).map_err(transaction_error)?;
    update_transaction::discard(data_dir).map_err(|source| UpdateError::Io {
        path: data_dir.to_path_buf(),
        source,
    })?;

    install::write_desktop_entry(paths, id, manifest, app_dir);
    println!("Updated {id} to {}.", manifest.app_version);
    Ok(())
}

fn transaction_error(error: update_transaction::JournalError) -> UpdateError {
    match error {
        update_transaction::JournalError::Io { path, source } => UpdateError::Io { path, source },
        other => UpdateError::Io {
            path: PathBuf::from("update transaction"),
            source: io::Error::other(other.to_string()),
        },
    }
}

/// Restore precisely the outgoing state described by a durable journal.  The
/// phase is the authority: an incomplete snapshot or a missing staged tree is
/// never treated as evidence that it is safe to delete a live app.
fn recover_after_failure(
    paths: &Paths,
    data_dir: &Path,
    app_dir: &Path,
    transaction: &Journal,
    error: impl std::fmt::Display,
) -> UpdateError {
    UpdateError::Reverted {
        detail: error.to_string(),
        outcome: recover_transaction(paths, data_dir, app_dir, transaction),
    }
}

pub(crate) fn recover_transaction(
    paths: &Paths,
    data_dir: &Path,
    app_dir: &Path,
    transaction: &Journal,
) -> RevertOutcome {
    let mut outcome = RevertOutcome::default();
    if transaction.phase == Phase::AnchorFinalised {
        outcome.record(
            "transaction cleanup",
            data_dir.to_path_buf(),
            update_transaction::discard(data_dir),
        );
        return outcome;
    }

    let data_subdir = data_dir.join("data");
    let tree_is_authoritative = matches!(
        transaction.phase,
        Phase::TreeRetained
            | Phase::ReplacementInstalled
            | Phase::LifecycleComplete
            | Phase::RegistryCommitted
    );
    let snapshot_is_authoritative = transaction.kind == TransactionKind::Apply
        && matches!(
            transaction.phase,
            Phase::SnapshotComplete
                | Phase::TreeRetained
                | Phase::ReplacementInstalled
                | Phase::LifecycleComplete
                | Phase::RegistryCommitted
        );

    // `retain_tree`'s rename is the one mutation that can have happened
    // without yet being durable at these two phases (`apply`/`resync_only`
    // write `Prepared` before it, `SnapshotComplete` — Apply only — right
    // before it too). What it did is visible on disk: `app_dir` is either
    // still there (the rename never ran) or it is not (it ran and the
    // journal did not catch up). Never inferred as a phase, only observed as
    // the one step this phase allows next.
    if matches!(transaction.phase, Phase::Prepared | Phase::SnapshotComplete) {
        let staged = update_transaction::staged_tree_path(app_dir);
        match (app_dir.is_dir(), staged.is_dir()) {
            (false, true) => {
                outcome.record(
                    "tree swap-back",
                    staged.clone(),
                    fs::rename(&staged, app_dir),
                );
            }
            (true, true) => {
                // The rename never happened this attempt; `staged` is a
                // leftover from an earlier interrupted transaction and
                // `app_dir` is authoritative.
                outcome.record(
                    "stale retained tree discard",
                    staged.clone(),
                    remove_dir_if_present(&staged),
                );
            }
            (false, false) => {
                outcome.record(
                    "tree",
                    app_dir.to_path_buf(),
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "neither the live tree nor a retained one exists",
                    )),
                );
            }
            (true, false) => {}
        }
    }

    if snapshot_is_authoritative {
        for name in lifecycle::DB_FILE_NAMES {
            let live = data_subdir.join(name);
            if transaction
                .database_members
                .iter()
                .any(|member| member == name)
            {
                let staged = update_transaction::staged_db_path(data_dir, name);
                if !staged.is_file() {
                    outcome.record(
                        "database snapshot restore",
                        staged,
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "staged member is missing",
                        )),
                    );
                } else {
                    let restore = fs::copy(&staged, &live).map(|_| ());
                    outcome.record("database snapshot restore", live, restore);
                }
            } else {
                outcome.record(
                    "database snapshot absence restore",
                    live.clone(),
                    remove_file_if_present(&live),
                );
            }
        }
    }

    if tree_is_authoritative {
        let staged = update_transaction::staged_tree_path(app_dir);
        if staged.is_dir() {
            outcome.record(
                "replacement tree discard",
                app_dir.to_path_buf(),
                remove_dir_if_present(app_dir),
            );
            outcome.record(
                "tree swap-back",
                staged.clone(),
                fs::rename(&staged, app_dir),
            );
        }
    }

    if tree_is_authoritative {
        outcome.record(
            "cache stamp discard",
            lifecycle::cache_stamp_path(&data_subdir),
            lifecycle::discard_cache_stamp(&data_subdir),
        );
        if transaction.kind == TransactionKind::Apply {
            outcome.record(
                "data version rewrite",
                lifecycle::data_config_path(&data_subdir),
                lifecycle::write_data_version(&data_subdir, &transaction.outgoing.app_version)
                    .map_err(lifecycle_error_io),
            );
        }
        outcome.record(
            "registry entry restore",
            paths.registry_path(),
            registry::update(paths, |registry| {
                if let Some(existing) = registry.get_mut(&transaction.outgoing.id) {
                    *existing = transaction.outgoing.clone();
                } else {
                    registry.apps.push(transaction.outgoing.clone());
                }
            })
            .map(|_| ())
            .map_err(|error| io::Error::other(error.to_string())),
        );
    }

    if outcome.is_complete() {
        outcome.record(
            "transaction cleanup",
            data_dir.to_path_buf(),
            update_transaction::discard(data_dir),
        );
    }
    outcome
}

#[derive(Debug, Default)]
pub struct RevertOutcome {
    failures: Vec<RevertFailure>,
}

#[derive(Debug)]
struct RevertFailure {
    step: &'static str,
    path: PathBuf,
    detail: String,
}

#[allow(dead_code)]
impl RevertOutcome {
    fn record(&mut self, step: &'static str, path: PathBuf, result: io::Result<()>) {
        if let Err(error) = result {
            self.failures.push(RevertFailure {
                step,
                path,
                detail: error.to_string(),
            });
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Attempt every half of an update's undo, retaining every failure for the
/// caller to report. A failed restore must not hide the attempts that follow:
/// the user needs the full on-disk state before deciding whether to free disk
/// space and retry `update` or use `rollback`.
#[allow(dead_code)]
fn revert(data_subdir: &Path, app_dir: &Path, outgoing_version: &str) -> RevertOutcome {
    let mut outcome = RevertOutcome::default();
    outcome.record(
        "database snapshot restore",
        data_subdir.to_path_buf(),
        lifecycle::restore_db_snapshot(data_subdir),
    );
    for name in lifecycle::DB_FILE_NAMES {
        let path = lifecycle::db_snapshot_path(data_subdir, name);
        outcome.record(
            "database snapshot discard",
            path.clone(),
            remove_file_if_present(&path),
        );
    }
    outcome.record(
        "new tree discard",
        app_dir.to_path_buf(),
        remove_dir_if_present(app_dir),
    );
    let previous = lifecycle::previous_tree_path(app_dir);
    outcome.record("tree swap-back", previous, restore_tree(app_dir));
    // A stamp `prepare`'s own warm-up wrote for the version this update was
    // moving to must not survive next to a tree just reverted back to the
    // version it was moving from — plan 024.
    let stamp = lifecycle::cache_stamp_path(data_subdir);
    outcome.record(
        "cache stamp discard",
        stamp,
        lifecycle::discard_cache_stamp(data_subdir),
    );
    outcome.record(
        "data version rewrite",
        lifecycle::data_config_path(data_subdir),
        lifecycle::write_data_version(data_subdir, outgoing_version).map_err(lifecycle_error_io),
    );
    let record = lifecycle::rollback_anchor_path(data_subdir);
    outcome.record(
        "rollback record discard",
        record.clone(),
        remove_file_if_present(&record),
    );
    outcome
}

#[allow(dead_code)]
fn remove_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[allow(dead_code)]
fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[allow(dead_code)]
fn lifecycle_error_io(error: LifecycleError) -> io::Error {
    io::Error::other(error.to_string())
}

/// `--force` on an equal record: move the current tree aside, re-copy the
/// source and re-run its dependency install, then discard the aside. No hooks,
/// database snapshot or anchor rotation — the existing rollback point, if any,
/// is left exactly as it was. A copy or Composer failure restores the aside;
/// only a later registry-write failure keeps the healthy replacement tree.
fn resync_only(
    paths: &Paths,
    toolchain: &php::Toolchain,
    entry: &RegistryEntry,
    manifest: &Manifest,
    resolved: &source::Resolved,
    app_dir: &Path,
) -> Result<(), UpdateError> {
    let state_root = paths.create_app_data_dir(&entry.identifier)?;
    let mut transaction = Journal::prepared(TransactionKind::ResyncOnly, entry.clone());
    update_transaction::write(&state_root, &transaction).map_err(transaction_error)?;
    // A resync uses the transaction sibling, never `.previous`: its existing
    // public rollback anchor remains untouched until this attempt is known good.
    update_transaction::retain_tree(app_dir).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;
    transaction.advance(Phase::TreeRetained);
    update_transaction::write(&state_root, &transaction).map_err(transaction_error)?;
    if let Err(error) = install::snapshot(&resolved.root, app_dir) {
        return Err(recover_after_failure(
            paths,
            &state_root,
            app_dir,
            &transaction,
            error,
        ));
    }
    transaction.advance(Phase::ReplacementInstalled);
    update_transaction::write(&state_root, &transaction).map_err(transaction_error)?;
    let environment = app_env::resolve(
        manifest,
        app_dir,
        &entry.identifier,
        &state_root,
        app_env::Mode::Install,
        // No window is built by an update either — see install.rs's own
        // call.
        false,
    )?;
    if let Err(error) = toolchain.composer_install(app_dir, &environment.vars) {
        return Err(recover_after_failure(
            paths,
            &state_root,
            app_dir,
            &transaction,
            error,
        ));
    }

    // A forced equal-version resync replaces the tree without changing any of
    // the fields a cache stamp compares. Keeping that stamp would let the next
    // launch reuse a container compiled from the tree just moved aside.
    let data_subdir = state_root.join("data");
    if let Err(error) = lifecycle::discard_cache_stamp(&data_subdir) {
        return Err(recover_after_failure(
            paths,
            &state_root,
            app_dir,
            &transaction,
            error,
        ));
    }

    let now = registry::now_timestamp();
    // Do not restore the aside if this write fails: the new tree and its
    // dependencies are healthy and already serving the resolved source. The
    // old revision only causes a later `--force` to repeat this safe resync.
    if let Err(error) = registry::update(paths, |registry| {
        if let Some(existing) = registry.get_mut(&entry.id) {
            existing.source_revision = resolved.revision.clone();
            // Same reasoning as `apply`'s own registry write: a resync
            // re-resolved the source too, and its `Source` — not only its
            // revision — is what has to be recorded.
            existing.source = resolved.source.clone();
            existing.updated_at = now;
        }
    }) {
        return Err(recover_after_failure(
            paths,
            &state_root,
            app_dir,
            &transaction,
            error,
        ));
    }
    transaction.advance(Phase::RegistryCommitted);
    update_transaction::write(&state_root, &transaction).map_err(transaction_error)?;
    update_transaction::discard_tree(app_dir).map_err(|source| UpdateError::Io {
        path: app_dir.to_path_buf(),
        source,
    })?;
    transaction.advance(Phase::AnchorFinalised);
    update_transaction::write(&state_root, &transaction).map_err(transaction_error)?;
    update_transaction::discard(&state_root).map_err(|source| UpdateError::Io {
        path: state_root.clone(),
        source,
    })?;

    // The same last step `apply` ends with, for the same reason: a resync
    // re-snapshots the tree, and the manifest it lands can change
    // `product_name` or `icon_path` — or, with `file_associations`, the
    // MIME types the entry advertises to the desktop. Leaving the old
    // entry standing would keep advertising a declaration the new tree no
    // longer carries; best-effort here too, because the resync itself has
    // already succeeded.
    install::write_desktop_entry(paths, &entry.id, manifest, app_dir);

    println!("Resynced {} from {}.", entry.id, resolved.root.display());
    Ok(())
}

/// Explicitly recover an update that was interrupted after its journal became
/// durable.  Normal commands deliberately never call this: choosing to put
/// the old version back is a user-visible decision.
pub fn repair(id: &str, assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    match repair_at(&paths, id, assume_yes) {
        Ok(true) => EXIT_OK,
        Ok(false) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

fn repair_at(paths: &Paths, id: &str, assume_yes: bool) -> Result<bool, UpdateError> {
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| UpdateError::NotInstalled { id: id.to_string() })?
        .clone();
    let _maintenance = lifecycle_gate::acquire_maintenance(paths, &entry.identifier, "repair")?;
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| UpdateError::NotInstalled { id: id.to_string() })?
        .clone();
    let data_dir = paths.app_data_dir(&entry.identifier)?;
    let journal = update_transaction::read(&data_dir)
        .map_err(transaction_error)?
        .ok_or_else(|| UpdateError::NoJournal { id: id.to_string() })?;
    if journal.outgoing.identifier != entry.identifier || journal.outgoing.id != entry.id {
        return Err(UpdateError::JournalMismatch {
            path: update_transaction::journal_path(&data_dir),
        });
    }
    let app_dir = paths.app_dir(id)?;
    println!(
        "Repairing {id}: this restores the interrupted {} attempt to {}.",
        match journal.kind {
            TransactionKind::Apply => "update",
            TransactionKind::ResyncOnly => "re-sync",
        },
        journal.outgoing.app_version
    );
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(false);
    }
    let outcome = recover_transaction(paths, &data_dir, &app_dir, &journal);
    if !outcome.is_complete() {
        return Err(UpdateError::Reverted {
            detail: "repair could not restore every outgoing path.".into(),
            outcome,
        });
    }
    println!("Repaired {id} to {}.", journal.outgoing.app_version);
    Ok(true)
}

/// A cheap, shared fence for ordinary commands. The journal is checked before
/// command-specific filesystem diagnostics can mistake an interrupted swap
/// for a missing installation.
pub(crate) fn repair_required(paths: &Paths, id: &str) -> Result<bool, UpdateError> {
    let installed = registry::load(paths)?;
    let Some(entry) = installed.get(id) else {
        return Ok(false);
    };
    let data_dir = paths.app_data_dir(&entry.identifier)?;
    Ok(update_transaction::read(&data_dir)
        .map_err(transaction_error)?
        .is_some())
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
    Gate(GateError),
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
    NoJournal {
        id: String,
    },
    JournalMismatch {
        path: PathBuf,
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
    #[allow(dead_code)]
    Reverted {
        detail: String,
        outcome: RevertOutcome,
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
            Self::Gate(error) => write!(formatter, "{error}"),
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
            Self::NoJournal { id } => write!(formatter, "{id} has nothing to repair."),
            Self::JournalMismatch { path } => write!(
                formatter,
                "{} does not describe this installed app — refusing to repair it.",
                path.display()
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
            Self::Reverted { detail, outcome } if outcome.is_complete() => write!(
                formatter,
                "{detail} The installation was put back to its previous state."
            ),
            Self::Reverted { detail, outcome } => {
                write!(
                    formatter,
                    "{detail} The installation was not put back to its previous state. \
                     These revert steps failed:"
                )?;
                for failure in &outcome.failures {
                    write!(
                        formatter,
                        " {} at {}: {};",
                        failure.step,
                        failure.path.display(),
                        failure.detail
                    )?;
                }
                Ok(())
            }
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
            Self::Gate(error) => Some(error),
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

impl From<GateError> for UpdateError {
    fn from(error: GateError) -> Self {
        Self::Gate(error)
    }
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
