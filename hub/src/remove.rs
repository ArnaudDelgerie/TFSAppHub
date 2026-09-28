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

use serde::{Deserialize, Serialize};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    desktop::{self, RemovalOutcome},
    lifecycle,
    lifecycle_gate::{self, GateError},
    manifest,
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
use crate::secrets::{self, APP_SECRET_ACCOUNT, PROBE_ACCOUNT};

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
    let _maintenance = lifecycle_gate::acquire_maintenance(paths, &identifier, "remove")?;
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| RemoveError::NotInstalled { id: id.to_string() })?;
    let identifier = entry.identifier.clone();

    let removal_plan = plan(paths, id, &identifier)?;

    // Both forms remove the snapshot that a live window is serving, and a
    // `--purge` also deletes its data: a live window has an open SQLite file
    // under it, and an active `run` command is reading or writing the same
    // directory. Shared with
    // `export`/`import` (plan 022) and, from plan 023 step 3, `purge
    // <identifier>` — one definition of "who holds this data directory",
    // never a second probe with its own idea of it. A read-only probe, so a
    // refusal never has the side effect of reaping anything.
    match busy_holder(&removal_plan.data_dir, &identifier) {
        Ok(None) => {}
        Ok(Some(holder)) => {
            return Err(RemoveError::StillRunning {
                id: Some(id.to_string()),
                identifier: identifier.clone(),
                holder,
            })
        }
        Err(source) => {
            return Err(RemoveError::Io {
                path: removal_plan.data_dir.clone(),
                source,
            })
        }
    }

    announce(&removal_plan, purge);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was removed.");
        return Ok(false);
    }

    execute(paths, &removal_plan, purge)?;
    Ok(true)
}

/// Whether something already holds `data_dir` — a live window, or an active
/// `run` command. Mirrors `portability::busy_holder`'s own guard:
/// [`lifecycle::data_dir_holder`] has no opinion about a missing directory
/// (scanning `runs/` inside one would just find it absent), and a data
/// directory that has never been written to is not busy.
fn busy_holder(data_dir: &Path, identifier: &str) -> io::Result<Option<lifecycle::DataDirHolder>> {
    if !data_dir.is_dir() {
        return Ok(None);
    }
    lifecycle::data_dir_holder(data_dir, identifier)
}

/// Delete every zone `plan` carries: the registry entry and the code-side
/// zones only for an installed subject (`plan.id`/`plan.app_dir` are
/// `Some`), the identifier-keyed zones always, and the keyring accounts only
/// when `purge` is true. The shared execution path behind both of this
/// module's entry points — `remove <id> [--purge]` today, and (plan 023 step
/// 3) `purge <identifier>`'s orphan subject.
fn execute(paths: &Paths, plan: &RemovalPlan, purge: bool) -> Result<(), RemoveError> {
    execute_with_keyring(paths, plan, purge, delete_keyring_account)
}

fn execute_with_keyring<F>(
    paths: &Paths,
    plan: &RemovalPlan,
    purge: bool,
    delete_keyring_account: F,
) -> Result<(), RemoveError>
where
    F: Fn(&str, &str) -> Result<&'static str, crate::secrets::StorageError>,
{
    if let Some(id) = &plan.id {
        // The entry goes first, under the registry's own lock. An app whose
        // files are gone but which `list` still shows is the worse of the two
        // orders: every command afterwards points at a tree that is not
        // there, and the id cannot be reused.
        registry::update(paths, |registry| registry.remove(id))?;
        println!("  registry entry: removed");
    }

    if let Some(app_dir) = &plan.app_dir {
        report("installed app", remove_directory(app_dir));
        // `apps/<id>.previous` — the rollback anchor's tree half, if `id`
        // ever had one — goes with it. Silent and best-effort: it is
        // bookkeeping the anchor owns, not a zone this command's own
        // announcement promises, and an already-absent one is exactly the
        // common case.
        crate::update::discard_tree(app_dir);
    }

    // Code-side, not data, for an installed subject: the entry points at a
    // snapshot that is being deleted, so plain `remove` takes it exactly as
    // it takes `apps/<id>/`. The orphan subject has no `id` left to verify
    // the marker against, so it takes any entry the identifier's own
    // filename carries a marker at all (`desktop::remove_any`). The stable
    // hub copy the entry's `Exec=` points at is left alone either way: it is
    // the hub's own file, and another installed app's launcher points at it.
    report(
        "desktop entry",
        match &plan.id {
            Some(id) => desktop::remove(id, &plan.identifier, paths).map(desktop_removal_status),
            None => desktop::remove_any(&plan.identifier, paths).map(desktop_removal_status),
        },
    );

    if purge {
        report("data dir", remove_directory(&plan.data_dir));
        report("WebKit data dir", remove_directory(&plan.webkit_data_dir));
        for account in &plan.keyring_accounts {
            let outcome = delete_keyring_account(&plan.identifier, account);
            report(&format!("keyring[{account}]"), outcome);
        }
    } else {
        // Only reached by a retaining `remove` (plan_orphan's own subject
        // always executes with `purge` true) — the note the data directory
        // it keeps needs for a later `purge <identifier>` to reach the
        // accounts this manifest declared. Best-effort and silent: a failed
        // note costs that later purge some completeness, never this
        // removal.
        let _ = write_keyring_note(&plan.data_dir, &plan.keyring_accounts);
    }

    Ok(())
}

