use std::{fs, path::Path};

use super::{
    append_bytes, data_dir_populated, export_temp_path, import_decision, run_export, run_import,
    write_archive, ImportRefusal, Manifest, PortabilityError, DATA_DIR, MANIFEST_FILE, UPLOADS_DIR,
};
use crate::{
    install, lifecycle, lifecycle_gate,
    paths::Paths,
    registry::{self, Platform, RegistryEntry, Source, SourceKind, State},
    run,
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
fn a_data_dir_with_no_database_and_no_uploads_is_not_populated() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");

    assert!(!data_dir_populated(data_dir.path()));
}

#[test]
fn a_data_dir_holding_app_db_is_populated() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    fs::create_dir_all(data_dir.path().join(DATA_DIR)).expect("a data subdir");
    fs::write(data_dir.path().join(DATA_DIR).join("app.db"), b"sqlite").expect("a fake app.db");

    assert!(data_dir_populated(data_dir.path()));
}

#[test]
fn a_data_dir_holding_only_an_upload_is_populated() {
    // decision 006: a durable file is exactly as much the user's data as the
    // database that indexes it — an app with uploads and no database yet
    // must still be refused without --force.
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    fs::create_dir_all(data_dir.path().join(UPLOADS_DIR)).expect("an uploads dir");
    fs::write(
        data_dir.path().join(UPLOADS_DIR).join("avatar.png"),
        b"an upload with no database behind it yet",
    )
    .expect("a fake upload");

    assert!(data_dir_populated(data_dir.path()));
}

#[test]
fn an_empty_uploads_directory_is_not_populated() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    fs::create_dir_all(data_dir.path().join(UPLOADS_DIR)).expect("an empty uploads dir");

    assert!(!data_dir_populated(data_dir.path()));
}

#[test]
fn an_empty_directory_left_by_a_fresh_install_is_not_populated() {
    // `install` creates both the data dir and `uploads/`, empty, before
    // anything else runs — a reinstall-then-import must not be refused just
    // because the directories themselves exist.
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    fs::create_dir_all(data_dir.path().join(DATA_DIR)).expect("a data subdir");
    fs::write(data_dir.path().join(DATA_DIR).join("config.json"), "{}")
        .expect("a bare config.json");

    assert!(!data_dir_populated(data_dir.path()));
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

// --- import invalidates derived state: shared sentinels (plan 052) ------

/// Seed the destination's derived state — a stamp plus one sentinel in each
/// disposable directory — the fixture every preflight-refusal test asserts a
/// *refused* import must leave exactly where it was.
fn seed_derived_sentinels(data_dir: &Path, data_subdir: &Path) {
    let cache_dir = data_dir.join("cache");
    let build_dir = data_dir.join("build");
    fs::create_dir_all(&cache_dir).expect("a cache dir");
    fs::create_dir_all(&build_dir).expect("a build dir");
    fs::write(
        cache_dir.join("sentinel"),
        b"derived from the current database",
    )
    .expect("a cache sentinel");
    fs::write(
        build_dir.join("sentinel"),
        b"derived from the current database",
    )
    .expect("a build sentinel");
    fs::create_dir_all(data_subdir).expect("a data subdir");
    lifecycle::write_cache_stamp(
        data_subdir,
        &lifecycle::CacheStamp {
            app_version: "1.2.3".to_string(),
            snapshot_path: "/apps/demo".to_string(),
            platform: Platform {
                php_version: "8.5".to_string(),
                extensions_hash: "a1b2c3d4".repeat(8),
            },
        },
    )
    .expect("a cache stamp");
}

/// The counterpart to [`seed_derived_sentinels`]: every sentinel and the
/// stamp still present, byte for byte.
fn assert_derived_state_retained(data_dir: &Path, data_subdir: &Path) {
    assert_eq!(
        fs::read(data_dir.join("cache/sentinel")).expect("the cache sentinel"),
        b"derived from the current database"
    );
    assert_eq!(
        fs::read(data_dir.join("build/sentinel")).expect("the build sentinel"),
        b"derived from the current database"
    );
    assert!(
        lifecycle::cache_stamp_path(data_subdir).exists(),
        "a refused import must not discard the destination's stamp"
    );
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
fn a_held_maintenance_lease_refuses_export_before_it_creates_the_archive() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let target = base.path().join("backup.tar.gz");
    let _held = lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "update")
        .expect("the first maintenance command owns the gate");

    let error = run_export(&paths, "demo", &target)
        .expect_err("export must not overlap another maintenance command");

    assert!(matches!(error, PortabilityError::Gate(_)), "{error}");
    assert!(error.to_string().contains("update"), "{error}");
    assert!(!target.exists(), "a refused export leaves no archive");
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
    assert!(!export_temp_path(&target).exists());
}

#[test]
fn an_export_temp_uses_the_complete_target_filename() {
    let target = Path::new("backup.tar.gz");

    assert_eq!(export_temp_path(target), Path::new("backup.tar.gz.tmp"));
}

