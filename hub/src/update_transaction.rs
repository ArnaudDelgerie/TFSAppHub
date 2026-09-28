//! Durable, outgoing-only state for an interrupted app update.
//!
//! This is deliberately separate from the public rollback anchor: until an
//! update is committed, its outgoing files belong to this transaction alone.

use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{lifecycle, registry::RegistryEntry};

pub const FORMAT_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "update-transaction.json";
const STAGING_DIR: &str = ".update-transaction";

/// The deterministic kill stand-in `update::stop_at` uses, shared here so
/// `finalise_anchor`'s own sub-steps (reached through a real `apply` as well
/// as directly) can be stopped at without a second thread-local.
#[cfg(test)]
fn stop_at(point: &'static str) -> io::Result<()> {
    if crate::update::test_stop::hit(point) {
        return Err(io::Error::other(format!("test stop point: {point}")));
    }
    Ok(())
}

#[cfg(not(test))]
fn stop_at(_point: &'static str) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    Apply,
    ResyncOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    SnapshotComplete,
    TreeRetained,
    ReplacementInstalled,
    LifecycleComplete,
    RegistryCommitted,
    AnchorFinalised,
}

impl<'de> Deserialize<'de> for Phase {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "prepared" => Ok(Self::Prepared),
            "snapshot_complete" => Ok(Self::SnapshotComplete),
            "tree_retained" => Ok(Self::TreeRetained),
            "replacement_installed" => Ok(Self::ReplacementInstalled),
            "lifecycle_complete" => Ok(Self::LifecycleComplete),
            "registry_committed" => Ok(Self::RegistryCommitted),
            "anchor_finalised" => Ok(Self::AnchorFinalised),
            other => Err(serde::de::Error::custom(format!(
                "unknown transaction phase `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub format_version: u32,
    pub kind: TransactionKind,
    pub outgoing: RegistryEntry,
    pub phase: Phase,
    /// Database members that existed when the outgoing snapshot completed.
    /// Absence is meaningful for SQLite's optional WAL/SHM twins, so recovery
    /// must never infer it from a missing staging file.
    #[serde(default)]
    pub database_members: Vec<String>,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Journal {
    pub fn prepared(kind: TransactionKind, outgoing: RegistryEntry) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            kind,
            outgoing,
            phase: Phase::Prepared,
            database_members: Vec::new(),
            unknown: Default::default(),
        }
    }

    pub fn advance(&mut self, phase: Phase) {
        self.phase = phase;
    }
}

#[derive(Debug)]
pub enum JournalError {
    Io { path: PathBuf, source: io::Error },
    Malformed { path: PathBuf, detail: String },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Malformed { path, detail } => write!(f, "{}: {detail}", path.display()),
        }
    }
}
impl std::error::Error for JournalError {}

pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}
pub fn staging_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(STAGING_DIR)
}
pub fn staged_db_path(data_dir: &Path, name: &str) -> PathBuf {
    staging_dir(data_dir).join(name)
}
pub fn staged_tree_path(app_dir: &Path) -> PathBuf {
    let mut path = app_dir.as_os_str().to_os_string();
    path.push(".update-transaction");
    PathBuf::from(path)
}

/// The one durable write every interrupted-operation record shares — the
/// update journal, the import intent, the rollback marker: a same-directory
/// temp file, written and fsynced, `rename`d into place, then the directory
/// itself fsynced so the rename is durable too (plan 064).
pub(crate) fn write_record<T: Serialize>(
    data_dir: &Path,
    file_name: &str,
    value: &T,
) -> Result<(), JournalError> {
    let target = data_dir.join(file_name);
    let temp = target.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| JournalError::Malformed {
        path: target.clone(),
        detail: error.to_string(),
    })?;
    let mut file = fs::File::create(&temp).map_err(|source| JournalError::Io {
        path: temp.clone(),
        source,
    })?;
    file.write_all(&bytes).map_err(|source| JournalError::Io {
        path: temp.clone(),
        source,
    })?;
    file.sync_all().map_err(|source| JournalError::Io {
        path: temp.clone(),
        source,
    })?;
    drop(file);
    fs::rename(&temp, &target).map_err(|source| JournalError::Io {
        path: target.clone(),
        source,
    })?;
    fs::File::open(data_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|source| JournalError::Io {
            path: data_dir.to_path_buf(),
            source,
        })
}

/// The matching read: absent is `Ok(None)`, unreadable or unparseable is an
/// error. Format versions are *not* checked here — each record type checks
/// its own.
pub(crate) fn read_record<T: DeserializeOwned>(
    data_dir: &Path,
    file_name: &str,
) -> Result<Option<T>, JournalError> {
    let path = data_dir.join(file_name);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(JournalError::Io { path, source }),
    };
    let record: T = serde_json::from_str(&text).map_err(|error| JournalError::Malformed {
        path: path.clone(),
        detail: error.to_string(),
    })?;
    Ok(Some(record))
}

pub fn write(data_dir: &Path, journal: &Journal) -> Result<(), JournalError> {
    write_record(data_dir, JOURNAL_FILE, journal)
}

pub fn read(data_dir: &Path) -> Result<Option<Journal>, JournalError> {
    let Some(journal): Option<Journal> = read_record(data_dir, JOURNAL_FILE)? else {
        return Ok(None);
    };
    if journal.format_version != FORMAT_VERSION {
        return Err(JournalError::Malformed {
            path: journal_path(data_dir),
            detail: format!(
                "unsupported transaction format version {}",
                journal.format_version
            ),
        });
    }
    Ok(Some(journal))
}