/// `tfsapp-hub purge [<identifier>] [--yes]` — resolve `Paths` and either list
/// what is purgeable (`identifier` is `None`) or purge one.
///
/// The two forms share one grammar line (`cli::SURFACE`'s `purge` row)
/// because they share one subject, an `identifier` with no registry entry.
pub fn purge(identifier: Option<&str>, assume_yes: bool) -> i32 {
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
        Some(identifier) => match purge_identifier(&paths, identifier, assume_yes) {
            Ok(true) => EXIT_OK,
            // Same reasoning as `remove`'s own decline: a script reading 0
            // must not conclude the data is gone.
            Ok(false) => EXIT_FAILED,
            Err(error) => {
                eprintln!("tfsapp-hub: {error}");
                EXIT_FAILED
            }
        },
    }
}

/// Load the registry and render bare `purge`'s whole output.
fn list_orphaned_data(paths: &Paths) -> Result<String, RemoveError> {
    let registry = registry::load(paths)?;
    let orphans = orphaned_data(paths, &registry)?;
    Ok(render_orphans(&orphans))
}

/// `purge <identifier>` itself: the refusals in the fixed order the plan's
/// Overview lays out, then the same announce/confirm/execute shape
/// `remove --purge` uses — [`execute`] is the one execution path both entry
/// points share.
///
/// `false` when the user declined, matching [`remove`]'s own contract.
fn purge_identifier(
    paths: &Paths,
    identifier: &str,
    assume_yes: bool,
) -> Result<bool, RemoveError> {
    purge_identifier_with_keyring(paths, identifier, assume_yes, delete_keyring_account)
}

fn purge_identifier_with_keyring<F>(
    paths: &Paths,
    identifier: &str,
    assume_yes: bool,
    delete_keyring_account: F,
) -> Result<bool, RemoveError>
where
    F: Fn(&str, &str) -> Result<&'static str, crate::secrets::StorageError>,
{
    // Refusal 0: these names identify infrastructure, never orphaned app data.
    if paths::is_reserved_identifier(identifier) {
        return Err(RemoveError::ReservedIdentifier {
            identifier: identifier.to_string(),
        });
    }

    let _maintenance = lifecycle_gate::acquire_maintenance(paths, identifier, "purge")?;

    // Refusal 1: a registry entry still claims this identifier. Purging
    // under a live install would delete data `remove <id> --purge` is the
    // one command meant to reach.
    let registry = registry::load(paths)?;
    if let Some(entry) = registry.by_identifier(identifier) {
        return Err(RemoveError::AlreadyInstalled {
            id: entry.id.clone(),
            identifier: identifier.to_string(),
        });
    }

    // Refusals 2 and 3 both read `TFSApp/<identifier>/` once, with
    // `symlink_metadata` — never `metadata` — so a symlink is proven rather
    // than followed. Nothing there at all (refusal 2) and a symlink in its
    // place (refusal 3) are the two ways this path is not a real data
    // directory this command may act on; anything else not a directory (a
    // plain file, say) falls into the same "nothing to purge" refusal as a
    // missing path, exactly as the enumeration behind bare `purge` skips it.
    let data_dir = paths.app_data_dir(identifier)?;
    match fs::symlink_metadata(&data_dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(RemoveError::NoOrphanData {
                identifier: identifier.to_string(),
            });
        }
        Err(source) => {
            return Err(RemoveError::Io {
                path: data_dir,
                source,
            });
        }
        Ok(metadata) if metadata.is_symlink() => {
            return Err(RemoveError::SymlinkData {
                target: fs::read_link(&data_dir).ok(),
                identifier: identifier.to_string(),
                path: data_dir,
            });
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(RemoveError::NoOrphanData {
                identifier: identifier.to_string(),
            });
        }
    }

    // Refusal 4: the same shared guard `remove --purge` now goes through
    // too (step 2) — a live window or an active `run` command holds this
    // data directory.
    match busy_holder(&data_dir, identifier) {
        Ok(None) => {}
        Ok(Some(holder)) => {
            return Err(RemoveError::StillRunning {
                id: None,
                identifier: identifier.to_string(),
                holder,
            })
        }
        Err(source) => {
            return Err(RemoveError::Io {
                path: data_dir,
                source,
            })
        }
    }

    let removal_plan = plan_orphan(paths, identifier)?;

    announce(&removal_plan, true);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was purged.");
        return Ok(false);
    }

    execute_with_keyring(paths, &removal_plan, true, delete_keyring_account)?;
    Ok(true)
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