#[test]
fn exporting_refuses_a_leftover_temp_file_without_touching_it() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let target = base.path().join("backup.tar.gz");
    let temporary = export_temp_path(&target);
    fs::write(&temporary, b"unfinished archive").expect("a leftover temp file");

    let error = run_export(&paths, "demo", &target).expect_err("the temp file must be kept");

    assert!(
        matches!(error, PortabilityError::TemporaryExists { .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains("left over from a failed export"),
        "{error}"
    );
    assert_eq!(
        fs::read(&temporary).expect("the leftover remains"),
        b"unfinished archive"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_export_removes_its_temp_file() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    // `/proc/self/mem` presents as a regular file but reading it reliably
    // fails, which makes the archive fail only after its temp was created.
    std::os::unix::fs::symlink("/proc/self/mem", data_subdir.join("app.db"))
        .expect("a deliberately unreadable database");
    let target = base.path().join("backup.tar.gz");
    let temporary = export_temp_path(&target);

    let error = run_export(&paths, "demo", &target).expect_err("the database cannot be read");

    assert!(matches!(error, PortabilityError::Io { .. }), "{error}");
    assert!(
        !temporary.exists(),
        "failed exports clean up their temp file"
    );
    assert!(!target.exists(), "the requested target remains untouched");
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

// --- run_export carries uploads/ (plan 049 / decision 006) -----------------

#[test]
fn a_nested_upload_tree_round_trips_into_the_archive_with_its_structure_intact() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let uploads_dir = base.path().join("TFSApp/dev.local.demo/uploads");
    fs::create_dir_all(uploads_dir.join("invoices")).expect("a nested uploads dir");
    fs::write(uploads_dir.join("avatar.png"), b"top-level file").expect("a top-level upload");
    fs::write(uploads_dir.join("invoices/2026-01.pdf"), b"nested file").expect("a nested upload");
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("a nested upload tree exports cleanly");

    let mut entries = archive_entries(&target);
    entries.sort();
    assert_eq!(
        entries,
        vec![
            MANIFEST_FILE.to_string(),
            format!("{UPLOADS_DIR}/avatar.png"),
            format!("{UPLOADS_DIR}/invoices/2026-01.pdf"),
        ]
    );
}

#[test]
fn an_absent_uploads_directory_exports_cleanly() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("no uploads/ at all is the ordinary case");

    assert_eq!(archive_entries(&target), vec![MANIFEST_FILE.to_string()]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_symlink_under_uploads_is_skipped_rather_than_followed() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let uploads_dir = base.path().join("TFSApp/dev.local.demo/uploads");
    fs::create_dir_all(&uploads_dir).expect("an uploads dir");
    fs::write(uploads_dir.join("real.txt"), b"a real file").expect("a real upload");
    std::os::unix::fs::symlink("/etc/hostname", uploads_dir.join("link.txt"))
        .expect("a symlink planted under uploads");
    let target = base.path().join("backup.tar.gz");

    run_export(&paths, "demo", &target).expect("a symlink must not fail the export");

    assert_eq!(
        archive_entries(&target),
        vec![MANIFEST_FILE.to_string(), format!("{UPLOADS_DIR}/real.txt")],
        "the symlink travels in neither name nor content"
    );
}

#[test]
fn two_exports_of_the_same_upload_tree_are_byte_identical() {
    let base = tempfile::tempdir().expect("a temp base");
    let data_subdir = base.path().join("data");
    let uploads_dir = base.path().join("uploads");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::create_dir_all(uploads_dir.join("a")).expect("a nested uploads dir");
    fs::write(uploads_dir.join("a/one.txt"), b"one").expect("a nested upload");
    fs::write(uploads_dir.join("two.txt"), b"two").expect("a top-level upload");
    let archive_manifest = manifest("dev.local.demo", "1.2.3");

    let first = base.path().join("first.tar.gz");
    write_archive(&first, &archive_manifest, &data_subdir, &uploads_dir)
        .expect("the first export succeeds");
    let second = base.path().join("second.tar.gz");
    write_archive(&second, &archive_manifest, &data_subdir, &uploads_dir)
        .expect("the second export succeeds");

    assert_eq!(
        fs::read(&first).expect("the first archive"),
        fs::read(&second).expect("the second archive"),
        "two exports of identical content must produce byte-identical archives"
    );
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
    let runs_dir = data_dir.join("runs");
    fs::create_dir_all(&runs_dir).expect("a runs dir");
    let entry_path = runs_dir.join("1.lock");
    let _holder = tfsapp_core::process::try_lock_file(&entry_path)
        .expect("no I/O error")
        .expect("the lock is free to take");
    fs::write(&entry_path, "migrate\n1234").expect("an entry record");
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
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    seed_derived_sentinels(&data_dir, &data_subdir);
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
    assert_derived_state_retained(&data_dir, &data_subdir);
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
fn an_oversized_manifest_is_refused_before_touching_the_data_directory() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"original database").expect("database sentinel");
    seed_derived_sentinels(&data_dir, &data_subdir);

    let mut oversized = manifest_json("dev.local.demo", "1.2.3");
    oversized.extend(vec![b' '; 1024 * 1024 + 1]);
    let archive = base.path().join("oversized-manifest.tar.gz");
    write_test_archive(&archive, &[(MANIFEST_FILE, &oversized)]);

    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("manifest over 1 MiB must be refused");
    assert!(
        matches!(error, PortabilityError::MalformedManifest { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("1 MiB"), "{error}");
    assert_eq!(
        fs::read(data_subdir.join("app.db")).unwrap(),
        b"original database"
    );
    assert_derived_state_retained(&data_dir, &data_subdir);
}

#[test]
fn import_refuses_an_over_budget_payload_before_replacing_any_state() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).unwrap();
    let app_dir = paths.app_dir("demo").unwrap();
    seed_persistent_state(&data_dir, &data_subdir, &app_dir);
    seed_derived_sentinels(&data_dir, &data_subdir);
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            ("data/app.db", b"new database"),
        ],
    );
    let error = crate::disk_space::with_available_bytes(crate::disk_space::MARGIN + 20, || {
        run_import(&paths, "demo", &archive, true, false).expect_err("too little space")
    });
    assert!(
        matches!(error, PortabilityError::Preflight { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("before changing anything"));
    assert_persistent_state_unchanged(&data_dir, &data_subdir, &app_dir);
    assert_derived_state_retained(&data_dir, &data_subdir);
}

#[test]
fn import_accepts_a_payload_just_inside_the_space_budget() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            ("data/app.db", b"new database"),
        ],
    );
    let payload = crate::archive::check_payload(&archive, u64::MAX).unwrap();
    let imported =
        crate::disk_space::with_available_bytes(crate::disk_space::MARGIN + payload + 1, || {
            run_import(&paths, "demo", &archive, false, true).expect("one spare byte suffices")
        });
    assert!(imported);
    let data_dir = base.path().join("TFSApp/dev.local.demo/data");
    assert_eq!(fs::read(data_dir.join("app.db")).unwrap(), b"new database");
}

