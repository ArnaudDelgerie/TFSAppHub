use std::path::PathBuf;

use super::{Paths, PathsError, DATA_DIR_VENDOR};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

#[test]
fn the_hub_root_sits_under_the_shared_vendor_folder() {
    let (base, paths) = temp_paths();

    assert_eq!(paths.vendor_dir(), base.path().join("TFSApp"));
    assert_eq!(paths.hub_root(), base.path().join("TFSApp/hub"));
    assert_eq!(paths.apps_root(), base.path().join("TFSApp/hub/apps"));
    assert_eq!(
        paths.registry_path(),
        base.path().join("TFSApp/hub/registry.json")
    );
    assert_eq!(
        paths.registry_lock_path(),
        base.path().join("TFSApp/hub/registry.lock")
    );
}

#[test]
fn an_installed_snapshot_is_named_by_the_hub_local_id() {
    let (base, paths) = temp_paths();

    assert_eq!(
        paths.app_dir("tfsapp-test").expect("a safe id"),
        base.path().join("TFSApp/hub/apps/tfsapp-test")
    );
}

#[test]
fn the_app_data_dir_is_the_station_own_formula() {
    // The one path both hosts must agree on, byte for byte — reproduced here
    // the way the station's `packaged_data_dir` builds it (`dirs::data_dir()`
    // + DATA_DIR_VENDOR + identifier) rather than by copying this module's
    // own output, so the assertion would survive a refactor of either side.
    let (base, paths) = temp_paths();
    let identifier = "dev.local.tfsapp-test";

    let station_formula = PathBuf::from(base.path())
        .join(DATA_DIR_VENDOR)
        .join(identifier);

    assert_eq!(
        paths.app_data_dir(identifier).expect("a safe identifier"),
        station_formula
    );
}

#[test]
fn the_app_data_dir_is_a_sibling_of_the_hub_root_not_a_child() {
    // If it ever nested under `hub/`, a packaged AppImage of the same app
    // would stop finding its own data — the migration promise, silently
    // broken, with nothing failing loudly.
    let (_base, paths) = temp_paths();

    let data_dir = paths.app_data_dir("dev.local.tfsapp-test").expect("safe");

    assert!(!data_dir.starts_with(paths.hub_root()));
    assert_eq!(data_dir.parent(), Some(paths.vendor_dir().as_path()));
}

#[test]
fn the_hub_executable_path_sits_beside_the_php_shim() {
    let (base, paths) = temp_paths();

    assert_eq!(
        paths.hub_executable_path(),
        base.path().join("TFSApp/hub/bin/tfsapp-hub")
    );
    assert_eq!(
        paths.hub_executable_path().parent(),
        paths.php_shim_path().parent()
    );
}

#[test]
fn the_applications_dir_is_a_sibling_of_the_vendor_dir() {
    let (base, paths) = temp_paths();

    assert_eq!(paths.applications_dir(), base.path().join("applications"));
    assert_eq!(paths.applications_dir().parent(), Some(base.path()));
}

#[test]
fn the_desktop_entry_path_is_named_after_the_identifier() {
    let (base, paths) = temp_paths();

    assert_eq!(
        paths
            .desktop_entry_path("dev.local.tfsapp-test")
            .expect("a safe identifier"),
        base.path().join("applications/dev.local.tfsapp-test.desktop")
    );
}

#[test]
fn an_id_that_is_not_one_path_component_is_refused() {
    let (_base, paths) = temp_paths();

    for id in ["", ".", "..", "a/b", "../escape", "a\\b", "a\0b"] {
        let error = paths
            .app_dir(id)
            .expect_err(&format!("{id:?} must not name a directory"));
        assert!(
            matches!(&error, PathsError::UnsafeSegment { kind, value } if *kind == "app id" && value == id),
            "unexpected error for {id:?}: {error}"
        );
    }
}

#[test]
fn a_desktop_entry_path_for_a_traversing_identifier_is_refused() {
    let (_base, paths) = temp_paths();

    let error = paths
        .desktop_entry_path("../../evil")
        .expect_err("a traversing identifier must not name a desktop entry");

    assert!(matches!(
        error,
        PathsError::UnsafeSegment {
            kind: "app identifier",
            ..
        }
    ));
}

#[test]
fn an_identifier_that_would_escape_the_vendor_dir_is_refused() {
    let (_base, paths) = temp_paths();

    let error = paths
        .app_data_dir("../../evil")
        .expect_err("a traversing identifier must not name a data dir");

    assert!(matches!(
        error,
        PathsError::UnsafeSegment {
            kind: "app identifier",
            ..
        }
    ));
}

#[test]
fn an_ordinary_reverse_dns_identifier_is_accepted_as_it_came() {
    // The counterpart of the test above, and the more important half: the hub
    // must not refuse a value the station accepts. No normalisation, no case
    // folding, no slug — the directory is named by the exact string the
    // manifest declared.
    let (_base, paths) = temp_paths();

    for identifier in ["dev.local.tfsapp-test", "Dev.Local.App", "a b", "app_1"] {
        let data_dir = paths
            .app_data_dir(identifier)
            .unwrap_or_else(|error| panic!("{identifier:?} must be accepted: {error}"));
        assert_eq!(
            data_dir.file_name().and_then(|name| name.to_str()),
            Some(identifier)
        );
    }
}

#[test]
fn creating_the_data_dir_leaves_it_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;

    let (_base, paths) = temp_paths();

    let data_dir = paths
        .create_app_data_dir("dev.local.tfsapp-test")
        .expect("the data dir is created");

    let mode = std::fs::metadata(&data_dir)
        .expect("it exists")
        .permissions();
    assert_eq!(mode.mode() & 0o777, 0o700);
}

#[test]
fn an_already_lax_data_dir_is_tightened_on_the_next_call() {
    // Re-applying `0700` on every call, not only at creation, is what lets an
    // installation written by an older host — or recreated by hand — get
    // fixed instead of staying open forever.
    use std::os::unix::fs::PermissionsExt;

    let (_base, paths) = temp_paths();
    let data_dir = paths
        .create_app_data_dir("dev.local.tfsapp-test")
        .expect("created");
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755)).expect("loosened");

    paths
        .create_app_data_dir("dev.local.tfsapp-test")
        .expect("the second call succeeds on an existing dir");

    let mode = std::fs::metadata(&data_dir)
        .expect("it exists")
        .permissions();
    assert_eq!(mode.mode() & 0o777, 0o700);
}

#[test]
fn the_vendor_dir_above_it_is_left_alone() {
    // Shared with every packaged app on the machine: the hub creates it on the
    // way past and must not tighten or otherwise claim it. Its mode is set
    // here rather than read from whatever the umask produced, so the assertion
    // measures the hub's behaviour and not the developer's environment.
    use std::os::unix::fs::PermissionsExt;

    let (_base, paths) = temp_paths();
    std::fs::create_dir_all(paths.vendor_dir()).expect("the shared vendor dir");
    std::fs::set_permissions(paths.vendor_dir(), std::fs::Permissions::from_mode(0o755))
        .expect("as another app would have left it");

    paths
        .create_app_data_dir("dev.local.tfsapp-test")
        .expect("created");

    let vendor = std::fs::metadata(paths.vendor_dir()).expect("it exists");
    assert_eq!(vendor.permissions().mode() & 0o777, 0o755);
}
