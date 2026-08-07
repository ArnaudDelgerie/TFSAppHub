use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use super::{check_id_free, check_port_free, resolve_id, snapshot, validate, InstallError};
use crate::{
    paths::Paths,
    registry::{now_timestamp, Platform, Registry, RegistryEntry, Source, SourceKind, State},
};

/// The smallest tree the installer accepts: a manifest with the four required
/// fields, plus the three files CONTRACT.md §1 and Composer need.
fn app_tree(root: &Path) {
    manifest_at(root, "0.6.0");
    fs::write(root.join("composer.json"), "{\"require\": {}}").expect("a composer.json");

    fs::create_dir_all(root.join("bin")).expect("a bin dir");
    fs::write(root.join("bin/console"), "#!/usr/bin/env php\n").expect("a console");
    fs::set_permissions(root.join("bin/console"), fs::Permissions::from_mode(0o755))
        .expect("an executable console");

    fs::create_dir_all(root.join("public")).expect("a public dir");
    fs::write(root.join("public/index.php"), "<?php").expect("a front controller");
}

fn manifest_at(root: &Path, app_version: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{app_version}"
            }}"#
        ),
    )
    .expect("a manifest");
}

fn manifest(root: &Path) -> crate::manifest::Manifest {
    crate::manifest::load(root)
        .expect("a readable manifest")
        .manifest
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

fn registered(id: &str, location: &str) -> RegistryEntry {
    RegistryEntry {
        id: id.to_string(),
        identifier: format!("dev.local.{id}"),
        source: Source {
            kind: SourceKind::LocalPath,
            location: location.to_string(),
            reference: None,
            reference_kind: None,
        },
        app_version: "0.6.0".to_string(),
        source_revision: "sha256:deadbeef".to_string(),
        app_port: None,
        platform: Platform {
            php_version: "8.5".to_string(),
            extensions_hash: "a1b2c3d4".repeat(8),
        },
        state: State::Ready,
        installed_at: now_timestamp(),
        updated_at: now_timestamp(),
        unknown: serde_json::Map::new(),
    }
}

#[test]
fn the_id_is_the_project_name_unless_told_otherwise() {
    let root = tempfile::tempdir().expect("a temp dir");
    app_tree(root.path());
    let manifest = manifest(root.path());

    assert_eq!(resolve_id(None, &manifest).expect("a derived id"), "demo");
    assert_eq!(
        resolve_id(Some("second-demo"), &manifest).expect("an explicit id"),
        "second-demo"
    );
}

#[test]
fn an_id_that_cannot_name_a_directory_is_refused_rather_than_sanitised() {
    // A silent transformation would leave the user with an app installed under
    // a name they never chose and cannot guess.
    let root = tempfile::tempdir().expect("a temp dir");
    app_tree(root.path());
    let manifest = manifest(root.path());

    for id in ["", "..", "a/b", "-leading", ".hidden", "with space"] {
        let error =
            resolve_id(Some(id), &manifest).expect_err(&format!("{id:?} cannot be an app id"));
        assert!(matches!(error, InstallError::UnusableId { .. }), "{error}");
        // The way out is named, since the manifest is not always the user's to
        // edit.
        assert!(error.to_string().contains("--as"), "{error}");
    }
}

#[test]
fn an_unusable_project_name_says_where_it_came_from() {
    // The two sources of an id fail the same check and need opposite answers:
    // one is the user's own argument, the other is a field in someone else's
    // manifest.
    let root = tempfile::tempdir().expect("a temp dir");
    app_tree(root.path());
    fs::write(
        root.path().join("tfsapp.config.json"),
        r#"{
          "product_name": "Demo App",
          "identifier": "dev.local.demo",
          "project_name": "../demo",
          "app_version": "0.6.0"
        }"#,
    )
    .expect("a manifest with an unusable slug");

    let error = resolve_id(None, &manifest(root.path())).expect_err("../demo names a directory");

    assert!(error.to_string().contains("project_name"), "{error}");
}