#[test]
fn importing_refuses_a_foreign_identifier() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    seed_derived_sentinels(&data_dir, &data_subdir);
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
    assert!(
        !error.to_string().contains("rescue"),
        "a refusal before extraction must not imply data was moved aside"
    );
    assert_derived_state_retained(&data_dir, &data_subdir);
}

#[test]
fn importing_refuses_an_archive_newer_than_the_installed_app_even_with_force() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    seed_derived_sentinels(&data_dir, &data_subdir);
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
    assert_derived_state_retained(&data_dir, &data_subdir);
}

#[test]
fn importing_over_a_populated_dir_without_force_touches_nothing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"existing").expect("an existing database");
    seed_derived_sentinels(&data_dir, &data_subdir);
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
    assert_derived_state_retained(&data_dir, &data_subdir);
}

#[test]
fn declining_the_overwrite_confirmation_touches_nothing() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"existing").expect("an existing database");
    seed_derived_sentinels(&data_dir, &data_subdir);
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
    assert_derived_state_retained(&data_dir, &data_subdir);
    assert!(
        lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "export").is_ok(),
        "a declined confirmation releases its maintenance lease"
    );
}

#[test]
fn interleaved_forced_imports_cannot_overwrite_the_first_rescue_dump() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(
        data_subdir.join("app.db"),
        b"first import's outgoing database",
    )
    .expect("an existing database");
    let rescue = data_subdir.join("app.db.rescue-first-import");
    fs::write(&rescue, b"first import's rescue dump").expect("the first rescue dump");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"second import's database"),
        ],
    );

    // The held lease represents the first forced import after it has created
    // its rescue dump. A second import must stop before it can copy or remove
    // any database member.
    let held = lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "import")
        .expect("the first import owns the gate");
    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("the interleaved import must refuse");

    assert!(matches!(error, PortabilityError::Gate(_)), "{error}");
    assert_eq!(
        fs::read(&rescue).expect("the first rescue dump survives"),
        b"first import's rescue dump"
    );
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the live database survives"),
        b"first import's outgoing database"
    );
    drop(held);
    assert!(
        lifecycle_gate::acquire_maintenance(&paths, "dev.local.demo", "export").is_ok(),
        "the refusal must not retain a competing handle"
    );
}

#[test]
fn a_forced_import_rescue_dumps_the_replaced_database_and_discards_the_anchor() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_subdir = base.path().join("TFSApp/dev.local.demo/data");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"existing").expect("an existing database");
    fs::write(data_subdir.join("app.db-wal"), b"stale transactions")
        .expect("a stale WAL beside the database");
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
    let rescue = fs::read_dir(&data_subdir)
        .expect("the data directory")
        .map(|entry| entry.expect("a directory entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("app.db.rescue-"))
        })
        .expect("the replaced database, rescue-dumped");
    assert_eq!(fs::read(rescue).expect("read the rescue dump"), b"existing");
    let rescued_wal = fs::read_dir(&data_subdir)
        .expect("the data directory")
        .map(|entry| entry.expect("a directory entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("app.db-wal.rescue-"))
        })
        .expect("the stale WAL must be rescued too");
    assert_eq!(
        fs::read(rescued_wal).expect("read the rescued WAL"),
        b"stale transactions"
    );
    assert!(
        !data_subdir.join("app.db-wal").exists() && !data_subdir.join("app.db-shm").exists(),
        "the archive supplied neither twin, so no stale SQLite side file may survive it"
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
    seed_derived_sentinels(&data_dir, &data_dir.join(DATA_DIR));
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
    assert_derived_state_retained(&data_dir, &data_dir.join(DATA_DIR));
}

#[test]
fn an_active_run_command_refuses_the_import_naming_the_alias() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let runs_dir = data_dir.join("runs");
    fs::create_dir_all(&runs_dir).expect("a runs dir");
    seed_derived_sentinels(&data_dir, &data_dir.join(DATA_DIR));
    let entry_path = runs_dir.join("1.lock");
    let _holder = tfsapp_core::process::try_lock_file(&entry_path)
        .expect("no I/O error")
        .expect("the lock is free to take");
    fs::write(&entry_path, "migrate\n1234").expect("an entry record");
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
    assert_derived_state_retained(&data_dir, &data_dir.join(DATA_DIR));
}

