use std::fs;

use crate::{
    registry::{Platform, RegistryEntry, Source, SourceKind, State},
    update_transaction::{
        journal_path, read, staged_db_path, staged_tree_path, write, Journal, Phase,
        TransactionKind,
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
