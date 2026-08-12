use std::{fs, path::Path};

use super::{
    data_dir_populated, import_decision, run_export, ImportRefusal, Manifest, PortabilityError,
    MANIFEST_FILE,
};
use crate::{
    paths::Paths,
    registry::{self, Platform, RegistryEntry, Source, SourceKind, State},
};

fn version(text: &str) -> semver::Version {
    semver::Version::parse(text).expect("a semver version")
}

fn manifest(identifier: &str, app_version: &str) -> Manifest {
    Manifest {
        identifier: identifier.to_string(),
        app_version: app_version.to_string(),
        exported_at: "2026-08-12T00:00:00Z".to_string(),
        unknown: serde_json::Map::new(),
    }
}

// --- import_decision: precedence, one refusal at a time -------------------

#[test]
fn a_foreign_identifier_is_refused_before_anything_else_is_even_checked() {
    // A future version *and* a populated dir on top of the mismatch: none of
    // that matters, identifier is checked first and force does not reach it.
    let archive = manifest("dev.local.other", "9.9.9");

    let error = import_decision(&archive, "dev.local.demo", &version("1.0.0"), true, true)
        .expect_err("a foreign archive is always refused");

    assert_eq!(
        error,
        ImportRefusal::IdentifierMismatch {
            archive: "dev.local.other".to_string(),
            installed: "dev.local.demo".to_string(),
        }
    );
}

#[test]
fn an_archive_newer_than_the_installed_app_is_refused_even_with_force() {
    let archive = manifest("dev.local.demo", "2.0.0");

    let error = import_decision(&archive, "dev.local.demo", &version("1.0.0"), true, true)
        .expect_err("a future archive is never adopted");

    assert_eq!(
        error,
        ImportRefusal::ArchiveNewer {
            archive: "2.0.0".to_string(),
            installed: "1.0.0".to_string(),
        }
    );
}

#[test]
fn a_populated_data_dir_is_refused_without_force() {
    let archive = manifest("dev.local.demo", "1.0.0");

    let error = import_decision(&archive, "dev.local.demo", &version("1.0.0"), true, false)
        .expect_err("a populated data dir needs --force");

    assert_eq!(error, ImportRefusal::DataDirPopulated);
}

#[test]
fn force_unlocks_only_the_populated_data_dir_refusal() {
    let archive = manifest("dev.local.demo", "1.0.0");

    import_decision(&archive, "dev.local.demo", &version("1.0.0"), true, true)
        .expect("force unlocks a populated data dir over a matching, non-newer archive");
}

#[test]
fn an_equal_version_is_accepted() {
    let archive = manifest("dev.local.demo", "1.0.0");

    import_decision(&archive, "dev.local.demo", &version("1.0.0"), false, false)
        .expect("an equal version imports over an empty data dir");
}

#[test]
fn an_older_version_is_accepted() {
    let archive = manifest("dev.local.demo", "0.9.0");

    import_decision(&archive, "dev.local.demo", &version("1.0.0"), false, false)
        .expect("an older archive is the ordinary case — the next launch migrates forward");
}

#[test]
fn a_matching_identifier_and_version_over_an_empty_dir_needs_no_force() {
    let archive = manifest("dev.local.demo", "1.0.0");

    import_decision(&archive, "dev.local.demo", &version("1.0.0"), false, true)
        .expect("force is harmless when nothing needs overriding");
}

// --- data_dir_populated -----------------------------------------------------

#[test]
fn a_data_dir_with_no_database_is_not_populated() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");

    assert!(!data_dir_populated(data_subdir.path()));
}

#[test]
fn a_data_dir_holding_app_db_is_populated() {
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    std::fs::write(data_subdir.path().join("app.db"), b"sqlite").expect("a fake app.db");

    assert!(data_dir_populated(data_subdir.path()));
}

#[test]
fn an_empty_directory_left_by_a_fresh_install_is_not_populated() {
    // `install` creates the data dir, empty, before anything else runs — a
    // reinstall-then-import must not be refused just because the directory
    // itself exists.
    let data_subdir = tempfile::tempdir().expect("a temp data subdir");
    std::fs::write(data_subdir.path().join("config.json"), "{}").expect("a bare config.json");

    assert!(!data_dir_populated(data_subdir.path()));
}

// --- Manifest: round trip, and an unknown key surviving it -----------------

#[test]
fn a_manifest_round_trips_through_serde() {
    let original = manifest("dev.local.demo", "1.2.3");

    let json = serde_json::to_string(&original).expect("a serialisable manifest");
    let parsed: Manifest = serde_json::from_str(&json).expect("a parseable manifest");

    assert_eq!(parsed, original);
}

#[test]
fn an_unknown_key_survives_a_manifest_round_trip() {
    // CONTRACT.md's "warn on an unknown key, never refuse", applied to this
    // project's own format: a manifest a later hub wrote is read, not
    // rejected, and what this hub could not read is carried through rather
    // than dropped.
    let json = r#"{
        "identifier": "dev.local.demo",
        "app_version": "1.2.3",
        "exported_at": "2026-08-12T00:00:00Z",
        "checksum": "sha256:deadbeef"
    }"#;

    let parsed: Manifest = serde_json::from_str(json).expect("an otherwise-valid manifest");

    assert_eq!(
        parsed.unknown.get("checksum"),
        Some(&serde_json::Value::from("sha256:deadbeef"))
    );

    let rewritten = serde_json::to_string(&parsed).expect("a re-serialisable manifest");
    let value: serde_json::Value = serde_json::from_str(&rewritten).expect("valid JSON");
    assert_eq!(value["checksum"].as_str(), Some("sha256:deadbeef"));
}