#[test]
fn an_id_already_installed_is_refused_naming_what_holds_it() {
    let (_base, paths) = temp_paths();
    let mut registry = Registry::default();
    registry.upsert(registered("demo", "/home/arnaud/Dev/Demo"));

    let error = check_id_free(&registry, &paths, "demo").expect_err("demo is taken");

    assert!(matches!(error, InstallError::IdTaken { .. }), "{error}");
    assert!(
        error.to_string().contains("/home/arnaud/Dev/Demo"),
        "{error}"
    );
    check_id_free(&registry, &paths, "other").expect("nothing holds `other`");
}

#[test]
fn a_directory_with_no_entry_behind_it_is_refused_rather_than_reused() {
    // The leftover of an install interrupted before it could register — the one
    // case where copying over would destroy something nobody recorded.
    let (_base, paths) = temp_paths();
    let app_dir = paths.app_dir("demo").expect("an app dir");
    fs::create_dir_all(&app_dir).expect("a leftover directory");

    let error = check_id_free(&Registry::default(), &paths, "demo")
        .expect_err("the directory is in the way");

    assert!(
        matches!(error, InstallError::DirectoryInTheWay { .. }),
        "{error}"
    );
}

#[test]
fn a_tree_missing_what_an_app_needs_is_refused_before_anything_is_copied() {
    for missing in ["composer.json", "bin/console", "public/index.php"] {
        let root = tempfile::tempdir().expect("a temp dir");
        app_tree(root.path());
        fs::remove_file(root.path().join(missing)).expect("a removed requirement");

        let error = validate(root.path()).expect_err(&format!("{missing} is required"));

        assert!(matches!(error, InstallError::MissingFile { .. }), "{error}");
        assert!(error.to_string().contains(missing), "{error}");
    }
}

#[test]
fn a_missing_manifest_is_reported_as_the_contract_states_it() {
    let root = tempfile::tempdir().expect("a temp dir");
    app_tree(root.path());
    fs::remove_file(root.path().join("tfsapp.config.json")).expect("a removed manifest");

    let error = validate(root.path()).expect_err("an app carries a manifest");

    assert!(matches!(error, InstallError::Manifest(_)), "{error}");
}

