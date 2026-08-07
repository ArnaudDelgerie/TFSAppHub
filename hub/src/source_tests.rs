use std::{fs, path::Path};

use super::{current_revision, tree_hash, Revision};
use crate::registry::{Source, SourceKind};

fn project(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("a source dir");
    fs::write(root.join("tfsapp.config.json"), "{}").expect("a manifest");
    fs::write(root.join("src/Kernel.php"), "<?php class Kernel {}").expect("a class");
}

fn local(location: &Path) -> Source {
    Source {
        kind: SourceKind::LocalPath,
        location: location.display().to_string(),
        reference: None,
        reference_kind: None,
    }
}

#[test]
fn the_same_tree_hashes_the_same_wherever_it_sits() {
    // What the value is for: a tree copied to another path is the same source,
    // and a hash that disagreed would report every install as changed.
    let one = tempfile::tempdir().expect("a temp dir");
    let other = tempfile::tempdir().expect("a second temp dir");
    project(one.path());
    project(other.path());

    assert_eq!(
        tree_hash(one.path()).expect("it hashes"),
        tree_hash(other.path()).expect("it hashes"),
    );
}

#[test]
fn an_edit_changes_the_hash() {
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let before = tree_hash(root.path()).expect("it hashes");

    fs::write(root.path().join("src/Kernel.php"), "<?php class Kernel { }")
        .expect("an edited class");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn moving_content_to_another_name_changes_the_hash() {
    // The bytes are identical and the tree is not: without the path in the
    // hash, a rename would be invisible.
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let before = tree_hash(root.path()).expect("it hashes");

    fs::rename(
        root.path().join("src/Kernel.php"),
        root.path().join("src/AppKernel.php"),
    )
    .expect("a rename");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn losing_the_executable_bit_changes_the_hash() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let script = root.path().join("bin/console");
    fs::create_dir_all(root.path().join("bin")).expect("a bin dir");
    fs::write(&script, "#!/usr/bin/env php").expect("a script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("+x");
    let before = tree_hash(root.path()).expect("it hashes");

    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).expect("-x");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn the_directories_that_churn_are_left_out() {
    // `var/cache` changes on every request the app serves. Hashing it would
    // report "changed since install" forever, which is the same as reporting
    // nothing at all.
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let before = tree_hash(root.path()).expect("it hashes");

    for churn in ["var/cache/dev", "vendor/symfony/console", "node_modules/x"] {
        fs::create_dir_all(root.path().join(churn)).expect("a churning dir");
        fs::write(root.path().join(churn).join("file"), "noise").expect("noise");
    }

    assert_eq!(before, tree_hash(root.path()).expect("it hashes"));

    // Only at the top level, though: a source file that happens to live under
    // a directory of that name still counts.
    fs::create_dir_all(root.path().join("src/var")).expect("a source subdir");
    fs::write(root.path().join("src/var/Holder.php"), "<?php").expect("a class");

    assert_ne!(before, tree_hash(root.path()).expect("it hashes"));
}

#[test]
fn a_local_source_that_is_gone_is_unreachable_not_an_error() {
    let root = tempfile::tempdir().expect("a temp dir");
    project(root.path());
    let source = local(root.path());

    assert!(matches!(current_revision(&source), Revision::At(_)));

    drop(root);

    assert_eq!(current_revision(&source), Revision::Unreachable);
}

#[test]
fn a_git_source_is_unreachable_until_something_resolves_one() {
    // `list` must not put a network call behind a listing, and no installer
    // writes a git source yet. Silence is the honest answer.
    let source = Source {
        kind: SourceKind::Git,
        location: "https://example.test/demo.git".to_string(),
        reference: Some("v1.4.0".to_string()),
        reference_kind: None,
    };

    assert_eq!(current_revision(&source), Revision::Unreachable);
}