#[test]
fn an_archive_with_a_traversal_entry_is_refused_by_archives_own_checks() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    seed_derived_sentinels(&data_dir, &data_subdir);
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
    assert!(
        matches!(error, PortabilityError::Preflight { .. }),
        "{error}"
    );
    assert_derived_state_retained(&data_dir, &data_subdir);
}

// --- import restores uploads/, rescuing what it replaces by rename
// (plan 049 / decision 006) --------------------------------------------

#[test]
fn a_round_trip_through_export_then_import_restores_a_nested_upload_tree_exactly() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let uploads_dir = base.path().join("TFSApp/dev.local.demo/uploads");
    fs::create_dir_all(uploads_dir.join("invoices")).expect("a nested uploads dir");
    fs::write(uploads_dir.join("avatar.png"), b"avatar bytes").expect("a top-level upload");
    fs::write(uploads_dir.join("invoices/2026-01.pdf"), b"invoice bytes").expect("a nested upload");
    let archive = base.path().join("backup.tar.gz");
    run_export(&paths, "demo", &archive).expect("export succeeds");
    fs::remove_dir_all(&uploads_dir).expect("empty the data dir before reimporting");

    run_import(&paths, "demo", &archive, false, true).expect("an empty data dir needs no force");

    assert_eq!(
        fs::read(uploads_dir.join("avatar.png")).expect("the top-level upload restored"),
        b"avatar bytes"
    );
    assert_eq!(
        fs::read(uploads_dir.join("invoices/2026-01.pdf")).expect("the nested upload restored"),
        b"invoice bytes"
    );
}

#[test]
fn a_forced_import_over_a_populated_uploads_directory_rescues_it_by_rename_and_writes_the_new_tree()
{
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let uploads_dir = data_dir.join(UPLOADS_DIR);
    fs::create_dir_all(&uploads_dir).expect("an uploads dir");
    fs::write(uploads_dir.join("old.txt"), b"old upload").expect("an existing upload");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{UPLOADS_DIR}/new.txt"), b"new upload"),
        ],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("force unlocks a populated uploads/ directory");
    assert!(proceeded);

    assert_eq!(
        fs::read(uploads_dir.join("new.txt")).expect("the archive's upload"),
        b"new upload"
    );
    assert!(
        !uploads_dir.join("old.txt").exists(),
        "the old tree was moved aside, not merged with the archive's"
    );
    let rescue_dir = fs::read_dir(&data_dir)
        .expect("the data directory")
        .map(|entry| entry.expect("a directory entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("uploads.rescue-"))
        })
        .expect("the replaced uploads/ directory, rescue-renamed");
    assert_eq!(
        fs::read(rescue_dir.join("old.txt")).expect("the rescued upload"),
        b"old upload"
    );
}

#[test]
fn an_archive_with_no_uploads_entries_empties_the_destination_and_rescues_the_old_one() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let uploads_dir = data_dir.join(UPLOADS_DIR);
    fs::create_dir_all(&uploads_dir).expect("an uploads dir");
    fs::write(uploads_dir.join("old.txt"), b"old upload").expect("an existing upload");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[(MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3"))],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("force unlocks a populated uploads/ directory even with nothing to replace it");
    assert!(proceeded);

    assert_eq!(
        fs::read_dir(&uploads_dir)
            .expect("uploads/ exists, empty")
            .count(),
        0,
        "an archive carrying no uploads/ entries leaves the destination empty, not merged"
    );
    assert!(
        fs::read_dir(&data_dir)
            .expect("the data directory")
            .any(|entry| entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .starts_with("uploads.rescue-")),
        "the previous uploads/ tree must still be recoverable"
    );
}

#[test]
fn a_pre_049_archive_carrying_no_uploads_prefix_at_all_imports_without_error() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"seeded"),
        ],
    );

    run_import(&paths, "demo", &archive, false, true)
        .expect("an archive written before plan 049 carries no uploads/ prefix at all");

    assert_eq!(
        fs::read(data_dir.join("data/app.db")).expect("the seeded database"),
        b"seeded"
    );
    assert!(
        data_dir.join(UPLOADS_DIR).is_dir(),
        "import still creates an empty uploads/ for a legacy archive"
    );
}

// --- import invalidates the destination's derived state (plan 052) -----