/// Everything a given removal would touch — computed before anything is, so
/// it can be printed in full and confirmed as one decision.
///
/// `id` and `app_dir` are the code-side zones: real for the installed
/// subject `remove <id> [--purge]` builds today (via [`plan`]), `None` for
/// the orphan subject `purge <identifier>` (plan 023 step 3) builds instead —
/// there is no hub-local `id` left once an app has been removed, so there is
/// nothing code-side to plan for. The rest is keyed on `identifier` alone,
/// and every subject carries it.
struct RemovalPlan {
    id: Option<String>,
    identifier: String,
    app_dir: Option<PathBuf>,
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
    /// under `actions.secrets.keys`. Computed for both forms now (plan 023
    /// step 4): the installed subject reads it from its live manifest via
    /// [`plan`] whether or not `purge` is set, since a plain `remove` needs
    /// the full set to write into the note it leaves behind; the orphan
    /// subject reads it from that note via [`plan_orphan`], or falls back to
    /// the two hub accounts when there is none.
    keyring_accounts: Vec<String>,
    /// Whether `keyring_accounts` is a set this hub can vouch for in full —
    /// the installed subject's live manifest, or the orphan subject's found
    /// note — rather than the two-account fallback with nothing behind it.
    /// `announce`'s "declared secrets may remain" caveat is gated on this,
    /// not on `id`, so a purge with a note stops naming a limit that no
    /// longer applies to it.
    keyring_note_found: bool,
}

fn plan(paths: &Paths, id: &str, identifier: &str) -> Result<RemovalPlan, RemoveError> {
    let app_dir = paths.app_dir(id)?;

    let mut keyring_accounts = vec![APP_SECRET_ACCOUNT.to_string(), PROBE_ACCOUNT.to_string()];
    // Read from the snapshot, and best-effort: an unreadable manifest costs
    // the declared keys, not the removal. The two above are the hub's own
    // and need no manifest to know about.
    let keyring_note_found = match manifest::load(&app_dir) {
        Ok(loaded) => {
            keyring_accounts.extend(loaded.manifest.actions.secrets.keys);
            true
        }
        Err(_) => false,
    };

    Ok(RemovalPlan {
        id: Some(id.to_string()),
        app_dir: Some(app_dir),
        desktop_entry: paths.desktop_entry_path(identifier)?,
        data_dir: paths.app_data_dir(identifier)?,
        webkit_data_dir: paths.webkit_data_dir(identifier)?,
        identifier: identifier.to_string(),
        keyring_accounts,
        keyring_note_found,
    })
}

/// The orphan subject `purge <identifier>` builds once its refusals have
/// passed: no `id`, no `app_dir`, and the keyring zone read from the note a
/// retaining `remove` may have left behind (plan 023 step 4), unioned with
/// [`secrets::RESERVED_SECRET_KEYS`] and deduplicated. No note (an older
/// hub's leftover, or a packaged station app's data, which never writes one)
/// falls back to the two hub-owned accounts alone.
fn plan_orphan(paths: &Paths, identifier: &str) -> Result<RemovalPlan, RemoveError> {
    let data_dir = paths.app_data_dir(identifier)?;
    let note = read_keyring_note(&data_dir);
    let keyring_note_found = note.is_some();

    let mut keyring_accounts = note.unwrap_or_default();
    for &reserved in secrets::RESERVED_SECRET_KEYS {
        if !keyring_accounts.iter().any(|account| account == reserved) {
            keyring_accounts.push(reserved.to_string());
        }
    }

    Ok(RemovalPlan {
        id: None,
        app_dir: None,
        desktop_entry: paths.desktop_entry_path(identifier)?,
        webkit_data_dir: paths.webkit_data_dir(identifier)?,
        identifier: identifier.to_string(),
        data_dir,
        keyring_accounts,
        keyring_note_found,
    })
}

