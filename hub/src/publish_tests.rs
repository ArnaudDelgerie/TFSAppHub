use std::{fs, os::unix::fs::symlink, path::Path};

use super::{build_archive, changelog_section, is_owner_repo_shape, run_local_gates, PublishError};
use crate::{archive, release, source};

fn write_manifest(root: &Path, app_version: &str, extra: &str) {
    fs::create_dir_all(root).expect("a project root");
    fs::write(
        root.join("tfsapp.config.json"),
        format!(
            r#"{{
              "product_name": "Demo App",
              "identifier": "dev.local.demo",
              "project_name": "demo",
              "app_version": "{app_version}"{extra}
            }}"#
        ),
    )
    .expect("a manifest");
}

fn write_changelog(root: &Path, contents: &str) {
    fs::write(root.join("CHANGELOG.md"), contents).expect("a changelog");
}

const CHANGELOG: &str = "\
# Changelog\n\
\n\
## 1.2.0\n\
\n\
Added the frobnicator.\n\
Fixed the widget.\n\
\n\
## 1.1.0\n\
\n\
Initial release.\n";

#[test]
fn every_gate_passes_and_produces_the_repo_and_the_notes() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "releases_repo": "owner/repo""#,
    );
    write_changelog(project.path(), CHANGELOG);

    let gates = run_local_gates(project.path(), None).expect("every gate to pass");
    assert_eq!(gates.repo, "owner/repo");
    assert_eq!(gates.notes, "Added the frobnicator.\nFixed the widget.");
}

#[test]
fn a_repo_flag_overrides_the_manifests_releases_repo() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "releases_repo": "owner/repo""#,
    );
    write_changelog(project.path(), CHANGELOG);

    let gates = run_local_gates(project.path(), Some("other-owner/other-repo"))
        .expect("every gate to pass");
    assert_eq!(gates.repo, "other-owner/other-repo");
}

#[test]
fn a_missing_manifest_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    fs::create_dir_all(project.path()).expect("a project root");

    let error = run_local_gates(project.path(), Some("owner/repo")).unwrap_err();
    assert!(matches!(error, PublishError::Manifest(_)), "{error}");
}

#[test]
fn a_non_semver_app_version_is_refused_naming_the_value_and_the_field() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "v1.2", "");
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(project.path(), Some("owner/repo")).unwrap_err();
    match &error {
        PublishError::UnusableVersion { version, .. } => assert_eq!(version, "v1.2"),
        other => panic!("expected UnusableVersion, got {other}"),
    }
    assert!(error.to_string().contains("app_version"), "{error}");
    assert!(error.to_string().contains("CONTRACT.md §2"), "{error}");
}

#[test]
fn no_repo_from_either_source_is_refused_naming_both_ways() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(project.path(), None).unwrap_err();
    assert!(
        matches!(error, PublishError::NoRepository { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("--repo"), "{error}");
    assert!(error.to_string().contains("releases_repo"), "{error}");
}

#[test]
fn an_empty_releases_repo_is_the_same_as_no_repo() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", r#", "releases_repo": "   ""#);
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(project.path(), None).unwrap_err();
    assert!(
        matches!(error, PublishError::NoRepository { .. }),
        "{error}"
    );
}

#[test]
fn a_malformed_repo_shape_is_refused() {
    for bad in ["not-a-repo", "owner/", "/repo", "owner/repo/extra", ""] {
        assert!(!is_owner_repo_shape(bad), "{bad:?} should be rejected");
    }
    assert!(is_owner_repo_shape("owner/repo"));

    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(project.path(), Some("not-a-repo")).unwrap_err();
    match &error {
        PublishError::InvalidRepoShape { repo, source, .. } => {
            assert_eq!(repo, "not-a-repo");
            assert_eq!(*source, "--repo");
        }
        other => panic!("expected InvalidRepoShape, got {other}"),
    }
}

#[test]
fn a_missing_changelog_file_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");

    let error = run_local_gates(project.path(), Some("owner/repo")).unwrap_err();
    assert!(
        matches!(error, PublishError::MissingChangelog { .. }),
        "{error}"
    );
}

#[test]
fn a_changelog_with_no_matching_heading_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.3.0", "");
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(project.path(), Some("owner/repo")).unwrap_err();
    match &error {
        PublishError::MissingChangelogEntry { version, .. } => assert_eq!(version, "1.3.0"),
        other => panic!("expected MissingChangelogEntry, got {other}"),
    }
}

#[test]
fn a_heading_that_merely_contains_the_version_does_not_answer_for_it() {
    // "## 1.2.0.1" must not satisfy a gate for "1.2.0" — CONTRACT.md §7's own
    // example of the trap a looser match would fall into.
    let changelog = "## 1.2.0.1\n\nUnrelated notes.\n";
    assert_eq!(changelog_section(changelog, "1.2.0"), None);
}