/// The previous database's derived state, in the shape the launch cache
/// policy manages: a stamp matching every dimension it compares (the database
/// is not one of them) and nested sentinels in both disposable directories.
#[test]
fn an_equal_version_import_clears_cache_build_and_stamp_before_replacing_data() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    let cache_dir = data_dir.join("cache");
    let build_dir = data_dir.join("build");
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"previous database").expect("an existing database");
    lifecycle::write_cache_stamp(
        &data_subdir,
        &lifecycle::CacheStamp {
            app_version: "1.2.3".to_string(),
            snapshot_path: "/apps/demo".to_string(),
            platform: Platform {
                php_version: "8.5".to_string(),
                extensions_hash: "a1b2c3d4".repeat(8),
            },
        },
    )
    .expect("a stamp that matches on every dimension the launch compares");
    fs::create_dir_all(cache_dir.join("pooled/container")).expect("a nested cache tree");
    fs::write(
        cache_dir.join("pooled/container/ProjectContainer.php"),
        b"compiled from the previous database",
    )
    .expect("a compiled container");
    fs::create_dir_all(build_dir.join("nested")).expect("a nested build tree");
    fs::write(
        build_dir.join("nested/sentinel"),
        b"derived from the previous database",
    )
    .expect("a build sentinel");
    let uploads_dir = data_dir.join(UPLOADS_DIR);
    fs::create_dir_all(&uploads_dir).expect("an uploads dir");
    fs::write(uploads_dir.join("old.txt"), b"old upload").expect("an existing upload");
    fs::write(
        data_subdir.join("secrets.json"),
        br#"{"app_secret":"kept"}"#,
    )
    .expect("a plaintext secrets fallback");

    // The same version as the registry entry — exactly the restore that
    // changes none of the stamp's dimensions, which is why the cleanup cannot
    // lean on the launch-time comparison and must be the import's own.
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"imported database"),
            (&format!("{UPLOADS_DIR}/new.txt"), b"new upload"),
        ],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("force unlocks a populated data dir at the same version");
    assert!(proceeded);

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the imported database"),
        b"imported database"
    );
    assert!(
        !cache_dir.exists() && !build_dir.exists(),
        "both disposable directories are gone outright, nested sentinels included"
    );
    assert!(
        !lifecycle::cache_stamp_path(&data_subdir).exists(),
        "the old stamp must not survive next to the data it no longer vouches for"
    );
    assert_eq!(
        fs::read(uploads_dir.join("new.txt")).expect("the archive's upload"),
        b"new upload",
        "uploads replacement is unaffected by the cache cleanup"
    );
    let rescue_db = fs::read_dir(&data_subdir)
        .expect("the data directory")
        .map(|entry| entry.expect("a directory entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("app.db.rescue-"))
        })
        .expect("the replaced database, rescue-dumped");
    assert_eq!(
        fs::read(rescue_db).expect("the rescued database"),
        b"previous database"
    );
    let rescue_uploads = fs::read_dir(&data_dir)
        .expect("the data directory")
        .map(|entry| entry.expect("a directory entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("uploads.rescue-"))
        })
        .expect("the replaced uploads/ directory, rescue-renamed");
    assert_eq!(
        fs::read(rescue_uploads.join("old.txt")).expect("the rescued upload"),
        b"old upload"
    );
    assert_eq!(
        fs::read_to_string(data_subdir.join("secrets.json")).expect("the secrets fallback"),
        r#"{"app_secret":"kept"}"#,
        "the cleanup reaches neither data/ nor the keyring fallback inside it"
    );
}

/// A destination with no cache directories and no stamp — a fresh install
/// that has never been opened — is the ordinary absent-path case, not an
/// error, and the equal-version path stays independent of bundled PHP.
#[test]
fn an_accepted_import_over_a_destination_with_no_cache_or_stamp_imports_cleanly() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    fs::write(data_subdir.join("app.db"), b"previous database").expect("an existing database");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"imported database"),
        ],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("nothing about a missing cache can refuse an accepted import");
    assert!(proceeded);

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the imported database"),
        b"imported database"
    );
    assert!(
        !lifecycle::cache_stamp_path(&data_subdir).exists(),
        "an equal-version import warms nothing and stamps nothing — the next launch rebuilds"
    );
    assert!(
        !data_dir.join("cache").exists() && !data_dir.join("build").exists(),
        "the cleanup must not recreate what it exists to remove"
    );
}

// --- a cleanup failure stops the import before any data changes ---------

/// The destination's persistent state — everything a failed cleanup must
/// leave exactly where it was: the database, `uploads/`, the version record
/// and the rollback anchor's three halves.
fn seed_persistent_state(data_dir: &Path, data_subdir: &Path, app_dir: &Path) {
    fs::write(data_subdir.join("app.db"), b"the live database").expect("an existing database");
    let uploads_dir = data_dir.join(UPLOADS_DIR);
    fs::create_dir_all(&uploads_dir).expect("an uploads dir");
    fs::write(uploads_dir.join("old.txt"), b"an existing upload").expect("an existing upload");
    lifecycle::write_data_version(data_subdir, "1.2.3").expect("a version record");
    lifecycle::write_rollback_anchor(
        data_subdir,
        &lifecycle::RollbackAnchor {
            app_version: "1.1.0".to_string(),
            source_revision: "sha256:previous".to_string(),
            created_at: registry::now_timestamp(),
        },
    )
    .expect("a seeded anchor");
    fs::write(
        lifecycle::db_snapshot_path(data_subdir, "app.db"),
        b"pre-update-snapshot",
    )
    .expect("a seeded db snapshot");
    fs::create_dir_all(lifecycle::previous_tree_path(app_dir)).expect("a retained tree");
}

/// The counterpart to [`seed_persistent_state`], plus the two things that
/// prove the failure stopped *before* the rescue/replacement boundary: no
/// rescue dump anywhere, and the disposable directories untouched by any
/// later phase.
fn assert_persistent_state_unchanged(data_dir: &Path, data_subdir: &Path, app_dir: &Path) {
    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the database"),
        b"the live database"
    );
    assert_eq!(
        fs::read(data_dir.join(UPLOADS_DIR).join("old.txt")).expect("the upload"),
        b"an existing upload"
    );
    assert_eq!(
        lifecycle::read_data_version(data_subdir).expect("the version record"),
        Some("1.2.3".to_string())
    );
    assert!(
        lifecycle::read_rollback_anchor(data_subdir).is_some(),
        "the anchor's rollback.json half must be untouched"
    );
    assert!(
        lifecycle::db_snapshot_path(data_subdir, "app.db").is_file(),
        "the anchor's database-snapshot half must be untouched"
    );
    assert!(
        lifecycle::previous_tree_path(app_dir).is_dir(),
        "the anchor's retained-tree half must be untouched"
    );
    for directory in [data_subdir, data_dir] {
        assert!(
            !fs::read_dir(directory)
                .expect("a readable directory")
                .any(|entry| {
                    entry
                        .expect("a directory entry")
                        .file_name()
                        .to_string_lossy()
                        .contains(".rescue-")
                }),
            "rescue and extraction must not have started"
        );
    }
}