// --- run_export --------------------------------------------------------

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A registry entry recording `dev.local.demo` at `1.2.3` — self-contained
/// per this crate's own convention rather than imported from another
/// module's `_tests.rs`.
fn seeded_entry() -> RegistryEntry {
    RegistryEntry {
        id: "demo".to_string(),
        identifier: "dev.local.demo".to_string(),
        source: Source {
            kind: SourceKind::LocalPath,
            location: "/dev/null".to_string(),
            reference: None,
            reference_kind: None,
            index: None,
        },
        app_version: "1.2.3".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        app_port: None,
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "a1b2c3d4".repeat(8),
        },
        state: State::Ready,
        installed_at: registry::now_timestamp(),
        updated_at: registry::now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

fn seed_registry(paths: &Paths, entry: RegistryEntry) {
    registry::update(paths, |registry| registry.upsert(entry)).expect("a seeded registry");
}

/// Every entry's path in the `.tar.gz` at `path`, in archive order — the
/// same check `tar tzf` performs by eye, made assertable.
fn archive_entries(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).expect("the archive opens");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    archive
        .entries()
        .expect("a readable archive")
        .map(|entry| {
            entry
                .expect("a readable entry")
                .path()
                .expect("a readable path")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn exporting_an_unregistered_id_refuses() {
    let (base, paths) = temp_paths();
    let target = base.path().join("backup.tar.gz");

    let error = run_export(&paths, "demo", &target).expect_err("nothing is installed");
    assert!(
        matches!(error, PortabilityError::NotInstalled { .. }),
        "{error}"
    );
}

#[test]
fn exporting_never_overwrites_an_existing_target() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let target = base.path().join("backup.tar.gz");
    fs::write(&target, b"already here").expect("a pre-existing file at the target");

    let error = run_export(&paths, "demo", &target).expect_err("the target already exists");
    assert!(
        matches!(error, PortabilityError::TargetExists { .. }),
        "{error}"
    );
    assert_eq!(fs::read(&target).expect("still there"), b"already here");
}

#[test]
fn an_app_with_no_database_yet_exports_the_manifest_alone() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("nothing blocks a never-opened app");

    assert_eq!(archive_entries(&target), vec![MANIFEST_FILE.to_string()]);
    assert!(!target.with_extension("tmp").exists());
}

#[test]
fn an_app_with_a_database_exports_it_under_data_alongside_its_wal_twin() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"sqlite").expect("a fake app.db");
    fs::write(data_subdir.join("app.db-wal"), b"wal").expect("a fake wal");
    // Not exported: excluded from `lifecycle::DB_FILE_NAMES` and from the
    // curated set on purpose (the Overview's "what does not travel").
    fs::write(data_subdir.join("secrets.json"), b"{}").expect("a fake secrets.json");
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("a quiescent database exports cleanly");

    let mut entries = archive_entries(&target);
    entries.sort();
    assert_eq!(
        entries,
        vec![
            "data/app.db".to_string(),
            "data/app.db-wal".to_string(),
            MANIFEST_FILE.to_string(),
        ]
    );
}

#[test]
fn a_manifest_written_by_export_names_the_registry_entry_not_the_hub() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("nothing blocks a never-opened app");

    let file = fs::File::open(&target).expect("the archive opens");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut entry = archive
        .entries()
        .expect("a readable archive")
        .next()
        .expect("at least one entry")
        .expect("a readable entry");
    let mut contents = String::new();
    std::io::Read::read_to_string(&mut entry, &mut contents).expect("readable manifest bytes");
    let manifest: Manifest = serde_json::from_str(&contents).expect("a parseable manifest");

    assert_eq!(manifest.identifier, "dev.local.demo");
    assert_eq!(manifest.app_version, "1.2.3");
}

#[test]
fn a_live_window_refuses_the_export() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let pid_file = data_dir.join("sidecar.pid");
    // Held in this test's own process, the same fake `install_tests.rs` uses
    // for its own busy-guard tests: `flock` is per open file description, so
    // a second, fresh open of the same path still sees it held.
    let _holder = tfsapp_core::process::try_lock_file(&tfsapp_core::process::lock_path(&pid_file))
        .expect("no I/O error")
        .expect("the lock is free to take");
    let target = base.path().join("backup.tar.gz");

    let error = run_export(&paths, "demo", &target).expect_err("a live window owns this data dir");

    assert!(matches!(error, PortabilityError::Busy { .. }), "{error}");
    assert!(error.to_string().contains("demo"), "{error}");
    assert!(!target.exists());
}

#[test]
fn an_active_run_command_refuses_the_export_naming_the_alias() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let run_lock_path = data_dir.join("run.lock");
    let _holder = tfsapp_core::process::try_lock_file(&run_lock_path)
        .expect("no I/O error")
        .expect("the lock is free to take");
    fs::write(&run_lock_path, "migrate\n1234").expect("a run.lock record");
    let target = base.path().join("backup.tar.gz");

    let error =
        run_export(&paths, "demo", &target).expect_err("an active run command owns this data dir");

    let message = error.to_string();
    assert!(message.contains("migrate"), "{message}");
    assert!(message.contains("run --stop demo"), "{message}");
}

#[test]
fn a_crashed_instance_does_not_block_export() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    // A data dir with no live lock on either file — precisely what a crashed
    // instance leaves behind.
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("a crashed instance must not block export");
}