#[test]
fn an_app_version_nothing_can_compare_is_refused_at_install() {
    // The hub's equivalent of the station's build-time semver check
    // (CONTRACT.md §2 / .project/contract-amendments.md #5): install is the
    // moment the hub can still say no, and `update` is what would otherwise
    // discover it months later.
    let root = tempfile::tempdir().expect("a temp dir");
    app_tree(root.path());
    manifest_at(root.path(), "01.2.3");

    let error = validate(root.path()).expect_err("01.2.3 is not canonical semver");

    assert!(
        matches!(error, InstallError::UnusableVersion { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("app_version"), "{error}");

    manifest_at(root.path(), "1.2.3");
    validate(root.path()).expect("1.2.3 is canonical semver");
}

#[test]
fn the_snapshot_leaves_out_what_must_never_be_installed() {
    let source = tempfile::tempdir().expect("a temp source");
    let destination = tempfile::tempdir().expect("a temp destination");
    let root = source.path();
    app_tree(root);

    let noise: &[&str] = &[
        ".git/HEAD",
        "var/cache/prod/container.php",
        "var/log/app.log",
        "node_modules/left-pad/index.js",
        "assets/node_modules/vue/index.js",
        "tfsapp_build/demo_0.6.0_amd64.AppImage",
        "vendor/autoload.php",
        "var/data/app.db",
        "src/var/log/kept.txt",
    ];
    for relative in noise {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("a noise dir");
        fs::write(&path, "x").expect("a noise file");
    }

    let installed = destination.path().join("demo");
    snapshot(root, &installed).expect("it copies");

    let present = |relative: &str| installed.join(relative).exists();
    assert!(present("tfsapp.config.json") && present("bin/console"));
    // Vendored dependencies and the app's own data come along: `composer
    // install` then has something to work from offline, and `var/` is the
    // app's, minus the two directories it regenerates.
    assert!(present("vendor/autoload.php"), "vendor is source enough");
    assert!(present("var/data/app.db"), "var/data is the app's own");
    // A path that merely looks like an excluded one is kept — the exclusion is
    // on the project root's `var/log`, not on the word.
    assert!(
        present("src/var/log/kept.txt"),
        "src/var/log is not var/log"
    );

    for excluded in [
        ".git",
        "var/cache",
        "var/log",
        "node_modules",
        "assets/node_modules",
        "tfsapp_build",
    ] {
        assert!(
            !present(excluded),
            "{excluded} has no business in an installed snapshot"
        );
    }
}

#[test]
fn the_snapshot_keeps_symlinks_and_the_executable_bit() {
    let source = tempfile::tempdir().expect("a temp source");
    let destination = tempfile::tempdir().expect("a temp destination");
    let root = source.path();
    app_tree(root);
    std::os::unix::fs::symlink("console", root.join("bin/link")).expect("a symlink");

    let installed = destination.path().join("demo");
    snapshot(root, &installed).expect("it copies");

    // Without the bit, the very first lifecycle command fails on a file that
    // looks perfectly fine.
    let mode = fs::metadata(installed.join("bin/console"))
        .expect("a copied console")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "bin/console must stay executable: {mode:o}"
    );

    let link = fs::symlink_metadata(installed.join("bin/link")).expect("a copied link");
    assert!(
        link.is_symlink(),
        "a link is copied as a link, never followed"
    );
    assert_eq!(
        fs::read_link(installed.join("bin/link")).expect("a target"),
        PathBuf::from("console")
    );
}

#[test]
fn the_snapshot_refuses_to_write_over_an_existing_tree() {
    // Merging into a tree the installer did not write would leave files from a
    // previous version indistinguishable from this one's.
    let source = tempfile::tempdir().expect("a temp source");
    let destination = tempfile::tempdir().expect("a temp destination");
    app_tree(source.path());
    let installed = destination.path().join("demo");
    fs::create_dir_all(&installed).expect("an occupied destination");

    let error = snapshot(source.path(), &installed).expect_err("it must refuse");

    assert!(
        matches!(error, InstallError::DirectoryInTheWay { .. }),
        "{error}"
    );
}

/// A registry holding one app, pinning `app_port` or not.
fn registry_with(id: &str, app_port: Option<u16>) -> Registry {
    let mut registry = Registry::default();
    registry.upsert(RegistryEntry {
        app_port,
        ..registered(id, "/home/arnaud/Dev/Demo")
    });
    registry
}

#[test]
fn two_apps_cannot_pin_one_port() {
    // The generalisation of `build-app.sh`'s build-time port check: over there
    // one build made one app, here N apps share one machine, so the question
    // stops being "is this a plausible port" and becomes "whose port is it".
    let registry = registry_with("first", Some(8123));

    let error = check_port_free(&registry, Some(8123)).expect_err("8123 is claimed");

    assert!(matches!(error, InstallError::PortTaken { .. }), "{error}");
    // Naming the holder is the point: the user cannot resolve a collision with
    // an app they cannot identify.
    assert!(error.to_string().contains("first"), "{error}");

    check_port_free(&registry, Some(8124)).expect("another number is free");
}

#[test]
fn a_dynamic_port_neither_claims_nor_loses_a_number() {
    // The default, and what makes the gate narrow: a port picked free at launch
    // cannot collide at install with anything, in either direction.
    check_port_free(&registry_with("first", Some(8123)), None).expect("dynamic against static");
    check_port_free(&registry_with("first", None), Some(8123)).expect("static against dynamic");
    check_port_free(&registry_with("first", None), None).expect("dynamic against dynamic");
    check_port_free(&Registry::default(), Some(8123)).expect("nothing is installed");
}

#[test]
fn a_pinned_port_nothing_can_bind_is_refused() {
    // The one value that survives the manifest's `u16` parse and still cannot
    // be bound: zero means "any free port" to the OS, the opposite of pinning.
    let error = check_port_free(&Registry::default(), Some(0)).expect_err("0 is not a port");

    assert!(
        matches!(error, InstallError::UnusablePort { .. }),
        "{error}"
    );
}
