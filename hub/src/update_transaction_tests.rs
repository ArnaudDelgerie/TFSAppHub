use std::fs;

use crate::{
    lifecycle::{self, read_data_version},
    paths::Paths,
    registry,
    registry::{Platform, RegistryEntry, Source, SourceKind, State},
    update::recover_transaction,
    update_transaction::{
        journal_path, read, retain_tree, snapshot_db, staged_db_path, staged_tree_path, write,
        Journal, Phase, TransactionKind,
    },
};

fn entry() -> RegistryEntry {
    RegistryEntry {
        id: "demo".into(),
        identifier: "dev.local.demo".into(),
        source: Source {
            kind: SourceKind::LocalPath,
            location: "/source".into(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "0.6.0".into(),
        source_revision: "old".into(),
        app_port: None,
        platform: Platform {
            php_version: "8.5".into(),
            extensions_hash: "x".repeat(64),
        },
        state: State::Ready,
        installed_at: "now".into(),
        updated_at: "now".into(),
        unknown: Default::default(),
    }
}

fn seeded_paths() -> (
    tempfile::TempDir,
    Paths,
    RegistryEntry,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let base = tempfile::tempdir().unwrap();
    let paths = Paths::rooted_at(base.path());
    let entry = entry();
    registry::update(&paths, |registry| registry.apps.push(entry.clone())).unwrap();
    let data_dir = paths.create_app_data_dir(&entry.identifier).unwrap();
    let app_dir = paths.app_dir(&entry.id).unwrap();
    fs::create_dir_all(app_dir.join("public")).unwrap();
    fs::write(app_dir.join("public/version"), "old").unwrap();
    fs::create_dir_all(data_dir.join("data")).unwrap();
    fs::write(data_dir.join("data/app.db"), "old-db").unwrap();
    lifecycle::write_data_version(&data_dir.join("data"), &entry.app_version).unwrap();
    (base, paths, entry, data_dir, app_dir)
}

#[test]
fn recovery_restores_a_replacement_from_the_durable_outgoing_state() {
    let (_base, paths, entry, data_dir, app_dir) = seeded_paths();
    let mut journal = Journal::prepared(TransactionKind::Apply, entry.clone());
    journal.database_members = snapshot_db(&data_dir.join("data"), &data_dir).unwrap();
    journal.advance(Phase::SnapshotComplete);
    retain_tree(&app_dir).unwrap();
    journal.advance(Phase::TreeRetained);
    fs::create_dir_all(app_dir.join("public")).unwrap();
    fs::write(app_dir.join("public/version"), "new").unwrap();
    fs::write(data_dir.join("data/app.db"), "new-db").unwrap();
    lifecycle::write_data_version(&data_dir.join("data"), "0.7.0").unwrap();
    registry::update(&paths, |registry| {
        registry.get_mut("demo").unwrap().app_version = "0.7.0".into();
    })
    .unwrap();
    journal.advance(Phase::RegistryCommitted);
    write(&data_dir, &journal).unwrap();

    let outcome = recover_transaction(&paths, &data_dir, &app_dir, &journal);
    assert!(outcome.is_complete());
    assert_eq!(
        fs::read_to_string(app_dir.join("public/version")).unwrap(),
        "old"
    );
    assert_eq!(
        fs::read_to_string(data_dir.join("data/app.db")).unwrap(),
        "old-db"
    );
    assert_eq!(
        read_data_version(&data_dir.join("data"))
            .unwrap()
            .as_deref(),
        Some("0.6.0")
    );
    assert_eq!(registry::load(&paths).unwrap().get("demo").unwrap(), &entry);
    assert!(read(&data_dir).unwrap().is_none());
}

#[test]
fn failed_recovery_keeps_its_journal_for_a_later_retry() {
    let (_base, paths, entry, data_dir, app_dir) = seeded_paths();
    let mut journal = Journal::prepared(TransactionKind::Apply, entry);
    journal.database_members = vec!["app.db".into()];
    journal.advance(Phase::SnapshotComplete);
    fs::create_dir_all(staged_db_path(&data_dir, "app.db")).unwrap();
    write(&data_dir, &journal).unwrap();

    let outcome = recover_transaction(&paths, &data_dir, &app_dir, &journal);
    assert!(!outcome.is_complete());
    assert!(read(&data_dir).unwrap().is_some());
}

#[test]
fn recovery_obeys_each_durable_phase_without_guessing() {
    // These fixtures are the deterministic equivalent of stopping the process
    // immediately after each journal write.  They deliberately construct only
    // state the recorded phase permits recovery to undo.
    for phase in [
        Phase::Prepared,
        Phase::SnapshotComplete,
        Phase::TreeRetained,
        Phase::ReplacementInstalled,
        Phase::LifecycleComplete,
        Phase::RegistryCommitted,
        Phase::AnchorFinalised,
    ] {
        let (_base, paths, entry, data_dir, app_dir) = seeded_paths();
        let mut journal = Journal::prepared(TransactionKind::Apply, entry.clone());

        if phase != Phase::Prepared {
            journal.database_members = snapshot_db(&data_dir.join("data"), &data_dir).unwrap();
        }
        if matches!(
            phase,
            Phase::SnapshotComplete
                | Phase::TreeRetained
                | Phase::ReplacementInstalled
                | Phase::LifecycleComplete
                | Phase::RegistryCommitted
                | Phase::AnchorFinalised
        ) {
            journal.advance(Phase::SnapshotComplete);
        }
        if matches!(
            phase,
            Phase::TreeRetained
                | Phase::ReplacementInstalled
                | Phase::LifecycleComplete
                | Phase::RegistryCommitted
                | Phase::AnchorFinalised
        ) {
            retain_tree(&app_dir).unwrap();
            fs::create_dir_all(app_dir.join("public")).unwrap();
            fs::write(app_dir.join("public/version"), "new").unwrap();
            journal.advance(Phase::TreeRetained);
        }
        if matches!(
            phase,
            Phase::ReplacementInstalled
                | Phase::LifecycleComplete
                | Phase::RegistryCommitted
                | Phase::AnchorFinalised
        ) {
            journal.advance(Phase::ReplacementInstalled);
        }
        if matches!(
            phase,
            Phase::LifecycleComplete | Phase::RegistryCommitted | Phase::AnchorFinalised
        ) {
            fs::write(data_dir.join("data/app.db"), "new-db").unwrap();
            lifecycle::write_data_version(&data_dir.join("data"), "0.7.0").unwrap();
            journal.advance(Phase::LifecycleComplete);
        }
        if matches!(phase, Phase::RegistryCommitted | Phase::AnchorFinalised) {
            registry::update(&paths, |registry| {
                registry.get_mut("demo").unwrap().app_version = "0.7.0".into();
            })
            .unwrap();
            journal.advance(Phase::RegistryCommitted);
        }
        if phase == Phase::AnchorFinalised {
            journal.advance(Phase::AnchorFinalised);
        }
        write(&data_dir, &journal).unwrap();

        let outcome = recover_transaction(&paths, &data_dir, &app_dir, &journal);
        assert!(outcome.is_complete(), "{phase:?}: {outcome:?}");
        assert!(read(&data_dir).unwrap().is_none(), "{phase:?}");
        if phase == Phase::AnchorFinalised {
            assert_eq!(
                fs::read_to_string(app_dir.join("public/version")).unwrap(),
                "new"
            );
        } else if matches!(
            phase,
            Phase::TreeRetained
                | Phase::ReplacementInstalled
                | Phase::LifecycleComplete
                | Phase::RegistryCommitted
        ) {
            assert_eq!(
                fs::read_to_string(app_dir.join("public/version")).unwrap(),
                "old"
            );
        }
        // `AnchorFinalised` is the one phase recovery must not reverse: the
        // registry write already committed and the anchor step means the
        // update is done, so `recover_transaction` only discards the journal
        // (update.rs's early `AnchorFinalised` branch) and rightly leaves the
        // new version in place — matching `public/version` staying "new"
        // above rather than reverting to the pre-update `entry`.
        let expected_entry = if phase == Phase::AnchorFinalised {
            RegistryEntry {
                app_version: "0.7.0".into(),
                ..entry.clone()
            }
        } else {
            entry.clone()
        };
        assert_eq!(
            registry::load(&paths).unwrap().get("demo").unwrap(),
            &expected_entry,
            "{phase:?}"
        );
    }
}

#[test]
fn journal_round_trip_tolerates_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::prepared(TransactionKind::Apply, entry());
    journal.advance(Phase::TreeRetained);
    write(dir.path(), &journal).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(journal_path(dir.path())).unwrap()).unwrap();
    value["future"] = serde_json::json!(true);
    fs::write(
        journal_path(dir.path()),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert_eq!(
        read(dir.path()).unwrap().unwrap().phase,
        Phase::TreeRetained
    );
}

#[test]
fn unknown_phase_refuses_to_load() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::prepared(TransactionKind::ResyncOnly, entry());
    write(dir.path(), &journal).unwrap();
    let path = journal_path(dir.path());
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value["phase"] = serde_json::json!("from_the_future");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read(dir.path()).is_err());
}

#[test]
fn transaction_paths_are_private_and_stable() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-root");
    let app = dir.path().join("apps/demo");
    assert_eq!(journal_path(&data), data.join("update-transaction.json"));
    assert_eq!(
        staged_db_path(&data, "app.db"),
        data.join(".update-transaction/app.db")
    );
    assert_eq!(
        staged_tree_path(&app),
        dir.path().join("apps/demo.update-transaction")
    );
}