#[test]
fn all_three_accepted_heading_spellings_match() {
    for (heading, expected_body) in [
        ("## 1.2.0", "plain"),
        ("## v1.2.0", "v-prefixed"),
        ("## [1.2.0]", "bracketed"),
        ("## 1.2.0 (2026-08-10)", "with a date"),
        (
            "## [1.2.0](https://example.com/releases/1.2.0)",
            "with a link",
        ),
    ] {
        let changelog = format!("{heading}\n\n{expected_body}\n");
        assert_eq!(
            changelog_section(&changelog, "1.2.0").as_deref(),
            Some(expected_body),
            "heading {heading:?} should have matched"
        );
    }
}

#[test]
fn the_extracted_section_stops_at_the_next_heading() {
    let notes = changelog_section(CHANGELOG, "1.2.0").expect("a matching section");
    assert_eq!(notes, "Added the frobnicator.\nFixed the widget.");
    assert!(!notes.contains("Initial release"));
}

#[test]
fn ipc_on_and_undeclinable_in_a_test_refuses_the_gate() {
    // Tests never run with a terminal on stdin, so `prompt::confirmed(false)`
    // always refuses here — which is exactly gate 5's point: there is no
    // `--yes` to make this pass non-interactively.
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "releases_repo": "owner/repo", "actions": {"secrets": {"ipc": true, "bridge": false, "keys": ["k"]}}"#,
    );
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(project.path(), None).unwrap_err();
    assert!(matches!(error, PublishError::IpcNotConfirmed), "{error}");
}

fn write_source_files(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("a src dir");
    fs::write(root.join("src/main.php"), "<?php\n").expect("a source file");
    symlink("src/main.php", root.join("link-to-main")).expect("a safe symlink");

    for (dir, file, content) in [
        ("vendor/pkg", "lib.php", "vendor"),
        ("var/cache", "entry", "cache"),
        ("node_modules/pkg", "index.js", "js"),
        (".git", "HEAD", "ref: refs/heads/main"),
        ("tfsapp_build", "app.AppImage", "binary"),
    ] {
        let dir = root.join(dir);
        fs::create_dir_all(&dir).expect("an excluded dir");
        fs::write(dir.join(file), content).expect("a file under an excluded dir");
    }
}

#[test]
fn the_archive_extracts_to_a_tree_hashing_the_same_as_the_source() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    let scratch = tempfile::tempdir().expect("a temp scratch dir");

    let assets = build_archive(project.path(), "demo", "1.2.0", scratch.path())
        .expect("the archive to build");
    assert_eq!(assets.archive_name, "demo-1.2.0.tar.gz");
    assert!(assets.archive_path.is_file());
    assert!(assets.sums_path.is_file());

    let extracted_into = scratch.path().join("extracted");
    let root = archive::extract(&assets.archive_path, &extracted_into)
        .expect("our own installer to accept what we just built");

    assert_eq!(
        source::tree_hash(&root).expect("a hash of the extracted tree"),
        source::tree_hash(project.path()).expect("a hash of the source tree"),
        "the archive must round-trip to the exact tree tree_hash covers"
    );

    for excluded in ["vendor", "var", "node_modules", ".git", "tfsapp_build"] {
        assert!(
            !root.join(excluded).exists(),
            "{excluded} must not be in the archive"
        );
    }
    assert!(root.join("src/main.php").is_file());
    assert!(root.join("link-to-main").is_symlink());
}

#[test]
fn the_sums_file_verifies_against_the_archive_it_names() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    let scratch = tempfile::tempdir().expect("a temp scratch dir");

    let assets = build_archive(project.path(), "demo", "1.2.0", scratch.path())
        .expect("the archive to build");

    let sums_body = fs::read_to_string(&assets.sums_path).expect("a readable sums file");
    assert_eq!(
        sums_body,
        format!("{}  {}\n", checksum(&assets), assets.archive_name)
    );

    let checksums = release::parse_sha256sums(&sums_body);
    let actual = release::sha256_file(&assets.archive_path).expect("a hash of the built archive");
    assert_eq!(
        release::verify(&checksums, &assets.archive_name, &actual),
        release::VerifyOutcome::Match
    );
}

fn checksum(assets: &super::Assets) -> String {
    release::sha256_file(&assets.archive_path).expect("a hash of the built archive")
}

#[test]
fn an_escaping_symlink_is_refused_before_anything_is_uploaded() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    symlink("../../outside", project.path().join("evil")).expect("an escaping symlink");
    let scratch = tempfile::tempdir().expect("a temp scratch dir");

    let error = build_archive(project.path(), "demo", "1.2.0", scratch.path()).unwrap_err();
    match &error {
        PublishError::EscapingSymlink { path } => {
            assert_eq!(path, &project.path().join("evil"));
        }
        other => panic!("expected EscapingSymlink, got {other}"),
    }
}

#[test]
fn ipc_off_or_bridge_only_never_asks() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "releases_repo": "owner/repo", "actions": {"secrets": {"ipc": false, "bridge": true, "keys": ["k"]}}"#,
    );
    write_changelog(project.path(), CHANGELOG);

    run_local_gates(project.path(), None).expect("bridge-only must not trip the ipc gate");
}
