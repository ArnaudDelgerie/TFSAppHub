//! `tfsapp-hub remove <id> [--purge]` — undo an install.
//!
//! The split between the two forms is the package manager's, and it is
//! deliberate: **`remove` drops the app, `--purge` drops the app's data.**
//! Someone reinstalling a broken app must not lose their database to a command
//! that only had to replace a tree, and someone genuinely done with an app has
//! to be able to say so once and be done.
//!
//! That is a different split from the station's `--uninstall` / `--purge`,
//! where plain `--uninstall` already removes the data dirs and `--purge` only
//! adds the keyring. It has to be: over there the binary *is* the app, so there
//! was no "keep the data, drop the code" state to be in. Here there is, and it
//! is the common one.
//!
//! What each form touches, in the order it is printed and executed:
//!
//! | zone | `remove` | `--purge` |
//! |---|---|---|
//! | `apps/<id>/` — the installed snapshot | yes | yes |
//! | the registry entry | yes | yes |
//! | `TFSApp/<identifier>/` — the app's data dir | no | yes |
//! | `<identifier>/` — WebKit's own website data | no | yes |
//! | the OS keyring accounts under `<identifier>` | no | yes |
//!
//! Execution is best-effort per zone, as the station's is: a directory this
//! process cannot remove is reported and every other zone is still attempted.
//! Half a removal that says which half is far better than an abort that leaves
//! the user guessing what survived.
//!
//! **One zone does not live under the data root, and it caught us out.** Every
//! path above moves with `XDG_DATA_HOME`, so a `--purge` aimed at a throwaway
//! root touches nothing real — except the keyring, which is the login session's
//! and has no such knob. A `--purge` run against a test root still deletes the
//! *real* accounts of any app sharing that `identifier`, as one did while this
//! plan was being written. There is nothing to fix here: the accounts genuinely
//! belong to the app being purged, and both hosts key them the same way on
//! purpose (which is what makes a hubbed install find a packaged one's
//! `APP_SECRET`). It is a caveat for whoever tests this by hand — use an
//! identifier nothing else owns, or a private bus, the way
//! `build/scripts/run-tests.sh` does for the suite.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    manifest,
    paths::{Paths, PathsError},
    prompt,
    registry::{self, RegistryError},
};

/// The two accounts a purge owns whatever the manifest says: the one holding
/// `APP_SECRET` (CONTRACT.md §6), and the one the store writes and reads to find
/// out whether a keyring answers at all. Leaving either behind would have a
/// purged app still own an entry.
///
/// Imported from the module that writes them, now that plan 007 has brought it
/// over — 006 spelled them out here because there was nothing yet to import
/// from, and a duplicated account name is a purge that silently misses.
use crate::secrets::{APP_SECRET_ACCOUNT, PROBE_ACCOUNT};

/// The whole command. Returns the process's exit code.
pub fn run(id: &str, purge: bool, assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match remove(&paths, id, purge, assume_yes) {
        Ok(true) => EXIT_OK,
        // Declining is not a failure of the command, but it is not the outcome
        // asked for either: a script that runs `remove` and reads 0 would
        // conclude the app is gone.
        Ok(false) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// `false` when the user declined. Errors are the reasons removal could not be
/// attempted at all — a removal that partly failed is reported zone by zone and
/// still returns `Ok`.
fn remove(paths: &Paths, id: &str, purge: bool, assume_yes: bool) -> Result<bool, RemoveError> {
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| RemoveError::NotInstalled { id: id.to_string() })?;
    let identifier = entry.identifier.clone();

    let plan = plan(paths, id, &identifier, purge)?;

    // Only for `--purge`, and only because that is the form that deletes data:
    // a live sidecar has an open SQLite file under it. A read-only probe, so a
    // refusal never has the side effect of reaping anything.
    if purge {
        let pid_file = plan.data_dir.join("sidecar.pid");
        if tfsapp_core::process::is_owner_live(&pid_file).unwrap_or(false) {
            return Err(RemoveError::StillRunning { identifier });
        }
    }

    announce(&plan, purge);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was removed.");
        return Ok(false);
    }

    // The entry goes first, under the registry's own lock. An app whose files
    // are gone but which `list` still shows is the worse of the two orders:
    // every command afterwards points at a tree that is not there, and the id
    // cannot be reused.
    registry::update(paths, |registry| registry.remove(id))?;
    println!("  registry entry: removed");

    report("installed app", remove_directory(&plan.app_dir));
    if purge {
        report("data dir", remove_directory(&plan.data_dir));
        report("WebKit data dir", remove_directory(&plan.webkit_data_dir));
        for account in &plan.keyring_accounts {
            let removed = delete_keyring_account(&identifier, account);
            println!(
                "  keyring[{account}]: {}",
                match removed {
                    true => "removed",
                    false => "already clean",
                }
            );
        }
    }

    Ok(true)
}

