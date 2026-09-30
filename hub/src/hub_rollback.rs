//! `tfsapp-hub --rollback` — undo the last hub self-update
//! (`../plan/020-hub-self-update-and-revalidation.md`, step 7).
//!
//! The anchor `hub_update::run` leaves behind — `bin/tfsapp-hub.previous` and
//! `registry.json.previous`, both written before the swap they anchor
//! (Overview, corrections 2 and 3) — is what this module restores from, and
//! nothing else: no network call is on this path at all, which is why its
//! precondition ([`hub_update::anchor_state`]) is checked before anything
//! else, including the confirmation prompt.
//!
//! The anchor is consumed as soon as both halves are safely back: the binary
//! half by the very rename that restores it, the registry half by deleting
//! the snapshot once [`registry::restore_for_rollback`] has copied or merged it
//! back. `$APPIMAGE`
//! is swapped after that, exactly as `hub_update::run`'s own step 9 comes
//! after its anchor-consuming step 8 — a failure there is a named partial
//! failure with its own message, not a reason to leave the anchor in place
//! for a retry that would not use it anyway (correction 3).

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    hub_bin::{self, HubBinError},
    hub_update::{self, MissingAnchorHalf},
    paths::Paths,
    prompt,
    registry::{self, Registry, RegistryError},
};

