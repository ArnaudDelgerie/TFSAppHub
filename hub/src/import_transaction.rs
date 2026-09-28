//! The import transaction's durable record and filesystem moves (plan 064).
//!
//! `portability.rs` keeps the orchestration; this module owns the intent
//! record and the two half-sequences around it. The archive is extracted
//! into a staging directory first — the live data is not touched — and only
//! then is the intent written and followed by a short run of renames: live
//! members to reserved rescue names, staged members to their live places.
//! Before `Committed`, the whole thing is reversible by renaming the
//! rescues back, which is exactly what [`back_out`] does; after it, all
//! that is left is cleanup ([`finish_import`]).
//!
//! A kill is resolved by `repair <id>`: before the commit it puts the data
//! the import replaced back, after it it finishes. The 060 update journal
//! is deliberately untouched — different commit model, separate record.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    lifecycle::{self, LifecycleError},
    update, update_transaction,
};

pub(crate) const FORMAT_VERSION: u32 = 1;
const INTENT_FILE: &str = "import-transaction.json";
const STAGING_DIR: &str = ".import-transaction";
/// The archive's internal prefix names, mirroring `portability`'s own two
/// constants — repeated rather than imported so this module stays runnable
/// without the whole pipeline attached.
const DATA_DIR: &str = "data";
const UPLOADS_DIR: &str = "uploads";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPhase {
    /// The archive is staged and the intent written; the live data may be
    /// partly switched. Reversible by [`back_out`].
    Staged,
    /// The switch, the version record and the forward migration are done.
    /// Only cleanup remains ([`finish_import`]); there is nothing to go
    /// back to.
    Committed,
}

/// The durable record an import writes once its archive is extracted, and
/// rewrites with [`ImportPhase::Committed`] once the data it brought is the
/// live data. The rescue names were reserved *before* this record was
/// written, so it already names where everything will go — under the
/// maintenance lease, nothing else creates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportIntent {
    pub format_version: u32,
    pub phase: ImportPhase,
    /// The version the archive's manifest declares — what the version
    /// record is about to carry.
    pub archive_version: String,
    /// The version record the import found on disk, to be written back by
    /// [`back_out`]; `None` when there was none, in which case back-out
    /// removes `data/config.json` instead.
    pub outgoing_version: Option<String>,
    /// One `(member, rescue path)` per live database member the import is
    /// about to set aside by rename.
    pub db_rescues: Vec<(String, PathBuf)>,
    /// The reserved rescue name for a non-empty live `uploads/`, if any.
    pub uploads_rescue: Option<PathBuf>,
}

pub fn intent_path(data_dir: &Path) -> PathBuf {
    data_dir.join(INTENT_FILE)
}

/// The staging directory the archive is extracted into before anything live
/// is touched, holding the archive's `data/` and `uploads/` prefixes.
pub fn staging_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(STAGING_DIR)
}

pub fn staged_data_dir(data_dir: &Path) -> PathBuf {
    staging_dir(data_dir).join(DATA_DIR)
}

pub fn staged_uploads_dir(data_dir: &Path) -> PathBuf {
    staging_dir(data_dir).join(UPLOADS_DIR)
}

pub fn write_intent(
    data_dir: &Path,
    intent: &ImportIntent,
) -> Result<(), update_transaction::JournalError> {
    update_transaction::write_record(data_dir, INTENT_FILE, intent)
}

/// Read the intent, rejecting an unknown `format_version` as malformed —
/// the same stance as the update journal's own read.
pub fn read_intent(
    data_dir: &Path,
) -> Result<Option<ImportIntent>, update_transaction::JournalError> {
    let Some(intent): Option<ImportIntent> =
        update_transaction::read_record(data_dir, INTENT_FILE)?
    else {
        return Ok(None);
    };
    if intent.format_version != FORMAT_VERSION {
        return Err(update_transaction::JournalError::Malformed {
            path: intent_path(data_dir),
            detail: format!(
                "unsupported transaction format version {}",
                intent.format_version
            ),
        });
    }
    Ok(Some(intent))
}

/// Remove the intent, strictly: an intent that cannot be removed keeps every
/// other command refused, which is exactly what an error here must report.
pub fn remove_intent(data_dir: &Path) -> io::Result<()> {
    fs::remove_file(intent_path(data_dir))?;
    fs::File::open(data_dir)?.sync_all()
}