/// Everything a given `remove` would touch — computed before anything is, so it
/// can be printed in full and confirmed as one decision.
struct RemovalPlan {
    id: String,
    identifier: String,
    app_dir: PathBuf,
    data_dir: PathBuf,
    /// `<OS data dir>/<identifier>/` — WebKitGTK's own per-identifier website
    /// data (CONTRACT.md §5/§6), a *sibling* of `TFSApp/` and not nested under
    /// it, which is exactly why it is easy to leave behind.
    webkit_data_dir: PathBuf,
    /// `app-secret`, the availability probe, then whatever the app declared
    /// under `actions.secrets.keys`. Empty unless purging.
    keyring_accounts: Vec<String>,
}

fn plan(
    paths: &Paths,
    id: &str,
    identifier: &str,
    purge: bool,
) -> Result<RemovalPlan, RemoveError> {
    let app_dir = paths.app_dir(id)?;

    let mut keyring_accounts = Vec::new();
    if purge {
        keyring_accounts.push(APP_SECRET_ACCOUNT.to_string());
        keyring_accounts.push(PROBE_ACCOUNT.to_string());
        // Read from the snapshot about to be deleted, and best-effort: an
        // unreadable manifest costs the declared keys, not the removal. The two
        // above are the hub's own and need no manifest to know about.
        if let Ok(loaded) = manifest::load(&app_dir) {
            keyring_accounts.extend(loaded.manifest.actions.secrets.keys);
        }
    }

    Ok(RemovalPlan {
        id: id.to_string(),
        app_dir,
        data_dir: paths.app_data_dir(identifier)?,
        webkit_data_dir: paths.webkit_data_dir(identifier)?,
        identifier: identifier.to_string(),
        keyring_accounts,
    })
}

/// Print what is about to happen, and — just as important — what is not.
fn announce(plan: &RemovalPlan, purge: bool) {
    println!("Remove {} ({}):", plan.id, plan.identifier);
    println!("  - {}", plan.app_dir.display());
    println!("  - its registry entry");

    match purge {
        true => {
            println!("  - {}", plan.data_dir.display());
            println!("  - {}", plan.webkit_data_dir.display());
            println!(
                "  - {} OS keyring account(s) under service \"{}\"",
                plan.keyring_accounts.len(),
                plan.identifier
            );
            println!();
            println!(
                "This deletes the app's database, sessions and secrets. Nothing here \
                 is recoverable, and reinstalling gives you an empty app."
            );
        }
        // Said out loud rather than left to be inferred: someone removing an
        // app to reinstall it needs to know their data is waiting, and someone
        // who meant to be rid of it needs to know it is not.
        false => {
            println!();
            println!(
                "Its data is kept: {}. Add --purge to remove that too.",
                plan.data_dir.display()
            );
        }
    }
}

/// An already-absent directory is a success, not an error: `remove` has to be
/// re-runnable after a partial failure without the second run reporting
/// problems the first one fixed.
fn remove_directory(path: &Path) -> Result<bool, std::io::Error> {
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(path).map(|()| true)
}

fn report(label: &str, outcome: Result<bool, std::io::Error>) {
    match outcome {
        Ok(true) => println!("  {label}: removed"),
        Ok(false) => println!("  {label}: already clean"),
        Err(error) => println!("  {label}: FAILED ({error})"),
    }
}

/// Delete one account from the OS keyring, keyed by the app's `identifier` as
/// the station's store keys it. `false` covers both "there was nothing" and "the
/// backend would not confirm", which are the same outcome here.
fn delete_keyring_account(identifier: &str, account: &str) -> bool {
    keyring::Entry::new(identifier, account)
        .map(|entry| entry.delete_credential().is_ok())
        .unwrap_or(false)
}

#[derive(Debug)]
pub enum RemoveError {
    Paths(PathsError),
    Registry(RegistryError),
    NotInstalled { id: String },
    StillRunning { identifier: String },
}

impl fmt::Display for RemoveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::NotInstalled { id } => write!(
                formatter,
                "no app is installed as {id} — `tfsapp-hub list` shows the ones that are."
            ),
            Self::StillRunning { identifier } => write!(
                formatter,
                "{identifier} is running — close it first. Purging would delete the \
                 database it has open."
            ),
        }
    }
}

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PathsError> for RemoveError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<RegistryError> for RemoveError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod tests;
