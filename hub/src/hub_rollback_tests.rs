use std::fs;

use super::{rollback, HubRollbackError};
use crate::{hub_bin, hub_update::MissingAnchorHalf, paths::Paths, registry};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A minimal registry snapshot, valid JSON with a `hub_version` a test can
/// assert came back — the exact shape `registry::snapshot_to` would have
/// written in `--update`'s step 7.
fn snapshot_body(hub_version: &str) -> String {
    format!(r#"{{"hub_version": "{hub_version}", "apps": []}}"#)
}

/// Write both anchor halves, and the "current" (newer) hub the rollback is
/// about to replace — the state a successful `--update` leaves behind.
fn seed_anchor(paths: &Paths, previous_bytes: &[u8], snapshot: &str) {
    let stable_path = paths.hub_executable_path();
    fs::create_dir_all(stable_path.parent().unwrap()).expect("bin/ exists");
    fs::write(&stable_path, b"hub v2 bytes (the newer hub)").expect("the running stable copy");
    fs::write(hub_bin::anchor_path(paths), previous_bytes).expect("the anchor binary");
    fs::write(hub_bin::anchor_registry_path(paths), snapshot).expect("the anchor registry");
}

#[test]
fn no_anchor_refuses_and_touches_nothing() {
    let (_base, paths) = temp_paths();

    let error = rollback(&paths, None, true).expect_err("nothing to roll back to");

    match error {
        HubRollbackError::NoAnchor { missing } => assert_eq!(missing, MissingAnchorHalf::Both),
        other => panic!("expected NoAnchor, got {other:?}"),
    }
    assert!(!paths.hub_executable_path().is_file());
}

#[test]
fn a_half_present_anchor_refuses_and_names_the_missing_half() {
    let (_base, paths) = temp_paths();
    // Only the registry half exists.
    let stable_path = paths.hub_executable_path();
    fs::create_dir_all(stable_path.parent().unwrap()).expect("bin/ exists");
    fs::write(hub_bin::anchor_registry_path(&paths), "{}").expect("the anchor registry");

    let error = rollback(&paths, None, true).expect_err("half an anchor is no anchor");

    match error {
        HubRollbackError::NoAnchor { missing } => assert_eq!(missing, MissingAnchorHalf::Binary),
        other => panic!("expected NoAnchor, got {other:?}"),
    }
}

#[test]
fn the_whole_flow_restores_the_binary_and_a_byte_identical_registry_and_consumes_the_anchor() {
    let (_base, paths) = temp_paths();
    let snapshot = snapshot_body("0.1.0");
    seed_anchor(&paths, b"hub v1 bytes (the previous hub)", &snapshot);

    let downloads = tempfile::tempdir().expect("a downloads dir");
    let appimage_path = downloads.path().join("tfsapp-hub-0.2.0.AppImage");
    fs::write(&appimage_path, b"hub v2 bytes (the newer hub)").expect("the $APPIMAGE fixture");

    let outcome = rollback(&paths, Some(appimage_path.to_str().unwrap()), true)
        .expect("the rollback succeeds");
    assert!(outcome);

    // The stable copy and $APPIMAGE both carry the restored bytes.
    assert_eq!(
        fs::read(paths.hub_executable_path()).expect("the stable copy exists"),
        b"hub v1 bytes (the previous hub)"
    );
    assert_eq!(
        fs::read(&appimage_path).expect("$APPIMAGE was restored"),
        b"hub v1 bytes (the previous hub)"
    );

    // registry.json is byte-identical to the snapshot, not merely equivalent
    // under it — restore_from copies bytes, it does not re-serialise.
    assert_eq!(
        fs::read(paths.registry_path()).expect("registry.json exists"),
        snapshot.as_bytes()
    );
    let restored = registry::load(&paths).expect("it reads");
    assert_eq!(restored.hub_version.as_deref(), Some("0.1.0"));

    // The anchor is fully consumed: neither half is left behind.
    assert!(!hub_bin::anchor_path(&paths).is_file());
    assert!(!hub_bin::anchor_registry_path(&paths).is_file());
}

#[test]
fn appimage_already_the_stable_copy_needs_no_second_swap() {
    let (_base, paths) = temp_paths();
    let snapshot = snapshot_body("0.1.0");
    seed_anchor(&paths, b"hub v1 bytes", &snapshot);
    let stable_path = paths.hub_executable_path();

    let outcome =
        rollback(&paths, Some(stable_path.to_str().unwrap()), true).expect("the rollback succeeds");
    assert!(outcome);

    assert_eq!(
        fs::read(&stable_path).expect("the stable copy exists"),
        b"hub v1 bytes"
    );
}

#[test]
fn an_unset_appimage_is_not_a_reason_to_refuse() {
    let (_base, paths) = temp_paths();
    let snapshot = snapshot_body("0.1.0");
    seed_anchor(&paths, b"hub v1 bytes", &snapshot);

    let outcome = rollback(&paths, None, true).expect("a stable-copy-only rollback succeeds");
    assert!(outcome);

    assert_eq!(
        fs::read(paths.hub_executable_path()).expect("the stable copy exists"),
        b"hub v1 bytes"
    );
}

#[test]
fn a_deleted_appimage_download_is_noted_not_a_failure() {
    let (_base, paths) = temp_paths();
    let snapshot = snapshot_body("0.1.0");
    seed_anchor(&paths, b"hub v1 bytes", &snapshot);

    let downloads = tempfile::tempdir().expect("a downloads dir");
    // Never written — the user's own download, gone since the update.
    let appimage_path = downloads.path().join("tfsapp-hub-0.2.0.AppImage");

    let outcome = rollback(&paths, Some(appimage_path.to_str().unwrap()), true)
        .expect("a missing download does not fail the rollback");
    assert!(outcome);

    assert_eq!(
        fs::read(paths.hub_executable_path()).expect("the stable copy exists"),
        b"hub v1 bytes"
    );
    assert!(!appimage_path.exists(), "nothing was written back there");

    // The anchor is still fully consumed — the missing download did not stop
    // the binary/registry half of the rollback from finishing.
    assert!(!hub_bin::anchor_path(&paths).is_file());
    assert!(!hub_bin::anchor_registry_path(&paths).is_file());
}
