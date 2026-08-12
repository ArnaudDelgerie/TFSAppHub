use super::{data_dir_populated, import_decision, ImportRefusal, Manifest};

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
