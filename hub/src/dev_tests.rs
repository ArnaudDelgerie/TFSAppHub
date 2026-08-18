use std::fs;

use super::{child_args, resolve, DevError};
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
    let canonical = dir.path().canonicalize().expect("a canonical project dir");

    let spec = resolve(dir.path().to_str().expect("a utf8 path")).expect("a resolvable project");

    assert_eq!(spec.source, Source::Live);
    assert_eq!(spec.app_dir, canonical);
    // The prefix is a runtime namespace, never a rename the user should read:
    // the identifier alone carries it, product_name never does.
    assert_eq!(spec.identity.identifier, "dev.dev.local.demo");
    assert_eq!(spec.identity.product_name, "Demo App");
    assert_eq!(spec.state_root, canonical.join("var"));
    assert_eq!(spec.label, canonical.display().to_string());
    // Named, never created: resolving is not launching.
    assert!(!spec.state_root.exists());
}

#[test]
fn an_icon_resolves_inside_the_project_unprefixed() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    project(dir.path(), "dev.local.demo", Some("assets/icon.png"));
    let canonical = dir.path().canonicalize().expect("a canonical project dir");

    let spec = resolve(dir.path().to_str().expect("a utf8 path")).expect("a resolvable project");

    assert_eq!(
        spec.identity.icon_path.as_deref(),
        Some(canonical.join("assets/icon.png").as_path())
    );
}

#[test]
fn the_child_argv_carries_the_project_path_not_an_id() {
    let dir = tempfile::tempdir().expect("a temp project dir");
    project(dir.path(), "dev.local.demo", Some("assets/icon.png"));
    let canonical = dir.path().canonicalize().expect("a canonical project dir");

    let spec = resolve(dir.path().to_str().expect("a utf8 path")).expect("a resolvable project");
    let args = child_args(&spec);

    // `--project`, never `--id`: this is the dev counterpart of
    // `open::child_args`, and the source it carries is a path, not a
    // registered handle.
    assert_eq!(
        args,
        vec![
            "__open".to_string(),
            "--project".to_string(),
            canonical.display().to_string(),
            "--identity".to_string(),
            "dev.dev.local.demo".to_string(),
            "--name".to_string(),
            "Demo App".to_string(),
            "--icon".to_string(),
            canonical.join("assets/icon.png").display().to_string(),
        ]
    );
}

#[test]
fn an_unnormalized_project_path_resolves_to_an_absolute_app_dir() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let project_dir = dir.path().join("project");
    let sibling_dir = dir.path().join("sibling");
    fs::create_dir_all(&project_dir).expect("a project dir");
    fs::create_dir_all(&sibling_dir).expect("a sibling dir");
    project(&project_dir, "dev.local.demo", None);
    let canonical = project_dir.canonicalize().expect("a canonical project dir");

    // Doesn't depend on the process cwd: an absolute path carrying a `..`
    // segment exercises the same un-normalized-path bug a genuinely relative
    // `dev ../Foo/app` argument would, without the test's outcome depending
    // on cwd matching the temp dir's parent.
    let unnormalized = sibling_dir.join("..").join("project");

    let spec = resolve(unnormalized.to_str().expect("a utf8 path")).expect("a resolvable project");

    // Left un-normalized, `sidecar::start` would resolve the Caddyfile path a
    // second time against a different `current_dir` and fail to find it.
    assert!(spec.app_dir.is_absolute());
    assert_eq!(spec.app_dir, canonical);
    assert_eq!(spec.state_root, canonical.join("var"));
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
