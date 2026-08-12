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
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK, EXIT_UNIMPLEMENTED},
    desktop::{self, RemovalOutcome},
    lifecycle, manifest,
    paths::{self, Paths, PathsError},
    prompt,
    registry::{self, Registry, RegistryError},
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
    // `apps/<id>.previous` — the rollback anchor's tree half, if `id` ever
    // had one — goes with it. Silent and best-effort: it is bookkeeping the
    // anchor owns, not a zone this command's own announcement promises, and
    // an already-absent one is exactly the common case.
    crate::update::discard_tree(&plan.app_dir);
    // Code-side, not data — the entry points at a snapshot that is being
    // deleted — so plain `remove` takes it exactly as it takes `apps/<id>/`;
    // `--purge` adds nothing here. The stable hub copy the entry's `Exec=`
    // points at is left alone: it is the hub's own file, and another
    // installed app's launcher points at it.
    report(
        "desktop entry",
        desktop::remove(&plan.id, &plan.identifier, paths).map(desktop_removal_status),
    );
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

/// `tfsapp-hub purge [<identifier>] [--yes]` — resolve `Paths` and either list
/// what is purgeable (`identifier` is `None`) or purge one.
///
/// The two forms share one grammar line (`cli::SURFACE`'s `purge` row) because
/// they share one subject, an `identifier` with no registry entry — but only
/// the listing form is wired up here. `purge <identifier>` itself lands in
/// this plan's step 3; until then the grammar already accepts it and this says
/// so, the same "recognised but not implemented yet" vocabulary `dispatch`
/// uses at the whole-command level.
pub fn purge(identifier: Option<&str>, _assume_yes: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match identifier {
        None => match list_orphaned_data(&paths) {
            Ok(text) => {
                print!("{text}");
                EXIT_OK
            }
            Err(error) => {
                eprintln!("tfsapp-hub: {error}");
                EXIT_FAILED
            }
        },
        Some(_) => {
            eprintln!("tfsapp-hub: purge <identifier> is recognised but not implemented yet.");
            EXIT_UNIMPLEMENTED
        }
    }
}

/// Load the registry and render bare `purge`'s whole output.
fn list_orphaned_data(paths: &Paths) -> Result<String, RemoveError> {
    let registry = registry::load(paths)?;
    let orphans = orphaned_data(paths, &registry)?;
    Ok(render_orphans(&orphans))
}

/// One identifier under `TFSApp/` that has data but no registered app — what
/// bare `purge` lists, and what `purge <identifier>` (step 3) refuses unless
/// it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedData {
    pub identifier: String,
    /// [`lifecycle::read_data_version`]'s own answer for this identifier's
    /// `data/` subdir. `None` covers both "no record" and "unreadable
    /// record" — a broken one must not hide the directory it belongs to.
    pub app_version: Option<String>,
    /// The sum of file lengths over a recursive walk — an order of magnitude
    /// to decide with, not accounting. Deliberately not `st_blocks`; do not
    /// "fix" it into one.
    pub size_bytes: u64,
    pub has_webkit_data: bool,
}

/// Every identifier under `TFSApp/` with data but no installed app.
///
/// Candidates are read from `TFSApp/` only, never from the OS data dir
/// directly — the existence of `TFSApp/<identifier>/` is what *proves* an
/// identifier is ours (see the plan's Overview). `paths::HUB_DIR` is excluded
/// by name, a symlink or a plain file is skipped rather than refused (only a
/// real directory counts as a candidate), and anything the registry still
/// claims is left out.
pub fn orphaned_data(paths: &Paths, registry: &Registry) -> Result<Vec<OrphanedData>, RemoveError> {
    let vendor_dir = paths.vendor_dir();
    let entries = match fs::read_dir(&vendor_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(RemoveError::Io {
                path: vendor_dir,
                source,
            })
        }
    };

    let mut orphans = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RemoveError::Io {
            path: vendor_dir.clone(),
            source,
        })?;
        let name = entry.file_name();
        let Some(identifier) = name.to_str() else {
            continue;
        };
        if identifier == paths::HUB_DIR {
            continue;
        }

        // `symlink_metadata`, never `metadata`: a symlink must be proven or
        // skipped here, not followed — the same rule the purge itself follows
        // for this same directory.
        let Ok(metadata) = entry.path().symlink_metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        if registry.by_identifier(identifier).is_some() {
            continue;
        }

        let data_subdir = entry.path().join("data");
        let app_version = lifecycle::read_data_version(&data_subdir).unwrap_or(None);
        let size_bytes = directory_size(&entry.path());
        let has_webkit_data = paths
            .webkit_data_dir(identifier)
            .map(|dir| dir.is_dir())
            .unwrap_or(false);

        orphans.push(OrphanedData {
            identifier: identifier.to_string(),
            app_version,
            size_bytes,
            has_webkit_data,
        });
    }

    // Deterministic, for a predictable listing and a predictable test.
    orphans.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    Ok(orphans)
}

