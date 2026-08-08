use std::fs;

use super::{resolve, DevError};
use crate::launch::Source;

/// A minimal but complete project: a manifest, `bin/console`,
/// `public/index.php` — the three things CONTRACT.md §1 requires.
fn project(dir: &std::path::Path, identifier: &str, icon_path: Option<&str>) {
    fs::create_dir_all(dir.join("bin")).expect("a bin dir");
    fs::create_dir_all(dir.join("public")).expect("a public dir");
    fs::write(dir.join("bin/console"), "#!/usr/bin/env php\n").expect("a console script");
    fs::write(dir.join("public/index.php"), "<?php\n").expect("a front controller");

    let icon_line = match icon_path {
        Some(path) => format!(",\n              \"icon_path\": \"{path}\""),
        None => String::new(),
    };
    fs::write(
        dir.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "{identifier}",
              "project_name": "demo",
              "app_version": "0.6.0"{icon_line}
            }}"#
        ),
    )
    .expect("a manifest");
}

#[test]
fn a_live_project_resolves_with_a_prefixed_identity_and_a_var_state_root() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    project(dir.path(), "dev.local.demo", None);

    let spec = resolve(dir.path().to_str().expect("a utf8 path")).expect("a resolvable project");

    assert_eq!(spec.source, Source::Live);
    assert_eq!(spec.app_dir, dir.path());
    // The prefix is a runtime namespace, never a rename the user should read:
    // the identifier alone carries it, product_name never does.
    assert_eq!(spec.identity.identifier, "dev.dev.local.demo");
    assert_eq!(spec.identity.product_name, "Demo App");
    assert_eq!(spec.state_root, dir.path().join("var"));
    assert_eq!(spec.label, dir.path().display().to_string());
    // Named, never created: resolving is not launching.
    assert!(!spec.state_root.exists());
}

#[test]
fn an_icon_resolves_inside_the_project_unprefixed() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    project(dir.path(), "dev.local.demo", Some("assets/icon.png"));

    let spec = resolve(dir.path().to_str().expect("a utf8 path")).expect("a resolvable project");

    assert_eq!(
        spec.identity.icon_path.as_deref(),
        Some(dir.path().join("assets/icon.png").as_path())
    );
}

#[test]
fn a_missing_path_is_refused_as_not_a_directory() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let missing = dir.path().join("nope");

    let error = resolve(missing.to_str().expect("a utf8 path")).expect_err("no such directory");

    assert!(matches!(error, DevError::NotADirectory { .. }));
    assert!(error.to_string().contains(&missing.display().to_string()));
}

#[test]
fn a_file_instead_of_a_directory_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("not-a-project");
    fs::write(&file, "").expect("a plain file");

    let error = resolve(file.to_str().expect("a utf8 path")).expect_err("not a directory");

    assert!(matches!(error, DevError::NotADirectory { .. }));
}

#[test]
fn a_missing_console_names_the_file_and_the_path() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    project(dir.path(), "dev.local.demo", None);
    fs::remove_file(dir.path().join("bin/console")).expect("removed");

    let error = resolve(dir.path().to_str().expect("a utf8 path")).expect_err("no console");

    assert!(matches!(error, DevError::MissingEntryPoint { .. }));
    let message = error.to_string();
    assert!(message.contains("bin/console"), "names the file: {message}");
    assert!(
        message.contains(&dir.path().display().to_string()),
        "names the path: {message}"
    );
}

#[test]
fn a_missing_front_controller_is_refused() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    project(dir.path(), "dev.local.demo", None);
    fs::remove_file(dir.path().join("public/index.php")).expect("removed");

    let error =
        resolve(dir.path().to_str().expect("a utf8 path")).expect_err("no front controller");

    assert!(matches!(error, DevError::MissingEntryPoint { .. }));
    assert!(error.to_string().contains("public/index.php"));
}

#[test]
fn a_missing_manifest_is_refused_by_the_shared_manifest_error() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    fs::create_dir_all(dir.path().join("bin")).expect("a bin dir");
    fs::create_dir_all(dir.path().join("public")).expect("a public dir");
    fs::write(dir.path().join("bin/console"), "").expect("a console script");
    fs::write(dir.path().join("public/index.php"), "").expect("a front controller");

    let error = resolve(dir.path().to_str().expect("a utf8 path")).expect_err("no manifest");

    assert!(matches!(error, DevError::Manifest(_)));
}