pub fn snapshot_db(data_subdir: &Path, data_dir: &Path) -> io::Result<Vec<String>> {
    let staging = staging_dir(data_dir);
    fs::create_dir_all(&staging)?;
    let mut members = Vec::new();
    for name in lifecycle::DB_FILE_NAMES {
        let source = data_subdir.join(name);
        let target = staged_db_path(data_dir, name);
        if source.is_file() {
            fs::copy(&source, &target)?;
            fs::File::open(&target)?.sync_all()?;
            members.push(name.to_string());
        } else {
            let _ = fs::remove_file(target);
        }
    }
    fs::File::open(staging)?.sync_all()?;
    Ok(members)
}

pub fn retain_tree(app_dir: &Path) -> io::Result<()> {
    let staged = staged_tree_path(app_dir);
    let _ = fs::remove_dir_all(&staged);
    fs::rename(app_dir, staged)?;
    fs::File::open(app_dir.parent().expect("an app dir has a parent"))?.sync_all()
}

/// Promote a retained transaction to the ordinary public rollback anchor.
/// Replayable: each step is guarded by the presence of its own source, so a
/// kill between any two of them and a second call once it already finished
/// both reach the same end state rather than erroring or re-destroying what
/// the first call already promoted.
///
/// 1. While the staged tree still exists, the promotion has not happened yet
///    this attempt: discard the old public anchor's database snapshot and
///    `rollback.json`, remove the old `.previous` tree, and rename the
///    staged tree onto it. Guarding the whole step on the staged tree is what
///    stops a replay from discarding the anchor this same call just wrote.
/// 2. For each member `database_members` (the journal's own record, not a
///    fresh probe of [`lifecycle::DB_FILE_NAMES`]) says was snapshotted:
///    rename its staged copy to the public snapshot path if the staged copy
///    is still there, or leave it alone if the public copy is already there
///    (an earlier call already renamed it). Neither existing is an error.
/// 3. Write `rollback.json`, skipped when it already names this same
///    outgoing version — the ordinary case on a replay after full success.
/// 4. Sync `data/` and `apps/`.
pub fn finalise_anchor(
    data_subdir: &Path,
    data_dir: &Path,
    app_dir: &Path,
    entry: &RegistryEntry,
    database_members: &[String],
) -> io::Result<()> {
    let staged_tree = staged_tree_path(app_dir);
    if staged_tree.is_dir() {
        lifecycle::discard_db_snapshot(data_subdir);
        lifecycle::discard_rollback_anchor(data_subdir);
        let previous = lifecycle::previous_tree_path(app_dir);
        let _ = fs::remove_dir_all(&previous);
        fs::rename(&staged_tree, &previous)?;
    }
    stop_at("finalise_tree_promoted")?;

    for name in database_members {
        let staged = staged_db_path(data_dir, name);
        let public = lifecycle::db_snapshot_path(data_subdir, name);
        if staged.is_file() {
            fs::rename(&staged, &public)?;
        } else if !public.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("database member {name} is neither staged nor already promoted"),
            ));
        }
    }
    stop_at("finalise_members_promoted")?;

    let already_finalised = lifecycle::read_rollback_anchor(data_subdir).is_some_and(|anchor| {
        anchor.app_version == entry.app_version && anchor.source_revision == entry.source_revision
    });
    if !already_finalised {
        lifecycle::write_rollback_anchor(
            data_subdir,
            &lifecycle::RollbackAnchor {
                app_version: entry.app_version.clone(),
                source_revision: entry.source_revision.clone(),
                created_at: crate::registry::now_timestamp(),
            },
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    }
    stop_at("finalise_rollback_written")?;

    fs::File::open(data_subdir)?.sync_all()?;
    fs::File::open(app_dir.parent().expect("an app dir has a parent"))?.sync_all()
}

pub fn discard(data_dir: &Path) -> io::Result<()> {
    let journal = journal_path(data_dir);
    let _ = fs::remove_file(journal);
    let staging = staging_dir(data_dir);
    let _ = fs::remove_dir_all(staging);
    fs::File::open(data_dir)?.sync_all()
}

pub fn discard_tree(app_dir: &Path) -> io::Result<()> {
    let _ = fs::remove_dir_all(staged_tree_path(app_dir));
    fs::File::open(app_dir.parent().expect("an app dir has a parent"))?.sync_all()
}

pub fn restore_outgoing(
    data_subdir: &Path,
    data_dir: &Path,
    app_dir: &Path,
    version: &str,
) -> io::Result<()> {
    for name in lifecycle::DB_FILE_NAMES {
        let staged = staged_db_path(data_dir, name);
        let live = data_subdir.join(name);
        if staged.is_file() {
            fs::copy(staged, live)?;
        } else {
            let _ = fs::remove_file(live);
        }
    }
    let _ = fs::remove_dir_all(app_dir);
    let staged_tree = staged_tree_path(app_dir);
    if staged_tree.is_dir() {
        fs::rename(staged_tree, app_dir)?;
    }
    let _ = lifecycle::discard_cache_stamp(data_subdir);
    lifecycle::write_data_version(data_subdir, version)
        .map_err(|error| io::Error::other(error.to_string()))?;
    discard(data_dir)
}