#[test]
fn a_stamp_that_cannot_be_removed_stops_the_import_before_any_data_changes() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    let app_dir = paths.app_dir("demo").expect("an app dir");
    seed_persistent_state(&data_dir, &data_subdir, &app_dir);
    // Seeded by hand rather than `seed_derived_sentinels`: the fixture here
    // is a stamp that is a *directory*, which the helper's file-writing
    // cannot produce. `cache.json` as a non-empty directory makes
    // `remove_file` fail with EISDIR whatever the privileges, so the failure
    // is deterministic even when the suite runs as root — permission bits
    // would not be.
    fs::create_dir_all(data_dir.join("cache")).expect("a cache dir");
    fs::create_dir_all(data_dir.join("build")).expect("a build dir");
    fs::write(
        data_dir.join("cache/sentinel"),
        b"derived from the current database",
    )
    .expect("a cache sentinel");
    fs::write(
        data_dir.join("build/sentinel"),
        b"derived from the current database",
    )
    .expect("a build sentinel");
    fs::create_dir_all(data_subdir.join("cache.json/unremovable"))
        .expect("a stamp that cannot be removed");

    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"incoming"),
        ],
    );

    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("a stamp that cannot be discarded stops the import");
    assert!(
        matches!(error, PortabilityError::CacheCleanup { .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("cache.json"), "{message}");
    assert!(message.contains("before replacing any data"), "{message}");
    // The stamp removal itself is what failed here, so the message must not
    // claim the stamp is already discarded — that would tell the next launch
    // the old container is reusable when it never got that far.
    assert!(
        message.contains("the cache stamp itself could not be discarded"),
        "{message}"
    );
    assert!(!message.contains("is already discarded"), "{message}");
    assert_persistent_state_unchanged(&data_dir, &data_subdir, &app_dir);
    // The stamp removal is the cleanup's first step, so neither disposable
    // directory has been touched yet either.
    assert_derived_state_retained(&data_dir, &data_subdir);
}

#[test]
fn an_unremovable_cache_directory_stops_the_import_with_the_stamp_already_gone() {
    let (base, paths) = temp_paths();
    seed_registry(&paths, seeded_entry());
    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    fs::create_dir_all(&data_subdir).expect("a data subdir");
    let app_dir = paths.app_dir("demo").expect("an app dir");
    seed_persistent_state(&data_dir, &data_subdir, &app_dir);
    // `cache` as a regular file: `remove_dir_all` on one fails with ENOTDIR
    // whatever the privileges — the same root-proof determinism as the
    // stamp-as-directory trick above, one step later in the cleanup.
    fs::write(data_dir.join("cache"), b"not a directory")
        .expect("a cache path that cannot be removed");
    let build_dir = data_dir.join("build");
    fs::create_dir_all(&build_dir).expect("a build dir");
    fs::write(
        build_dir.join("sentinel"),
        b"derived from the current database",
    )
    .expect("a build sentinel");
    lifecycle::write_cache_stamp(
        &data_subdir,
        &lifecycle::CacheStamp {
            app_version: "1.2.3".to_string(),
            snapshot_path: "/apps/demo".to_string(),
            platform: Platform {
                php_version: "8.5".to_string(),
                extensions_hash: "a1b2c3d4".repeat(8),
            },
        },
    )
    .expect("a cache stamp");

    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.3")),
            (&format!("{DATA_DIR}/app.db"), b"incoming"),
        ],
    );

    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("an unremovable cache directory stops the import");
    assert!(
        matches!(error, PortabilityError::CacheCleanup { .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(
        message.contains(data_dir.join("cache").display().to_string().as_str()),
        "{message}"
    );
    assert!(message.contains("before replacing any data"), "{message}");
    // The stamp was discarded before the directory cleanup failed, and the
    // failure must not resurrect it: a partial cleanup claiming the old
    // container is reusable is the one state worse than the error itself.
    assert!(
        !lifecycle::cache_stamp_path(&data_subdir).exists(),
        "the stamp must remain absent once discarded"
    );
    assert_eq!(
        fs::read(build_dir.join("sentinel")).expect("the build sentinel"),
        b"derived from the current database",
        "the cleanup stops at its first failure — build/ is left in place"
    );
    assert_persistent_state_unchanged(&data_dir, &data_subdir, &app_dir);
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
              "commands": {commands},
              "run": {{"read": {{"command": "setting:read"}}}}
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
        $line = implode(' ', $arguments);
        $cache_dir = getenv('APP_CACHE_DIR');
        $build_dir = getenv('APP_BUILD_DIR');
        // `probe-derived-state`: what a lifecycle command sees of the
        // destination's disposable state — plan 052's proof that import
        // cleared it before any application command ran.
        if (in_array('probe-derived-state', $arguments, true)) {
            $stamp = dirname($cache_dir) . '/data/cache.json';
            $line .= ' cache_sentinel=' . (file_exists($cache_dir . '/sentinel') ? 'present' : 'absent')
                . ' build_sentinel=' . (file_exists($build_dir . '/sentinel') ? 'present' : 'absent')
                . ' stamp=' . (file_exists($stamp) ? 'present' : 'absent');
        }
        // `setting:read`: the value a run command returns, read *through*
        // the app's own cache — the first run derives and caches it from the
        // database, every later run serves the cached copy without opening
        // the database at all.
        if (in_array('setting:read', $arguments, true)) {
            $cache_file = $cache_dir . '/derived.setting';
            if (file_exists($cache_file)) {
                $value = file_get_contents($cache_file);
            } else {
                $database = getenv('DATABASE_URL');
                $value = file_get_contents(substr($database, strlen('sqlite:///')));
                file_put_contents($cache_file, $value);
            }
            file_put_contents(getenv('APP_LOG_DIR') . '/read.result', $value);
        }
        file_put_contents(
            getenv('APP_LOG_DIR') . '/hooks.log',
            $line . "\n",
            FILE_APPEND
        );
        // A `warmup-boom` marker in the log directory makes the hub's own
        // `cache:warmup` fail — the log directory is the one place an
        // import's cache cleanup cannot reach, so a marker planted before
        // the import survives to the warm-up it is there to break.
        if ($arguments && $arguments[0] === 'cache:warmup'
            && file_exists(getenv('APP_LOG_DIR') . '/warmup-boom')) {
            exit(1);
        }
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
    // what reaches them here. `probe-derived-state` — not a `cache:clear`
    // that would itself hide a missed cleanup — is what makes the first
    // application command report the disposable state it ran against.
    runnable_app_tree(
        source.path(),
        "1.3.0",
        r#"{"pre-update": ["probe-derived-state"], "post-update": ["about"]}"#,
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

    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join("data");
    // The previous state the migration must not inherit: sentinels in both
    // disposable directories, plus the stamp the install's own warm-up wrote.
    fs::write(
        data_dir.join("cache/sentinel"),
        b"derived from the previous database",
    )
    .expect("a cache sentinel");
    fs::write(
        data_dir.join("build/sentinel"),
        b"derived from the previous database",
    )
    .expect("a build sentinel");
    assert!(
        lifecycle::cache_stamp_path(&data_subdir).exists(),
        "the install's warm-up stamped the cache the import must discard"
    );

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
    // The hub's own `cache:warmup` (plan 024) closes out the initial install
    // (no declared hooks there, so it is that event's only line) and then the
    // migrate-forward's own `install::prepare` call — whose first
    // application command reports exactly what the Overview requires: no
    // sentinel, no previous stamp, before any hook could have cleared them.
    assert_eq!(
        fs::read_to_string(log).expect("a hook trace"),
        "cache:warmup --env=prod --no-debug\n\
         probe-derived-state cache_sentinel=absent build_sentinel=absent stamp=absent\n\
         about\n\
         cache:warmup --env=prod --no-debug\n",
        "the installed manifest's pre-update then post-update must run over the imported data, \
         with the previous derived state already gone"
    );

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the imported database"),
        b"older-backup",
        "the imported bytes are what the hooks ran against, not a fixture database"
    );

    // The migrate-forward's own warm-up succeeded, so it wrote a fresh stamp
    // for the installed version — and the import, already past its cleanup,
    // must not delete that fresh one on the way out.
    let stamp: lifecycle::CacheStamp = serde_json::from_str(
        &fs::read_to_string(lifecycle::cache_stamp_path(&data_subdir)).expect("a fresh stamp"),
    )
    .expect("a parseable stamp");
    assert_eq!(stamp.app_version, "1.3.0");
    assert_eq!(
        stamp.snapshot_path,
        paths
            .app_dir("demo")
            .expect("an app dir")
            .display()
            .to_string(),
        "the fresh stamp must vouch for the installed snapshot the warm-up ran from"
    );
}

#[test]
fn a_failed_forward_migration_leaves_the_archive_version_and_names_the_rescue() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();
    runnable_app_tree(source.path(), "1.3.0", r#"{"pre-update": ["boom"]}"#);
    install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the initial install succeeds")
    .expect("the user did not decline");

    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join("data");
    fs::write(data_subdir.join("app.db"), b"before import").expect("an existing database");
    // The previous derived state: a failed migration must not leave any of
    // it behind for a later launch or command to reuse.
    fs::write(
        data_dir.join("cache/sentinel"),
        b"derived from the previous database",
    )
    .expect("a cache sentinel");
    fs::write(
        data_dir.join("build/sentinel"),
        b"derived from the previous database",
    )
    .expect("a build sentinel");
    assert!(
        lifecycle::cache_stamp_path(&data_subdir).exists(),
        "the install's warm-up stamped the cache the import must discard"
    );
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.0")),
            (&format!("{DATA_DIR}/app.db"), b"archive database"),
        ],
    );

    let error = run_import(&paths, "demo", &archive, true, true)
        .expect_err("the fixture's pre-update hook fails");
    let message = error.to_string();
    assert!(
        matches!(error, PortabilityError::ImportIncomplete { .. }),
        "{message}"
    );
    assert!(
        message.contains("extracted but not migrated forward"),
        "{message}"
    );
    assert!(message.contains("app.db.rescue-"), "{message}");
    assert_eq!(
        lifecycle::read_data_version(&data_subdir).expect("the stamped config"),
        Some("1.2.0".to_string()),
        "the next open must see the unfinished forward migration"
    );
    assert!(
        !data_dir.join("cache/sentinel").exists() && !data_dir.join("build/sentinel").exists(),
        "the old cache/build sentinels must not survive a failed migration"
    );
    assert!(
        !lifecycle::cache_stamp_path(&data_subdir).exists(),
        "the old stamp never survives, and a failed migration stamps nothing"
    );
}