/// `<data_dir>/data/keyring.json` — the note a retaining `remove` leaves
/// behind for a later `purge <identifier>`: the OS keyring accounts a
/// manifest declared, read while `plan` still has the snapshot that declares
/// them — by the time a purge runs, that snapshot is gone (the plan's
/// keyring design decision).
///
/// Not `data/config.json`, which is CONTRACT.md §6's contract surface shared
/// with the station: this note exists purely so *this* hub can `purge` what
/// *this* hub's `remove` retained. It lands inside `data/`, so `export`'s
/// curated set (`lifecycle::DB_FILE_NAMES`, nothing else) leaves it out by
/// construction.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
struct KeyringNote {
    accounts: Vec<String>,
    /// Keys a newer hub wrote and this one does not know — same rule as
    /// `registry::Registry::unknown`.
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

fn keyring_note_path(data_dir: &Path) -> PathBuf {
    data_dir.join("data").join("keyring.json")
}

/// Write `accounts` into `data_dir`'s note, atomically — same same-directory
/// temp-file-plus-`rename` move as `lifecycle::write_data_version`. Every
/// caller treats a failure here as non-fatal (see [`execute`]'s own comment).
fn write_keyring_note(data_dir: &Path, accounts: &[String]) -> io::Result<()> {
    let note = KeyringNote {
        accounts: accounts.to_vec(),
        unknown: serde_json::Map::new(),
    };
    let json = serde_json::to_string_pretty(&note).map_err(io::Error::other)?;

    let path = keyring_note_path(data_dir);
    let temporary = data_dir.join("data").join("keyring.json.tmp");
    fs::write(&temporary, json)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

/// Read `data_dir`'s note back, if there is one to read. `None` covers both
/// "no note" (an older hub's leftover, or a packaged station app's data
/// directory, neither of which ever writes one) and "a note that does not
/// parse" — [`plan_orphan`]'s honest fallback either way, never a reason to
/// refuse the purge itself.
fn read_keyring_note(data_dir: &Path) -> Option<Vec<String>> {
    let contents = fs::read_to_string(keyring_note_path(data_dir)).ok()?;
    serde_json::from_str::<KeyringNote>(&contents)
        .ok()
        .map(|note| note.accounts)
}

/// Print what is about to happen, and — just as important — what is not.
fn announce(plan: &RemovalPlan, purge: bool) {
    match &plan.id {
        Some(id) => println!("Remove {} ({}):", id, plan.identifier),
        // Reached from plan 023 step 3 onward, once `purge <identifier>`
        // builds an orphan plan — never today, since [`plan`] always fills
        // `id` in.
        None => println!("Purge {}:", plan.identifier),
    }
    if let Some(app_dir) = &plan.app_dir {
        println!("  - {}", app_dir.display());
        println!("  - its registry entry");
    }
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
            // An orphan subject needs the note a retaining `remove` left
            // behind (plan 023 step 4), while an installed subject needs its
            // live manifest, to know the full declared set. If either is
            // absent, only the two hub-owned accounts above are known; say
            // the limit out loud rather than silently under-purging.
            if !plan.keyring_note_found {
                println!(
                    "This identifier's data carries no note of declared secret keys (an older \
                     hub's leftover, or a packaged station app's data) — only the two accounts \
                     above are removed; anything the app itself declared under actions.secrets \
                     may remain."
                );
            }
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
/// the station's store keys it. `Ok("already clean")` is the keyring's plain
/// absence; any other failure — a backend that refuses, or one that does not
/// answer within the deadline — is an `Err`, which `report` prints as
/// `FAILED (<cause>)` exactly like a directory that cannot be removed. The
/// command's own outcome is unchanged either way.
fn delete_keyring_account(
    identifier: &str,
    account: &str,
) -> Result<&'static str, crate::secrets::StorageError> {
    let service = identifier.to_string();
    let account = account.to_string();
    match crate::secrets::with_deadline(crate::secrets::KEYRING_DEADLINE, move || {
        keyring::Entry::new(&service, &account).and_then(|entry| entry.delete_credential())
    }) {
        Some(Ok(())) => Ok("removed"),
        Some(Err(keyring::Error::NoEntry)) => Ok("already clean"),
        Some(Err(error)) => Err(crate::secrets::StorageError::Keyring(error)),
        None => Err(crate::secrets::StorageError::TimedOut),
    }
}

#[derive(Debug)]
pub enum RemoveError {
    Paths(PathsError),
    Registry(RegistryError),
    Gate(GateError),
    /// `purge <identifier>` named a directory owned by the hub or desktop environment.
    ReservedIdentifier {
        identifier: String,
    },
    NotInstalled {
        id: String,
    },
    /// A live window or an active `run` command holds the data directory —
    /// refused rather than deleting a database out from under it. `id` is
    /// `None` for an orphan subject (`purge <identifier>`), which has no
    /// hub-local id to suggest a `run --stop` command with.
    StillRunning {
        id: Option<String>,
        identifier: String,
        holder: lifecycle::DataDirHolder,
    },
    /// `purge <identifier>` named an identifier a registry entry still
    /// claims — `remove <id> --purge` is the way to delete an installed
    /// app's data, never this command.
    AlreadyInstalled {
        id: String,
        identifier: String,
    },
    /// `purge <identifier>` found nothing under `TFSApp/<identifier>/` — the
    /// guard that also stops a typo from ever reaching the WebKit sibling
    /// (see the plan's Overview).
    NoOrphanData {
        identifier: String,
    },
    /// `TFSApp/<identifier>/` is a symlink — refused rather than followed
    /// (see the plan's Overview design decision).
    SymlinkData {
        identifier: String,
        path: PathBuf,
        target: Option<PathBuf>,
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
            Self::Gate(error) => write!(formatter, "{error}"),
            Self::ReservedIdentifier { identifier } => match identifier.as_str() {
                "hub" => write!(
                    formatter,
                    "hub is the hub's own directory under TFSApp/ and cannot be purged."
                ),
                "TFSApp" => write!(
                    formatter,
                    "TFSApp is the shared vendor directory and cannot be purged."
                ),
                "applications" => write!(
                    formatter,
                    "applications is the XDG desktop-entry directory and cannot be purged."
                ),
                _ => unreachable!("only reserved identifiers construct this error"),
            },
            Self::NotInstalled { id } => write!(
                formatter,
                "no app is installed as {id} — `tfsapp-hub list` shows the ones that are."
            ),
            Self::StillRunning {
                id,
                identifier,
                holder,
            } => {
                let name = id.as_deref().unwrap_or(identifier);
                let stop = match id {
                    Some(id) => format!(" Stop it first with `tfsapp-hub run --stop {id}`."),
                    None => " Stop it first.".to_string(),
                };
                match holder {
                    lifecycle::DataDirHolder::Window => write!(
                        formatter,
                        "{name} has a window open right now — purging would delete the \
                         database it has open. Close {name} first."
                    ),
                    lifecycle::DataDirHolder::RunCommand { active } => {
                        match active.first().and_then(|run| run.alias.as_deref()) {
                            Some(alias) => write!(
                                formatter,
                                "{name}'s \"{alias}\" run command is still active — purging \
                                 would delete the database it has open.{stop}"
                            ),
                            None => write!(
                                formatter,
                                "a run command is still active for {name} — purging would \
                                 delete the database it has open.{stop}"
                            ),
                        }
                    }
                }
            }
            Self::AlreadyInstalled { id, identifier } => write!(
                formatter,
                "{identifier} is installed as {id} — purge only reaches data an installed \
                 app has no claim on. Use `tfsapp-hub remove {id} --purge` instead."
            ),
            Self::NoOrphanData { identifier } => write!(
                formatter,
                "{identifier}: nothing found under TFSApp/ — `tfsapp-hub purge` lists every \
                 identifier that has data to purge."
            ),
            Self::SymlinkData {
                identifier,
                path,
                target,
            } => {
                let target = match target {
                    Some(target) => format!(" (to {})", target.display()),
                    None => String::new(),
                };
                write!(
                    formatter,
                    "{identifier}'s data directory ({}) is a symlink{target} — refusing to \
                     purge through a link. Delete it by hand if you mean to clear it.",
                    path.display()
                )
            }
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Gate(error) => Some(error),
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

impl From<GateError> for RemoveError {
    fn from(error: GateError) -> Self {
        Self::Gate(error)
    }
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod tests;
