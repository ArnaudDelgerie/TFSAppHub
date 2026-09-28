//! Per-app lifecycle gates: activity is shared, maintenance is exclusive.
//!
//! The advisory text is useful for an actionable refusal, but the retained
//! flock is authoritative. A stale record therefore never grants access.
//!
//! Holding a lease is also what makes the interrupted-operation guard true:
//! every holder re-reads the records of the identifier's data directory —
//! the update journal, the rollback marker — while it owns the lease, so a
//! record a killed command left behind refuses the next operation no
//! matter which side of the dispatch check it arrived from. Each record
//! exempts only the command that resolves it.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    paths::{Paths, PathsError},
    registry, rollback, update_transaction,
};

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
    /// The identifier's data directory holds an update journal, so the
    /// operation being gated would run over an interrupted update (see
    /// `contract/6-lifecycle.md`, "An interrupted update requires an
    /// explicit repair"). `id` is the registry's answer for who owns that
    /// journal, resolved best-effort: the refusal needs nothing but the
    /// journal's presence, the id only makes the message actionable.
    RepairRequired {
        identifier: String,
        id: Option<String>,
    },
    /// The identifier's data directory holds a rollback marker: a rollback
    /// stopped partway through, and the only way out is running `rollback`
    /// again to finish it — never `repair`, which knows nothing about a
    /// rollback's steps.
    RollbackRequired {
        identifier: String,
        id: Option<String>,
    },
    /// An interrupted-operation record's presence could not even be
    /// established — a guard that cannot read cannot let anything through.
    Journal {
        identifier: String,
        source: update_transaction::JournalError,
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
            Self::RepairRequired { id: Some(id), .. } => write!(
                formatter,
                "{id} has an interrupted update; run `tfsapp-hub repair {id} --yes` first."
            ),
            Self::RepairRequired {
                identifier,
                id: None,
            } => write!(
                formatter,
                "{identifier} has an interrupted update; run `tfsapp-hub repair <id> --yes` on \
                 the app that owns it first."
            ),
            Self::RollbackRequired { id: Some(id), .. } => write!(
                formatter,
                "{id} has an interrupted rollback; run `tfsapp-hub rollback {id} --yes` again \
                 to finish it."
            ),
            Self::RollbackRequired {
                identifier,
                id: None,
            } => write!(
                formatter,
                "{identifier} has an interrupted rollback; run `tfsapp-hub rollback <id> --yes` \
                 again on the app that owns it, to finish it."
            ),
            Self::Journal { identifier, source } => write!(
                formatter,
                "cannot inspect the interrupted-operation records for {identifier}: {source}"
            ),
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

fn held_operation(path: &Path) -> Option<String> {
    match busy_decision(path) {
        GateDecision::Busy { operation } => operation,
        GateDecision::Granted => unreachable!("a read-only record probe cannot grant a flock"),
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

/// Refuse while the identifier's data directory holds an interrupted-
/// operation record — the guard every lease holder runs once it owns its
/// lease, so a record written after the dispatch-side advisory check is
/// still seen by whoever actually holds the lease.
///
/// `operation` is the maintenance operation being gated, or `None` for an
/// activity lease (whose holder runs no named operation). Two exemptions,
/// each belonging to its own record: `repair` may pass an update journal
/// (finishing or reverting the interrupted update is its whole job), and
/// `rollback` may pass a rollback marker (finishing the interrupted
/// rollback is its). A rollback marker refuses `repair` like anything
/// else: repair knows nothing about a rollback's steps.
///
/// The id in a refusal is resolved best-effort: any registry failure gives
/// `None`, because the refusal stands on the record alone and the id only
/// sharpens the message.
fn refuse_if_interrupted(
    paths: &Paths,
    identifier: &str,
    operation: Option<&str>,
) -> Result<(), GateError> {
    let data_dir = paths.app_data_dir(identifier).map_err(GateError::Paths)?;
    let owner = || {
        registry::load(paths).ok().and_then(|registry| {
            registry
                .by_identifier(identifier)
                .map(|entry| entry.id.clone())
        })
    };
    match update_transaction::read(&data_dir) {
        Ok(None) => {}
        Ok(Some(_)) if operation == Some("repair") => {}
        Ok(Some(_)) => {
            return Err(GateError::RepairRequired {
                identifier: identifier.to_string(),
                id: owner(),
            })
        }
        Err(source) => {
            return Err(GateError::Journal {
                identifier: identifier.to_string(),
                source,
            })
        }
    }
    match rollback::read_marker(&data_dir) {
        Ok(None) => {}
        Ok(Some(_)) if operation == Some("rollback") => {}
        Ok(Some(_)) => {
            return Err(GateError::RollbackRequired {
                identifier: identifier.to_string(),
                id: owner(),
            })
        }
        Err(source) => {
            return Err(GateError::Journal {
                identifier: identifier.to_string(),
                source,
            })
        }
    }
    Ok(())
}

/// The dispatch side's advisory answer: resolve `id`'s registry entry and
/// return the refusal [`refuse_if_interrupted`] would give for `operation`
/// — `Ok(None)` when the id is not registered, or when nothing interrupts
/// it. The authoritative check stays the one under the lease; this exists
/// only so a command whose first act predates its lease still says
/// "repair"/"rollback" instead of misreporting the interrupted state.
pub fn interruption(
    paths: &Paths,
    id: &str,
    operation: Option<&str>,
) -> Result<Option<GateError>, GateError> {
    // A registry that cannot be read leaves nothing to resolve, and the
    // advisory check has no business failing the command for it — the
    // lease-held check re-reads everything under its lock anyway.
    let Some(installed) = registry::load(paths).ok() else {
        return Ok(None);
    };
    let Some(entry) = installed.get(id) else {
        return Ok(None);
    };
    match refuse_if_interrupted(paths, &entry.identifier, operation) {
        Ok(()) => Ok(None),
        Err(error) => Ok(Some(error)),
    }
}

pub fn acquire_activity(paths: &Paths, identifier: &str) -> Result<ActivityLease, GateError> {
    let path = gate_path(paths, identifier)?;
    match tfsapp_core::process::try_lock_file_shared(&path).map_err(|source| GateError::Io {
        path: path.clone(),
        source,
    })? {
        Some(file) => {
            // A completed maintenance command leaves advisory text behind
            // until the next exclusive owner overwrites it. Once activity
            // holds this shared flock, that text cannot describe the current
            // owner, so clear it before a later maintenance contender reads
            // a stale operation name.
            file.set_len(0)
                .map_err(|source| GateError::Io { path, source })?;
            // Under the shared flock, so no record can be written or
            // discarded behind this read. A refusal drops the file handle,
            // which releases the lease.
            refuse_if_interrupted(paths, identifier, None)?;
            Ok(ActivityLease { _file: file })
        }
        None => match gate_decision(false, held_operation(&path)) {
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
        return match gate_decision(false, held_operation(&path)) {
            GateDecision::Busy { operation } => Err(GateError::Busy {
                identifier: identifier.to_string(),
                operation,
            }),
            GateDecision::Granted => unreachable!(),
        };
    };
    // Under the exclusive flock, and before the advisory operation text is
    // written — a refusal must not leave text claiming an operation that
    // then never runs. Each record exempts its own resolving command inside
    // `refuse_if_interrupted` (`repair` the journal, `rollback` the marker),
    // which is why the call is unconditional here. A refusal drops the file
    // handle, which releases the lease.
    refuse_if_interrupted(paths, identifier, Some(operation))?;
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

    /// A minimal registry entry, only so a journal has an `outgoing` to
    /// serialise — and so the id-resolution test has a name to find. Nothing
    /// but the `id`/`identifier` pair is ever read back.
    fn outgoing_entry(id: &str) -> registry::RegistryEntry {
        registry::RegistryEntry {
            id: id.to_string(),
            identifier: "dev.local.demo".to_string(),
            source: registry::Source {
                kind: registry::SourceKind::LocalPath,
                location: "/dev/null".to_string(),
                reference: None,
                reference_kind: None,
                index: None,
            },
            app_version: "1.0.0".to_string(),
            source_revision: "sha256:deadbeef".to_string(),
            app_port: None,
            platform: registry::Platform {
                php_version: "8.5".to_string(),
                extensions_hash: "a1b2c3d4".repeat(8),
            },
            state: registry::State::Ready,
            installed_at: registry::now_timestamp(),
            updated_at: registry::now_timestamp(),
            unknown: serde_json::Map::new(),
        }
    }

    /// Seed the identifier's data directory with the journal an interrupted
    /// `update` leaves behind, through the real writer so the guard reads
    /// exactly what production wrote.
    fn seed_journal(paths: &Paths) {
        let data_dir = paths.app_data_dir("dev.local.demo").unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        let journal = update_transaction::Journal::prepared(
            update_transaction::TransactionKind::Apply,
            outgoing_entry("demo"),
        );
        update_transaction::write(&data_dir, &journal).unwrap();
    }

    #[test]
    fn a_journal_refuses_a_maintenance_holder_and_the_refusal_releases_the_lease() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        seed_journal(&paths);

        // No registry: the refusal stands on the journal alone, so the id is
        // only `None` — the message points at `<id>` instead of naming one.
        match acquire_maintenance(&paths, "dev.local.demo", "export") {
            Err(GateError::RepairRequired { identifier, id }) => {
                assert_eq!(identifier, "dev.local.demo");
                assert_eq!(id, None);
            }
            Err(other) => panic!("the journal must refuse, not {other:?}"),
            Ok(_) => panic!("the journal must refuse the export"),
        }

        // The refused holder dropped its file, so the lease is free again:
        // once the journal is gone, the same operation is granted.
        fs::remove_file(update_transaction::journal_path(
            &paths.app_data_dir("dev.local.demo").unwrap(),
        ))
        .unwrap();
        assert!(acquire_maintenance(&paths, "dev.local.demo", "export").is_ok());
    }

    /// Seed the identifier's data directory with the rollback marker an
    /// interrupted `rollback` leaves behind, through the real writer so the
    /// guard reads exactly what production wrote.
    fn seed_marker(paths: &Paths) {
        let data_dir = paths.app_data_dir("dev.local.demo").unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        rollback::write_marker(
            &data_dir,
            &rollback::RollbackMarker {
                format_version: rollback::MARKER_FORMAT_VERSION,
                target_version: "1.0.0".to_string(),
                source_revision: "sha256:previous".to_string(),
                rescue_path: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn a_marker_refuses_every_operation_but_rollback_and_the_refusal_releases_the_lease() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        seed_marker(&paths);

        for operation in ["export", "repair"] {
            match acquire_maintenance(&paths, "dev.local.demo", operation) {
                Err(GateError::RollbackRequired { identifier, .. }) => {
                    assert_eq!(identifier, "dev.local.demo");
                }
                Err(other) => panic!("the marker must refuse {operation}, not {other:?}"),
                Ok(_) => panic!("the marker must refuse the {operation}"),
            }
        }
        assert!(matches!(
            acquire_activity(&paths, "dev.local.demo"),
            Err(GateError::RollbackRequired { .. })
        ));

        // `rollback` is the one command that resolves a marker: granted,
        // and it is the refusal above (not a stale flock) that blocked the
        // others.
        assert!(acquire_maintenance(&paths, "dev.local.demo", "rollback").is_ok());
    }

    #[test]
    fn an_unreadable_marker_refuses_rather_than_guessing() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        let data_dir = paths.app_data_dir("dev.local.demo").unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(rollback::marker_path(&data_dir), "not json").unwrap();

        assert!(matches!(
            acquire_maintenance(&paths, "dev.local.demo", "export"),
            Err(GateError::Journal { .. })
        ));
        assert!(matches!(
            acquire_activity(&paths, "dev.local.demo"),
            Err(GateError::Journal { .. })
        ));
    }

    #[test]
    fn repair_is_exempt_from_the_journal_guard() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        seed_journal(&paths);

        assert!(acquire_maintenance(&paths, "dev.local.demo", "repair").is_ok());
    }

    #[test]
    fn a_journal_refuses_an_activity_holder() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        seed_journal(&paths);

        assert!(matches!(
            acquire_activity(&paths, "dev.local.demo"),
            Err(GateError::RepairRequired { .. })
        ));
    }

    #[test]
    fn a_registered_owner_is_named_in_the_refusal() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        registry::update(&paths, |registry| registry.upsert(outgoing_entry("demo"))).unwrap();
        seed_journal(&paths);

        match acquire_maintenance(&paths, "dev.local.demo", "export") {
            Err(GateError::RepairRequired { id: Some(id), .. }) => assert_eq!(id, "demo"),
            Err(other) => panic!("the registry must name the owner, not {other:?}"),
            Ok(_) => panic!("the journal must refuse the export"),
        }
    }

    #[test]
    fn an_unreadable_journal_refuses_rather_than_guessing() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        let data_dir = paths.app_data_dir("dev.local.demo").unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(update_transaction::journal_path(&data_dir), "not json").unwrap();

        assert!(matches!(
            acquire_maintenance(&paths, "dev.local.demo", "export"),
            Err(GateError::Journal { .. })
        ));
        assert!(matches!(
            acquire_activity(&paths, "dev.local.demo"),
            Err(GateError::Journal { .. })
        ));
    }

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
    fn an_activity_holder_clears_an_old_maintenance_record() {
        let base = tempfile::tempdir().unwrap();
        let paths = Paths::rooted_at(base.path());
        drop(acquire_maintenance(&paths, "dev.local.demo", "export").unwrap());
        let _activity = acquire_activity(&paths, "dev.local.demo").unwrap();

        assert!(matches!(
            acquire_maintenance(&paths, "dev.local.demo", "import"),
            Err(GateError::Busy {
                operation: None,
                ..
            })
        ));
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
