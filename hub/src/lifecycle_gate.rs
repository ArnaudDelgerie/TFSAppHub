//! Per-app lifecycle gates: activity is shared, maintenance is exclusive.
//!
//! The advisory text is useful for an actionable refusal, but the retained
//! flock is authoritative. A stale record therefore never grants access.
//!
//! This first step defines the API; its callers land in the following plan
//! steps, so its public-to-the-hub surface is intentionally not live yet.
#![allow(dead_code)]

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::paths::{Paths, PathsError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Granted,
    Busy { operation: Option<String> },
}

#[derive(Debug)]
pub enum GateError {
    Paths(PathsError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Busy {
        identifier: String,
        operation: Option<String>,
    },
}

impl fmt::Display for GateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => error.fmt(formatter),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Busy {
                identifier,
                operation: Some(operation),
            } => write!(
                formatter,
                "{identifier} is busy: {operation} is already in progress"
            ),
            Self::Busy {
                identifier,
                operation: None,
            } => {
                write!(
                    formatter,
                    "{identifier} is busy with another lifecycle operation"
                )
            }
        }
    }
}

impl std::error::Error for GateError {}

/// A retained activity lease. Its file handle deliberately has no public
/// operations: retaining it is the whole protocol.
pub struct ActivityLease {
    _file: std::fs::File,
}

/// A retained maintenance lease. The record is written only after exclusivity
/// is acquired, so contenders can name this operation without trusting it for
/// ownership.
pub struct MaintenanceLease {
    _file: std::fs::File,
}

fn busy_decision(path: &Path) -> GateDecision {
    GateDecision::Busy {
        operation: fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
    }
}

/// Purely maps the flock result plus its advisory record to the result a
/// caller should present. The record is deliberately irrelevant on success.
pub fn gate_decision(acquired: bool, held_operation: Option<String>) -> GateDecision {
    if acquired {
        GateDecision::Granted
    } else {
        GateDecision::Busy {
            operation: held_operation,
        }
    }
}

fn gate_path(paths: &Paths, identifier: &str) -> Result<PathBuf, GateError> {
    let path = paths
        .lifecycle_gate_path(identifier)
        .map_err(GateError::Paths)?;
    let parent = path.parent().expect("lifecycle gate has a locks directory");
    fs::create_dir_all(parent).map_err(|source| GateError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    Ok(path)
}

pub fn acquire_activity(paths: &Paths, identifier: &str) -> Result<ActivityLease, GateError> {
    let path = gate_path(paths, identifier)?;
    match tfsapp_core::process::try_lock_file_shared(&path).map_err(|source| GateError::Io {
        path: path.clone(),
        source,
    })? {
        Some(file) => Ok(ActivityLease { _file: file }),
        None => match gate_decision(
            false,
            match busy_decision(&path) {
                GateDecision::Busy { operation } => operation,
                GateDecision::Granted => unreachable!(),
            },
        ) {
            GateDecision::Busy { operation } => Err(GateError::Busy {
                identifier: identifier.to_string(),
                operation,
            }),
            GateDecision::Granted => unreachable!(),
        },
    }
}

pub fn acquire_maintenance(
    paths: &Paths,
    identifier: &str,
    operation: &str,
) -> Result<MaintenanceLease, GateError> {
    let path = gate_path(paths, identifier)?;
    let Some(mut file) =
        tfsapp_core::process::try_lock_file(&path).map_err(|source| GateError::Io {
            path: path.clone(),
            source,
        })?
    else {
        return match gate_decision(
            false,
            match busy_decision(&path) {
                GateDecision::Busy { operation } => operation,
                GateDecision::Granted => unreachable!(),
            },
        ) {
            GateDecision::Busy { operation } => Err(GateError::Busy {
                identifier: identifier.to_string(),
                operation,
            }),
            GateDecision::Granted => unreachable!(),
        };
    };
    use std::io::Write;
    file.set_len(0).map_err(|source| GateError::Io {
        path: path.clone(),
        source,
    })?;
    file.write_all(operation.as_bytes())
        .map_err(|source| GateError::Io { path, source })?;
    Ok(MaintenanceLease { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_holders_coexist_and_a_maintenance_holder_excludes_both_modes() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        let first = acquire_activity(&paths, "dev.local.demo").unwrap();
        let second = acquire_activity(&paths, "dev.local.demo").unwrap();
        assert!(matches!(
            acquire_maintenance(&paths, "dev.local.demo", "update"),
            Err(GateError::Busy {
                operation: None,
                ..
            })
        ));
        drop(first);
        drop(second);

        let maintenance = acquire_maintenance(&paths, "dev.local.demo", "update").unwrap();
        assert!(
            matches!(acquire_activity(&paths, "dev.local.demo"), Err(GateError::Busy { operation: Some(operation), .. }) if operation == "update")
        );
        drop(maintenance);
        assert!(acquire_activity(&paths, "dev.local.demo").is_ok());
    }

    #[test]
    fn releasing_a_lease_allows_the_other_mode_and_other_identifiers_are_independent() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        let maintenance = acquire_maintenance(&paths, "dev.local.demo", "import").unwrap();
        assert!(acquire_maintenance(&paths, "dev.local.other", "import").is_ok());
        drop(maintenance);
        assert!(acquire_maintenance(&paths, "dev.local.demo", "rollback").is_ok());
    }

    #[test]
    fn a_stale_record_is_only_a_message_not_an_owner() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        let path = paths.lifecycle_gate_path("dev.local.demo").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old update").unwrap();
        assert!(acquire_maintenance(&paths, "dev.local.demo", "update").is_ok());
    }

    #[test]
    fn gate_decision_only_treats_a_retained_flock_as_authoritative() {
        assert_eq!(
            gate_decision(true, Some("old update".to_string())),
            GateDecision::Granted
        );
        assert_eq!(
            gate_decision(false, Some("update".to_string())),
            GateDecision::Busy {
                operation: Some("update".to_string())
            }
        );
    }
}