/// Remove the staging directory, best-effort: a leftover with no intent
/// behind it is inert, and the next import clears it before extracting.
pub fn remove_staging(data_dir: &Path) {
    let _ = fs::remove_dir_all(staging_dir(data_dir));
}

/// Whether `data_dir`'s `uploads/` holds anything at all — the same probe
/// `portability::data_dir_populated` acts on, for the one decision this
/// module shares with it: an empty or absent `uploads/` needs no rescue.
fn uploads_dir_non_empty(data_dir: &Path) -> bool {
    match fs::read_dir(data_dir.join(UPLOADS_DIR)) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Build the intent for an import whose archive is already staged: record
/// the version the import is about to replace, and reserve one rescue name
/// per live database member present plus one for a non-empty `uploads/`.
pub fn build_intent(
    data_dir: &Path,
    data_subdir: &Path,
    archive_version: &str,
) -> Result<ImportIntent, LifecycleError> {
    let outgoing_version = lifecycle::read_data_version(data_subdir)?;
    let mut db_rescues = Vec::new();
    for name in lifecycle::DB_FILE_NAMES {
        if data_subdir.join(name).is_file() {
            db_rescues.push((
                name.to_string(),
                lifecycle::reserve_rescue_name(data_subdir, name),
            ));
        }
    }
    let uploads_rescue = if uploads_dir_non_empty(data_dir) {
        Some(lifecycle::reserve_rescue_name(data_dir, UPLOADS_DIR))
    } else {
        None
    };
    Ok(ImportIntent {
        format_version: FORMAT_VERSION,
        phase: ImportPhase::Staged,
        archive_version: archive_version.to_string(),
        outgoing_version,
        db_rescues,
        uploads_rescue,
    })
}

/// The switch: live members to the rescue names the intent reserved, staged
/// members to their live places. Only the database members ever leave
/// staging's `data/` — anything else a forged archive put under `data/`
/// stays in staging and goes with it.
pub fn switch(data_dir: &Path, data_subdir: &Path, intent: &ImportIntent) -> io::Result<()> {
    for (name, rescue) in &intent.db_rescues {
        fs::rename(data_subdir.join(name), rescue).map_err(|source| named(rescue, source))?;
    }
    stop_at("import_db_rescued")?;

    match &intent.uploads_rescue {
        Some(rescue) => fs::rename(data_dir.join(UPLOADS_DIR), rescue)
            .map_err(|source| named(rescue, source))?,
        // An empty or absent `uploads/` needs no rescue; an empty one must
        // still get out of the staged tree's way.
        None => {
            let _ = fs::remove_dir(data_dir.join(UPLOADS_DIR));
        }
    }
    stop_at("import_uploads_rescued")?;

    for name in lifecycle::DB_FILE_NAMES {
        let staged = staged_data_dir(data_dir).join(name);
        if staged.is_file() {
            let live = data_subdir.join(name);
            fs::rename(&staged, &live).map_err(|source| named(&live, source))?;
        }
    }
    let staged_uploads = staged_uploads_dir(data_dir);
    let live_uploads = data_dir.join(UPLOADS_DIR);
    if staged_uploads.is_dir() {
        fs::rename(&staged_uploads, &live_uploads)
            .map_err(|source| named(&live_uploads, source))?;
    }
    stop_at("import_switched")?;
    Ok(())
}

/// Put the data the import replaced back, from any point before
/// [`ImportPhase::Committed`] — every step is guarded by the presence of
/// its own source, so a back-out of a half-done switch and a re-run after a
/// failed back-out both converge on the same state.
///
/// Returns an error naming the first path it failed on; the caller keeps
/// the intent, so a back-out that cannot finish is still resolvable by
/// `repair`.
pub fn back_out(data_dir: &Path, data_subdir: &Path, intent: &ImportIntent) -> io::Result<()> {
    // The database: a rescue that exists is renamed back over whatever the
    // archive or the migration left live; a rescue that does not exist
    // means that member was never moved, so it is left alone.
    for (name, rescue) in &intent.db_rescues {
        if rescue.exists() {
            let live = data_subdir.join(name);
            if live.exists() {
                fs::remove_file(&live).map_err(|source| named(&live, source))?;
            }
            fs::rename(rescue, &live).map_err(|source| named(&live, source))?;
        }
    }
    // A live member the intent never recorded came from the archive or the
    // migration, never from the installation — it has nothing to go back
    // to, so it goes.
    for name in lifecycle::DB_FILE_NAMES {
        if !intent.db_rescues.iter().any(|(rescued, _)| rescued == name) {
            let live = data_subdir.join(name);
            if live.exists() {
                fs::remove_file(&live).map_err(|source| named(&live, source))?;
            }
        }
    }

    // `uploads/`: a rescue that exists was renamed out of the way, so it
    // goes back over whatever the archive left live. A rescue that does
    // not exist was never renamed out, so the live tree is still the
    // previous one — nothing to do. No rescue at all means the previous
    // `uploads/` was empty or absent: an empty directory is what goes
    // back, whatever the archive put live.
    let live_uploads = data_dir.join(UPLOADS_DIR);
    match &intent.uploads_rescue {
        Some(rescue) if rescue.exists() => {
            if live_uploads.exists() {
                fs::remove_dir_all(&live_uploads).map_err(|source| named(&live_uploads, source))?;
            }
            fs::rename(rescue, &live_uploads).map_err(|source| named(&live_uploads, source))?;
        }
        // Reserved but never renamed out of the way: the live tree is the
        // previous one already.
        Some(_) => {}
        None => {
            if live_uploads.exists() {
                fs::remove_dir_all(&live_uploads).map_err(|source| named(&live_uploads, source))?;
            }
            fs::create_dir_all(&live_uploads).map_err(|source| named(&live_uploads, source))?;
        }
    }

    // The version record, exactly as the import found it.
    match &intent.outgoing_version {
        Some(version) => lifecycle::write_data_version(data_subdir, version).map_err(|error| {
            named(
                &data_subdir.join("config.json"),
                io::Error::other(error.to_string()),
            )
        })?,
        None => {
            let _ = fs::remove_file(data_subdir.join("config.json"));
        }
    }

    // The migration may have warmed a cache against the archive's database;
    // the same cleanup the import runs before its first mutation.
    crate::portability::clear_destination_cache(data_dir, data_subdir)
        .map_err(|error| io::Error::other(error.to_string()))?;

    remove_staging(data_dir);
    remove_intent(data_dir)?;
    Ok(())
}

/// The committed import's cleanup: consume the rollback anchor (an import
/// proceeding has already made it incoherent), the tree any interrupted
/// update left staged, the staging directory, and the intent itself —
/// strictly, because a leftover intent keeps every command refused.
pub fn finish_import(data_subdir: &Path, data_dir: &Path, app_dir: &Path) -> io::Result<()> {
    lifecycle::discard_rollback_anchor(data_subdir);
    lifecycle::discard_db_snapshot(data_subdir);
    update::discard_tree(app_dir);
    remove_staging(data_dir);
    remove_intent(data_dir)
}

/// Wrap an I/O failure with the path it failed on, so `back_out`'s caller
/// can name it.
fn named(path: &Path, source: io::Error) -> io::Error {
    io::Error::other(format!("{}: {source}", path.display()))
}

/// The payload of the deterministic kill stand-in — a distinct type so the
/// orchestration can tell a simulated kill from a real failure and skip the
/// in-process back-out for it: a stop must leave exactly what a kill leaves.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestStopError;

#[cfg(test)]
impl std::fmt::Display for TestStopError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "simulated kill")
    }
}

#[cfg(test)]
impl std::error::Error for TestStopError {}

/// The deterministic kill stand-in, on `update`'s own thread-local so one
/// `arm` covers both pipelines.
#[cfg(test)]
pub(crate) fn stop_at(point: &'static str) -> io::Result<()> {
    if crate::update::test_stop::hit(point) {
        return Err(io::Error::other(TestStopError));
    }
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn stop_at(_point: &'static str) -> io::Result<()> {
    Ok(())
}

/// Whether an error carries [`TestStopError`] — the one failure whose
/// handler is *not* an in-process back-out.
#[cfg(test)]
pub(crate) fn is_test_stop(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<TestStopError>().is_some())
}