#[test]
fn a_failed_warm_up_after_a_forward_migration_warns_without_stamping() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();
    runnable_app_tree(source.path(), "1.3.0", r#"{"pre-update": ["about"]}"#);
    install::install(
        &paths,
        &source.path().display().to_string(),
        None,
        None,
        true,
        true,
        "0.1.0",
    )
    .expect("the initial install succeeds")
    .expect("the user did not decline");

    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join("data");
    // The marker that makes the fixture's own `cache:warmup` exit 1 — planted
    // in the log directory, the one place the import's cache cleanup cannot
    // reach, so it survives to the migrate-forward's closing warm-up.
    fs::write(data_dir.join("log/warmup-boom"), b"").expect("a warm-up failure marker");
    let archive = base.path().join("backup.tar.gz");
    write_test_archive(
        &archive,
        &[
            (MANIFEST_FILE, &manifest_json("dev.local.demo", "1.2.0")),
            (&format!("{DATA_DIR}/app.db"), b"older-backup"),
        ],
    );

    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("the hooks succeed; only the closing warm-up fails");
    assert!(
        proceeded,
        "a failed warm-up is a warning, not an import failure"
    );

    assert_eq!(
        fs::read(data_subdir.join("app.db")).expect("the imported database"),
        b"older-backup"
    );
    assert_eq!(
        lifecycle::read_data_version(&data_subdir).expect("the stamped config"),
        Some("1.3.0".to_string()),
        "the migration itself completed — only its warm-up failed"
    );
    assert!(
        !lifecycle::cache_stamp_path(&data_subdir).exists(),
        "neither the old stamp nor a fresh one may survive a failed warm-up"
    );
}

