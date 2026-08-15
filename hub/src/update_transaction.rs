//! Durable, outgoing-only state for an interrupted app update.
//!
//! This is deliberately separate from the public rollback anchor: until an
//! update is committed, its outgoing files belong to this transaction alone.

use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::registry::RegistryEntry;

pub const FORMAT_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "update-transaction.json";
const STAGING_DIR: &str = ".update-transaction";

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

pub fn write(data_dir: &Path, journal: &Journal) -> Result<(), JournalError> {
    let target = journal_path(data_dir);
    let temp = target.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(journal).map_err(|error| JournalError::Malformed {
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

pub fn read(data_dir: &Path) -> Result<Option<Journal>, JournalError> {
    let path = journal_path(data_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(JournalError::Io { path, source }),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| JournalError::Malformed {
            path,
            detail: error.to_string(),
        })
}