/// The sum of file lengths under `path`, recursively. Best-effort: a subtree
/// this process cannot read contributes nothing rather than failing the whole
/// listing — the size is an estimate, never the reason a purge is refused.
fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => directory_size(&entry.path()),
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Bare `purge`'s whole output — pure, so the "nothing to purge" line and the
/// column layout are testable without a filesystem, matching `list::render`.
fn render_orphans(orphans: &[OrphanedData]) -> String {
    if orphans.is_empty() {
        return "nothing to purge\n".to_string();
    }

    let mut text = String::new();
    for orphan in orphans {
        let version = orphan
            .app_version
            .as_deref()
            .unwrap_or("no version recorded");
        let webkit = match orphan.has_webkit_data {
            true => "with WebKit data",
            false => "no WebKit data",
        };
        text.push_str(&format!(
            "{}  {version}  {}  {webkit}\n",
            orphan.identifier,
            human_size(orphan.size_bytes),
        ));
    }
    text
}

/// `size_bytes` as something a human can eyeball — a plausible order of
/// magnitude, not an exact figure (see [`OrphanedData::size_bytes`]).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ => format!("{size:.1} {}", UNITS[unit]),
    }
}

/// Everything a given `remove` would touch — computed before anything is, so it
/// can be printed in full and confirmed as one decision.
struct RemovalPlan {
    id: String,
    identifier: String,
    app_dir: PathBuf,
    /// The `.desktop` entry `install` may have written — code-side, taken by
    /// plain `remove` exactly as `app_dir` is, whether or not this app
    /// actually has one or the hub wrote it. [`desktop::remove`] is what
    /// decides that at execution time; announcing the path ahead of it is not
    /// a promise, only where the hub will look.
    desktop_entry: PathBuf,
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
        desktop_entry: paths.desktop_entry_path(identifier)?,
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
    println!("  - {} (if the hub wrote it)", plan.desktop_entry.display());

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
fn remove_directory(path: &Path) -> Result<&'static str, std::io::Error> {
    if !path.exists() {
        return Ok("already clean");
    }
    fs::remove_dir_all(path)?;
    Ok("removed")
}

/// [`desktop::RemovalOutcome`] in the same one-word-or-so vocabulary
/// [`remove_directory`]'s outcomes are reported in, plus the third state a
/// plain directory removal has no equivalent for: a file at the path that
/// this hub did not write.
fn desktop_removal_status(outcome: RemovalOutcome) -> &'static str {
    match outcome {
        RemovalOutcome::Removed => "removed",
        RemovalOutcome::Absent => "already clean",
        RemovalOutcome::LeftAlone => "left alone (not written by tfsapp-hub)",
    }
}

/// Shared by every zone a removal touches, `desktop::remove`'s `DesktopError`
/// included — generic over the error type rather than tied to `io::Error`, so
/// the desktop entry's own error type reports through the same line as
/// everything else.
fn report<E: fmt::Display>(label: &str, outcome: Result<&'static str, E>) {
    match outcome {
        Ok(status) => println!("  {label}: {status}"),
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
    NotInstalled {
        id: String,
    },
    StillRunning {
        identifier: String,
    },
    /// Reading `TFSApp/` to enumerate orphaned data — [`orphaned_data`]'s own
    /// I/O failure, distinct from a `Paths` resolution failure.
    Io {
        path: PathBuf,
        source: io::Error,
    },
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
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Io { source, .. } => Some(source),
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