// --- immediate use: a run command after the import (plan 052) -----------

/// Point `dirs::data_dir()` — and therefore `Paths::resolve()`, the one thing
/// `run::start` resolves on its own rather than taking as a parameter — at
/// `path` for the duration of this guard, restoring the previous value (or
/// its absence) on drop. `make check`'s `--test-threads=1` is what makes
/// borrowing a process-global variable safe here: nothing else in the test
/// binary runs concurrently, and the fixture owns the whole window.
struct RedirectedDataDir {
    previous: Option<std::ffi::OsString>,
}

impl RedirectedDataDir {
    fn point_at(path: &Path) -> Self {
        let previous = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", path);
        Self { previous }
    }
}

impl Drop for RedirectedDataDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var("XDG_DATA_HOME", previous),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
    }
}

/// The end-to-end probe the Overview's bug report describes: export database
/// A, replace it with B, let a declared `run` command derive and cache a
/// value from B, then import A at the same app version and run the command
/// again — before any `open`. The command reads *through* its cache, so it
/// returns A only because the import cleared what B left behind; `Mode::Run`
/// deliberately clears nothing itself.
#[test]
fn a_run_command_after_an_equal_version_import_reads_the_imported_database() {
    if !resources_present() {
        return;
    }
    let source = tempfile::tempdir().expect("a temp source");
    let (base, paths) = temp_paths();
    runnable_app_tree(source.path(), "1.3.0", "{}");
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

    let data_dir = base.path().join("TFSApp/dev.local.demo");
    let data_subdir = data_dir.join(DATA_DIR);
    let read_result = data_dir.join("log/read.result");

    // Database A, exported through the real `run_export`.
    fs::write(data_subdir.join("app.db"), b"database A").expect("database A");
    let archive = base.path().join("backup.tar.gz");
    run_export(&paths, "demo", &archive).expect("export succeeds");

    // Database B takes its place, and the declared `read` alias caches a
    // value derived from it — through the real `run::start`, bundled
    // interpreter and all.
    fs::write(data_subdir.join("app.db"), b"database B").expect("database B");
    {
        let _redirected = RedirectedDataDir::point_at(base.path());
        let code = run::start("demo", "read", &[], false);
        assert_eq!(code, 0, "the seeding run command succeeds");
    }
    assert_eq!(
        fs::read_to_string(&read_result).expect("the seeded result"),
        "database B",
        "the seeding run derived its cache value from B"
    );
    assert!(
        data_dir.join("cache/derived.setting").exists(),
        "the value is cached exactly where the import's cleanup must find it"
    );

    // A returns at the same app version, and the command runs again before
    // any `open` — only the import's own cleanup can keep this reading A.
    let proceeded = run_import(&paths, "demo", &archive, true, true)
        .expect("an equal-version restore over a populated data dir, forced");
    assert!(proceeded);
    {
        let _redirected = RedirectedDataDir::point_at(base.path());
        let code = run::start("demo", "read", &[], false);
        assert_eq!(code, 0, "the post-import run command succeeds");
    }
    assert_eq!(
        fs::read_to_string(&read_result).expect("the post-import result"),
        "database A",
        "the command reads through its cache — it returns A only because the import cleared B's"
    );
    assert!(
        !lifecycle::cache_stamp_path(&data_subdir).exists(),
        "an equal-version import leaves no stamp behind"
    );
}
