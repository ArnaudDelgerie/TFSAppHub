use std::{fs, path::Path};

use super::{
    append_bytes, data_dir_populated, import_decision, run_export, run_import, ImportRefusal,
    Manifest, PortabilityError, DATA_DIR, MANIFEST_FILE,
};
use crate::{
    install, lifecycle,
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

// --- run_import ----------------------------------------------------------

fn manifest_json(identifier: &str, app_version: &str) -> Vec<u8> {
    serde_json::to_vec(&manifest(identifier, app_version)).expect("a serialisable manifest")
}

fn write_test_archive(path: &Path, entries: &[(&str, &[u8])]) {
    let file = fs::File::create(path).expect("create archive file");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::fast(),
    ));
    for (name, data) in entries {
        append_bytes(&mut builder, name, data).expect("append entry");
    }
    let encoder = builder.into_inner().expect("finish tar layer");
    encoder.finish().expect("finish gzip layer");
}

/// Append an entry whose path bypasses `Header::set_path`'s own "relative, no
/// `..`" validation — `archive_tests.rs`'s own fixture trick, duplicated here
/// rather than shared: the only way to build an archive this crate's
/// *builder* would refuse to write honestly, needed to prove `import`
/// refuses one written by some other tool entirely.
fn append_raw_path(
    builder: &mut tar::Builder<flate2::write::GzEncoder<fs::File>>,
    raw_path: &str,
    content: &[u8],
) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    {
        let name = &mut header.as_old_mut().name;
        for byte in name.iter_mut() {
            *byte = 0;
        }
        let bytes = raw_path.as_bytes();
        assert!(
            bytes.len() <= name.len(),
            "fixture path too long for a raw header"
        );
        name[..bytes.len()].copy_from_slice(bytes);
    }
    header.set_cksum();
    builder.append(&header, content).expect("append raw entry");
}

#[test]
fn importing_an_unregistered_id_refuses() {
    let (base, paths) = temp_paths();
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[(MANIFEST_FILE, &manifest_json("dev.local.demo", "1.0.0"))],
    );

    let error =
        run_import(&paths, "demo", &archive, false, true).expect_err("nothing is installed");
    assert!(
        matches!(error, PortabilityError::NotInstalled { .. }),
        "{error}"
    );
}

#[test]
fn importing_a_non_archive_file_is_a_clean_refusal() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let not_an_archive = base.path().join("notes.txt");
    fs::write(&not_an_archive, b"hello, this is not a tar.gz").expect("a plain text file");

    let error =
        run_import(&paths, "demo", &not_an_archive, false, true).expect_err("not a tar.gz at all");
    assert!(
        matches!(
            error,
            PortabilityError::Io { .. } | PortabilityError::NoManifest { .. }
        ),
        "{error}"
    );
}

#[test]
fn an_archive_missing_manifest_json_is_refused() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let archive = base.path().join("no-manifest.tar.gz");
    write_test_archive(&archive, &[(&format!("{DATA_DIR}/app.db"), b"sqlite")]);

    let error =
        run_import(&paths, "demo", &archive, false, true).expect_err("no manifest.json at all");
    assert!(
        matches!(error, PortabilityError::NoManifest { .. }),
        "{error}"
    );
}

#[test]
fn importing_refuses_a_foreign_identifier() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[(MANIFEST_FILE, &manifest_json("dev.local.other", "1.2.3"))],
    );

    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("a foreign archive is always refused, even with force");
    assert!(
        matches!(
            error,
            PortabilityError::Refused(ImportRefusal::IdentifierMismatch { .. })
        ),
        "{error}"
    );
}

#[test]
fn importing_refuses_an_archive_newer_than_the_installed_app_even_with_force() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[(MANIFEST_FILE, &manifest_json("dev.local.demo", "9.9.9"))],
    );

    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("a future archive is never adopted, even with force");
    assert!(
        matches!(
            error,
            PortabilityError::Refused(ImportRefusal::ArchiveNewer { .. })
        ),
        "{error}"
    );
}