/// `tfsapp-hub --rollback [--yes]` — resolve `Paths`, read `$APPIMAGE`, and
/// turn the pipeline's outcome into an exit code.
pub fn run(assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let appimage_env = std::env::var("APPIMAGE").ok();

    match rollback(&paths, appimage_env.as_deref(), assume_yes) {
        Ok(outcome) if outcome.completed => EXIT_OK,
        // Declining is not a failure of the command, but nothing changed
        // either — a script reading 0 would conclude it did.
        Ok(_) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// [`run`]'s pipeline, minus resolving `Paths`/`$APPIMAGE` — the seam
/// `hub_rollback_tests.rs` uses to run a whole `--rollback` against a
/// throwaway root, matching `hub_update::update`'s own shape. Its outcome
/// records whether the user declined and which entries the locked merge kept.
pub(crate) fn rollback(
    paths: &Paths,
    appimage_env: Option<&str>,
    assume_yes: bool,
) -> Result<RollbackResult, HubRollbackError> {
    rollback_after_binary(paths, appimage_env, assume_yes, |_| {})
}

/// The rollback pipeline with a test seam immediately after the previous
/// binary has been put back. Production passes a no-op; the seam lets the
/// harness exercise the one partial state that only exists after that rename.
fn rollback_after_binary(
    paths: &Paths,
    appimage_env: Option<&str>,
    assume_yes: bool,
    after_binary: impl FnOnce(&Paths),
) -> Result<RollbackResult, HubRollbackError> {
    let anchor_binary = hub_bin::anchor_path(paths);
    let anchor_registry = hub_bin::anchor_registry_path(paths);

    // The precondition first, and nothing else before it: no network is on
    // this path at all, and a half-present anchor must never look like a
    // confirmation prompt away from working.
    hub_update::anchor_state(&anchor_binary, &anchor_registry)
        .map_err(|missing| HubRollbackError::NoAnchor { missing })?;

    // The snapshot was written by the hub being restored to, before the
    // update that replaced it — its own `hub_version` is the version this
    // rollback is restoring. This read validates the anchor before the binary
    // rename and provides the prompt's version; it never decides what live app
    // state will be written back.
    let snapshot = registry::load_from(&anchor_registry).map_err(HubRollbackError::Registry)?;
    let restoring_to = snapshot.hub_version.clone();

    announce(restoring_to.as_deref());
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(RollbackResult::declined());
    }

    let stable_path = paths.hub_executable_path();
    fs::rename(&anchor_binary, &stable_path).map_err(|source| HubRollbackError::Io {
        path: stable_path.clone(),
        source,
    })?;

    after_binary(paths);

    let kept_entries =
        match registry::restore_for_rollback(paths, &anchor_registry).map_err(|source| {
            HubRollbackError::RegistryRestoreFailed {
                snapshot_path: anchor_registry.clone(),
                source,
            }
        })? {
            registry::RollbackRestoreOutcome::RestoredBytes => Vec::new(),
            registry::RollbackRestoreOutcome::Merged { live } => kept_entries(&live, &snapshot),
        };

    // Both halves are safely back — the anchor is consumed here, before the
    // $APPIMAGE swap below, which is best-effort from this point on and not
    // what a second `--rollback` would need anyway.
    let _ = fs::remove_file(&anchor_registry);

    let missing_appimage = restore_appimage(&stable_path, appimage_env)?;

    report(
        restoring_to.as_deref(),
        missing_appimage.as_deref(),
        &kept_entries,
    );
    Ok(RollbackResult {
        completed: true,
        kept_entries,
    })
}

#[derive(Debug, PartialEq)]
pub(crate) struct RollbackResult {
    completed: bool,
    kept_entries: Vec<String>,
}

impl RollbackResult {
    fn declined() -> Self {
        Self {
            completed: false,
            kept_entries: Vec::new(),
        }
    }
}

/// Overwrite `$APPIMAGE` with the just-restored stable copy — unless it names
/// the stable copy itself (nothing to do, `hub_update::same_file`'s own
/// question asked in reverse), is unset (a `.desktop`-launched stable copy
/// has no `$APPIMAGE` in its environment at all, and that is not a reason to
/// refuse a rollback that needs no network to finish), or no longer exists on
/// disk (the user's own download, deleted since — said on screen, never a
/// failure). Returns the path when it was found missing, so [`report`] can
/// name it.
fn restore_appimage(
    stable_path: &Path,
    appimage_env: Option<&str>,
) -> Result<Option<PathBuf>, HubRollbackError> {
    let Some(appimage_path) = hub_update::resolve_appimage_target(appimage_env) else {
        return Ok(None);
    };
    if hub_update::same_file(stable_path, &appimage_path) {
        return Ok(None);
    }
    if !appimage_path.is_file() {
        return Ok(Some(appimage_path));
    }

    hub_bin::ensure_current_at(stable_path, &appimage_path).map_err(|error| {
        HubRollbackError::AppimageSwapFailed {
            appimage_path: appimage_path.clone(),
            source: error,
        }
    })?;
    Ok(None)
}

/// The `hub_version` recorded in the anchor's registry snapshot — the version
/// this rollback is restoring, since that snapshot was written by the hub
/// being rolled back to, before the update that replaced it. `None` keeps the
/// report honest instead of guessing: an unreadable or malformed snapshot, or
/// one old enough to predate `hub_version` itself.
fn kept_entries(live: &Registry, snapshot: &Registry) -> Vec<String> {
    let mut kept = Vec::new();
    for entry in &live.apps {
        match snapshot.get(&entry.id) {
            None => kept.push(format!("{} (installed since update)", entry.id)),
            Some(previous) if previous != entry => kept.push(format!(
                "{} (recorded state changed since update)",
                entry.id
            )),
            Some(_) => {}
        }
    }
    for entry in &snapshot.apps {
        if live.get(&entry.id).is_none() {
            kept.push(format!("{} (removed since update)", entry.id));
        }
    }
    kept
}

fn announce(restoring_to: Option<&str>) {
    match restoring_to {
        Some(version) => println!("Roll back to hub {version}."),
        None => println!("Roll back to the previous hub."),
    }
    println!(
        "  every generated launcher, and the file you downloaded if it is still there, go \
         back to that binary; the hub stamp goes back with it, while installed apps keep their \
         current recorded states if they changed since the update. Offline — no \
         network call is made on this path."
    );
}

fn report(restoring_to: Option<&str>, missing_appimage: Option<&Path>, kept_entries: &[String]) {
    match restoring_to {
        Some(version) => println!("Rolled back to hub {version}."),
        None => println!("Rolled back to the previous hub."),
    }
    if kept_entries.is_empty() {
        println!("Installed apps' recorded states came back with it.");
    } else {
        println!("Kept installed apps' recorded states that changed since the update:");
        for entry in kept_entries {
            println!("  {entry}");
        }
    }
    if let Some(path) = missing_appimage {
        println!(
            "  {} no longer exists — nothing to restore there, but every generated launcher \
             already runs the restored hub.",
            path.display()
        );
    }
}

/// Why [`rollback`] could not finish.
#[derive(Debug)]
pub enum HubRollbackError {
    Registry(RegistryError),
    /// The previous binary is already back, but restoring the registry after
    /// that irreversible anchor-consuming rename failed.
    RegistryRestoreFailed {
        snapshot_path: PathBuf,
        source: RegistryError,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// The rollback anchor is not complete — one or both halves are missing,
    /// named by [`MissingAnchorHalf`].
    NoAnchor {
        missing: MissingAnchorHalf,
    },
    /// The binary and the registry were already restored — every app
    /// launcher already runs the restored hub — but writing it over
    /// `$APPIMAGE` failed. `hub_update::HubUpdateError::AppimageSwapFailed`'s
    /// own named partial failure, run in reverse.
    AppimageSwapFailed {
        appimage_path: PathBuf,
        source: HubBinError,
    },
}

impl fmt::Display for HubRollbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::RegistryRestoreFailed {
                snapshot_path,
                source,
            } => write!(
                formatter,
                "the previous hub binary is back — every generated launcher already runs it — \
                 but the registry was not restored: {source}. The registry snapshot remains at \
                 {}; restore it there by hand.",
                snapshot_path.display()
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::NoAnchor { missing } => write!(
                formatter,
                "nothing was rolled back — {missing} missing. Either the hub was never \
                 updated, or a previous rollback already consumed its anchor."
            ),
            Self::AppimageSwapFailed {
                appimage_path,
                source,
            } => write!(
                formatter,
                "the hub was rolled back — every app launcher already runs the restored \
                 version — but {} could not be replaced: {source}. Replace that file by hand \
                 with the previous release's .AppImage.",
                appimage_path.display()
            ),
        }
    }
}

impl std::error::Error for HubRollbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::RegistryRestoreFailed { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::NoAnchor { .. } => None,
            Self::AppimageSwapFailed { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
#[path = "hub_rollback_tests.rs"]
mod tests;