#[test]
fn importing_over_a_populated_dir_without_force_touches_nothing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"existing").expect("an existing database");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"incoming"),
        ],
    );

    let error = run_import(&paths, "demo", &archive, false, true)
        .expect_err("a populated data dir needs --force");
    assert!(
        matches!(
            error,
            PortabilityError::Refused(ImportRefusal::DataDirPopulated)
        ),
        "{error}"
    );
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("untouched"),
        b"existing"
    );
}

#[test]
fn declining_the_overwrite_confirmation_touches_nothing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"existing").expect("an existing database");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"incoming"),
        ],
    );

    // stdin is not a terminal under the test harness, and assume_yes is
    // false: `prompt::confirmed` refuses non-interactively, which is exactly
    // the decline path this exercises.
    let proceeded = run_import(&paths, "demo", &archive, true, false)
        .expect("a non-interactive decline is Ok(false), not an error");

    assert!(!proceeded);
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("untouched"),
        b"existing"
    );
}

#[test]
fn a_forced_import_rescue_dumps_the_replaced_database_and_discards_the_anchor() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"existing").expect("an existing database");
    lifecycle::write_rollback_anchor(
        &data_subdir,
        &lifecycle::RollbackAnchor {
            app_version: "1.1.0".to_string(),
            source_revision: "sha256:previous".to_string(),
            created_at: registry::now_timestamp(),
        },
    )
    .expect("a seeded anchor");
    fs::write(
        lifecycle::db_snapshot_path(&data_subdir, "app.db"),
        b"pre-update-snapshot",
    )
    .expect("a seeded db snapshot");
    let app_dir = paths.app_dir("demo").expect("an app dir");
    fs::create_dir_all(lifecycle::previous_tree_path(&app_dir)).expect("a retained tree");

    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"incoming"),
        ],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("force unlocks a populated data dir");
    assert!(proceeded);

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the new database"),
        b"incoming"
    );
    assert_eq!(
        fs::read(lifecycle::rescue_dump_path(&data_subdir, "app.db"))
            .expect("the replaced database, rescue-dumped"),
        b"existing"
    );
    assert!(
        lifecycle::read_rollback_anchor(&data_subdir).is_none(),
        "the anchor's rollback.json half must be gone"
    );
    assert!(
        !lifecycle::db_snapshot_path(&data_subdir, "app.db").is_file(),
        "the anchor's database-snapshot half must be gone"
    );
    assert!(
        !lifecycle::previous_tree_path(&app_dir).is_dir(),
        "the anchor's retained-tree half must be gone"
    );
}

#[test]
fn a_successful_import_writes_the_manifests_version_and_preserves_an_existing_port_override() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(
        data_subdir.join("config.json"),
        r#"{"version":"0.0.0","port_override":4242}"#,
    )
    .expect("a config.json with a port override, from before this app ever had a database");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"seeded"),
        ],
    );

    run_import(&paths, "demo", &archive, false, true).expect("an empty data dir needs no force");

    let config: tfsapp_core::ports::DataConfig = serde_json::from_str(
        &fs::read_to_string(data_subdir.join("config.json")).expect("config.json"),
    )
    .expect("a parseable config.json");
    assert_eq!(config.version, "1.2.3");
    assert_eq!(config.port_override, Some(4242));
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the seeded database"),
        b"seeded"
    );
}

#[test]
fn a_live_window_refuses_the_import() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let pid_file = data_dir.join("sidecar.pid");
    let _holder = tfsapp_core::process::try_lock_file(&tfsapp_core::process::lock_path(&pid_file))
        .expect("no I/O error")
        .expect("the lock is free to take");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[(MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3"))],
    );

    let error = run_import(&paths, "demo", &archive, false, true)
        .expect_err("a live window owns this data dir");
    assert!(matches!(error, PortabilityError::Busy { .. }), "{error}");
}

#[test]
fn an_active_run_command_refuses_the_import_naming_the_alias() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    fs::create_dir_all(&data_dir).expect("a data dir");
    let run_lock_path = data_dir.join("run.lock");
    let _holder = tfsapp_core::process::try_lock_file(&run_lock_path)
        .expect("no I/O error")
        .expect("the lock is free to take");
    fs::write(&run_lock_path, "migrate\n1234").expect("a run.lock record");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[(MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3"))],
    );

    let error = run_import(&paths, "demo", &archive, false, true)
        .expect_err("an active run command owns this data dir");
    let message = error.to_string();
    assert!(message.contains("migrate"), "{message}");
    assert!(message.contains("run --stop demo"), "{message}");
}

#[test]
fn an_archive_with_a_traversal_entry_is_refused_by_archives_own_checks() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let archive = base.path().join("malicious.tar.gz");
    let file = fs::File::create(&archive).expect("create archive file");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::fast(),
    ));
    append_bytes(
        &mut builder,
        MANIFEST_FILE,
        &manifest_json("dev.local.demo", "1.2.3"),
    )
    .expect("manifest entry");
    append_raw_path(&mut builder, "data/../../evil", b"pwned");
    let encoder = builder.into_inner().expect("finish tar layer");
    encoder.finish().expect("finish gzip layer");

    let error = run_import(&paths, "demo", &archive, false, true)
        .expect_err("a traversal entry is refused by archive's own checks");
    assert!(matches!(error, PortabilityError::Archive(_)), "{error}");
}

// --- import migrates an older archive forward --------------------------

/// A fixture app whose `bin/console` records what it was asked to do —
/// `install_tests.rs`'s/`update_tests.rs`'s own `runnable_app_tree`,
/// self-contained per this file's own convention (`seeded_entry`'s doc)
/// rather than imported from another module's `_tests.rs`.
fn runnable_app_tree(root: &Path, app_version: &str, commands: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{app_version}",
              "commands": {commands}
            }}"#
        ),
    )
    .expect("a manifest");
    fs::write(root.join("composer.json"), "{}").expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(
        root.join("bin/console"),
        r#"<?php
        $arguments = array_slice($argv, 1);
        file_put_contents(
            getenv('APP_LOG_DIR') . '/hooks.log',
            implode(' ', $arguments) . "\n",
            FILE_APPEND
        );
        exit(in_array('boom', $arguments, true) ? 1 : 0);
        "#,
    )
    .expect("a console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

/// The bundled interpreter and Composer, or a reason to skip — copied from
/// `install_tests.rs`/`update_tests.rs` rather than shared, exactly as their
/// own doc comments already explain: `make check` has to stay green on a
/// fresh clone where `make resources` has never run.
fn resources_present() -> bool {
    let missing: Vec<_> = [
        crate::platform::bundled_frankenphp(),
        crate::php::bundled_composer(),
    ]
    .into_iter()
    .filter(|candidates| !candidates.iter().any(|path| path.is_file()))
    .collect();

    for candidates in &missing {
        eprintln!(
            "skipped: none of {candidates:?} are there — run `make resources` to cover this one"
        );
    }
    missing.is_empty()
}

#[test]
fn an_archive_older_than_the_installed_app_is_migrated_forward_on_import() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();

    // `pre-update`/`post-update` are properties of the manifest, not of an
    // event in progress — declared once, and only ever run under
    // `LifecycleEvent::Update`, whichever command decides that is the event.
    // A plain install runs neither; import's own migrate-forward step is
    // what reaches them here.
    runnable_app_tree(
        source.path(),
        "1.3.0",
        r#"{"pre-update": ["cache:clear"], "post-update": ["about"]}"#,
    );
    install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the install succeeds")
    .expect("the user did not decline");

    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.0")),
            (&format!("{DATA_DIR}/app.db"), b"older-backup"),
        ],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("an older archive over a populated dir, forced");
    assert!(proceeded);

    let config: tfsapp_core::ports::DataConfig = serde_json::from_str(
        &fs::read_to_string(data_subdir.join("config.json")).expect("config.json"),
    )
    .expect("a parseable config.json");
    assert_eq!(
        config.version, "1.3.0",
        "the record must land on the installed version, not the archive's older one"
    );

    let log = base.path().join("TFSApp/dev.local.demo/log/hooks.log");
    assert_eq!(
        fs::read_to_string(log).expect("a hook trace"),
        "cache:clear\nabout\n",
        "the installed manifest's pre-update then post-update must run over the imported data"
    );

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the imported database"),
        b"older-backup",
        "the imported bytes are what the hooks ran against, not a fixture database"
    );
}
