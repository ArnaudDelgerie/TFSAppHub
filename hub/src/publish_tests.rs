use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

use super::{
    build_archive, changelog_section, excluded_from_archive, is_owner_repo_shape, publish,
    publish_local, run_local_gates, BlobSource, PublishError, PublishTarget,
};
use crate::{
    archive,
    gh::{Gh, GhError},
    git::{Git, GitError, TreeEntry},
    paths::Paths,
    release, source,
};

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

/// A fake `git` (or `gh`) at `dir/<name>`, logging its own argv (space-joined)
/// to `dir/argv.log` before dispatching — reused across both seams' tests
/// here, unlike `git_tests.rs`/`gh_tests.rs`'s own copies, since a
/// full-pipeline test in this module needs both at once and each fixture
/// gets its own directory (and so its own log) anyway.
fn write_fake(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/argv.log\"\n{body}\n"),
    )
    .expect("a fake script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("+x");
    path
}

fn argv_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
}

/// A `git` that reports a work tree on `main`, clean, tracking
/// `origin/main` with no divergence, `HEAD` at a fixed sha, and `origin`
/// resolving to `https://github.com/owner/repo` — every gate 3–6 needs, all
/// at once, so most tests below that are not themselves about the git gate
/// can just pass this and move on.
const GIT_CLEAN_AND_PUSHED: &str = r#"
case "$3" in
  rev-parse)
    case "$4" in
      --show-toplevel) echo "/repo"; exit 0 ;;
      --show-prefix) printf ''; exit 0 ;;
      HEAD) echo "deadbeefcafe1234"; exit 0 ;;
    esac
    ;;
  status)
    printf '## main...origin/main\n'
    exit 0
    ;;
  ls-files)
    [ "$4" = "-z" ] && {
      printf 'CHANGELOG.md\0link-to-main\0src/main.php\0tfsapp.config.json\0'
      exit 0
    }
    ;;
  remote)
    [ "$4" = "get-url" ] && { echo "https://github.com/owner/repo"; exit 0; }
    ;;
  merge-base) exit 0 ;;
  ls-tree)
    printf '100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tCHANGELOG.md\0'
    printf '120000 blob bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\tlink-to-main\0'
    printf '100644 blob cccccccccccccccccccccccccccccccccccccccc\tsrc/main.php\0'
    printf '100644 blob dddddddddddddddddddddddddddddddddddddddd\ttfsapp.config.json\0'
    exit 0
    ;;
  cat-file)
    while IFS= read -r object; do
      case "$object" in
        a*) value='# Changelog

## 1.2.0

Added the frobnicator.
Fixed the widget.
'; printf '%s blob %s\n%s\n' "$object" "${#value}" "$value" ;;
        b*) printf '%s blob 12\nsrc/main.php\n' "$object" ;;
        c*) printf '%s blob 6\n<?php\n\n' "$object" ;;
        d*) value='{"product_name":"Demo App","identifier":"dev.local.demo","project_name":"demo","app_version":"1.2.0"}'; printf '%s blob %s\n%s\n' "$object" "${#value}" "$value" ;;
      esac
    done
    exit 0
    ;;
esac
exit 1
"#;

/// A `git` that refuses every call — proves, when a test expects a failure
/// from an *earlier* gate, that the git gate was never reached at all: a
/// nonexistent program would surface as `GitError::NotInstalled`, which is
/// never what these tests expect, so reaching it would fail loudly rather
/// than by coincidence.
fn git_never_called() -> Git {
    Git::at(PathBuf::from("/nonexistent/git"))
}

/// `gh` that refuses every call — the same proof as [`git_never_called`], for
/// the `gh` seam.
fn gh_never_called() -> Gh {
    Gh::at(PathBuf::from("/nonexistent/gh"))
}

#[test]
fn every_gate_passes_and_resolves_the_repo_from_the_upstream_remote() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));

    let gates = run_local_gates(project.path(), PublishTarget::Forge(None), &git)
        .expect("every gate to pass");
    assert_eq!(gates.commit.repo.as_deref(), Some("owner/repo"));
    assert_eq!(gates.commit.branch, "main");
    assert_eq!(gates.commit.sha, "deadbeefcafe1234");
    assert_eq!(gates.notes, "Added the frobnicator.\nFixed the widget.");
}

#[test]
fn a_repo_flag_overrides_the_resolved_remote() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));

    let gates = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("other-owner/other-repo")),
        &git,
    )
    .expect("every gate to pass");
    assert_eq!(gates.commit.repo.as_deref(), Some("other-owner/other-repo"));
}

#[test]
fn a_stale_releases_repo_in_the_manifest_is_ignored() {
    // The station-era key: still accepted by the manifest parser, read by
    // nothing. `owner/repo` must come from `origin`, never from this.
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "releases_repo": "stale-owner/stale-repo""#,
    );
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));

    let gates = run_local_gates(project.path(), PublishTarget::Forge(None), &git)
        .expect("every gate to pass");
    assert_eq!(gates.commit.repo.as_deref(), Some("owner/repo"));
}

#[test]
fn a_missing_pinned_manifest_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    fs::create_dir_all(project.path()).expect("a project root");

    let body = GIT_CLEAN_AND_PUSHED.replace(
        "    printf '100644 blob dddddddddddddddddddddddddddddddddddddddd\\ttfsapp.config.json\\0'\n",
        "",
    );
    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", &body));
    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
    assert!(matches!(error, PublishError::Manifest(_)), "{error}");
}

#[test]
fn a_non_semver_app_version_is_refused_naming_the_value_and_the_field() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "v1.2", "");
    write_changelog(project.path(), CHANGELOG);

    let body =
        GIT_CLEAN_AND_PUSHED.replace("\"app_version\":\"1.2.0\"", "\"app_version\":\"v1.2\"");
    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", &body));
    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
    match &error {
        PublishError::UnusableVersion { version, .. } => assert_eq!(version, "v1.2"),
        other => panic!("expected UnusableVersion, got {other}"),
    }
    assert!(error.to_string().contains("app_version"), "{error}");
    assert!(error.to_string().contains("CONTRACT.md §2"), "{error}");
}

#[test]
fn suffixed_app_versions_are_refused_before_git_is_called() {
    for version in ["1.2.3-rc.1", "1.2.3+build.7"] {
        let project = tempfile::tempdir().expect("a temp project dir");
        write_manifest(project.path(), version, "");

        let error = run_local_gates(
            project.path(),
            PublishTarget::Forge(Some("owner/repo")),
            &git_never_called(),
        )
        .expect_err("a suffixed app version is refused before Git");
        assert!(
            matches!(error, PublishError::UnusableVersion { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("MAJOR.MINOR.PATCH"), "{error}");
    }
}

#[test]
fn a_malformed_repo_flag_is_refused_before_git_is_ever_called() {
    for bad in ["not-a-repo", "owner/", "/repo", "owner/repo/extra", ""] {
        assert!(!is_owner_repo_shape(bad), "{bad:?} should be rejected");
    }
    assert!(is_owner_repo_shape("owner/repo"));

    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("not-a-repo")),
        &git_never_called(),
    )
    .unwrap_err();
    match &error {
        PublishError::InvalidRepoShape { repo } => assert_eq!(repo, "not-a-repo"),
        other => panic!("expected InvalidRepoShape, got {other}"),
    }
}

#[test]
fn a_dirty_project_directory_refuses_naming_the_paths() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(
        scripts.path(),
        "git",
        r#"
case "$3" in
  rev-parse)
    [ "$4" = "--show-toplevel" ] && { echo "/repo"; exit 0; }
    ;;
  status)
    printf '## main...origin/main\n M src/App.php\n'
    exit 0
    ;;
esac
exit 1
"#,
    ));

    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
    match &error {
        PublishError::Git(GitError::Dirty { paths }) => {
            assert_eq!(paths, &["src/App.php"]);
        }
        other => panic!("expected Git(Dirty), got {other}"),
    }
}

#[test]
fn no_upstream_refuses_naming_git_push() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(
        scripts.path(),
        "git",
        r#"
case "$3" in
  rev-parse)
    [ "$4" = "--show-toplevel" ] && { echo "/repo"; exit 0; }
    ;;
  status)
    printf '## main\n'
    exit 0
    ;;
esac
exit 1
"#,
    ));

    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
    assert!(
        matches!(error, PublishError::Git(GitError::NoUpstream { .. })),
        "{error}"
    );
    assert!(error.to_string().contains("git push -u origin main"));
}

#[test]
fn a_missing_changelog_file_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let body = GIT_CLEAN_AND_PUSHED.replace(
        "    printf '100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\tCHANGELOG.md\\0'\n",
        "",
    );
    let git = Git::at(write_fake(scripts.path(), "git", &body));

    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
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

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let body =
        GIT_CLEAN_AND_PUSHED.replace("\"app_version\":\"1.2.0\"", "\"app_version\":\"1.3.0\"");
    let git = Git::at(write_fake(scripts.path(), "git", &body));

    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
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
    // always refuses here — which is exactly gate 8's point: there is no
    // `--yes` to make this pass non-interactively.
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "actions": {"secrets": {"ipc": true, "bridge": false, "keys": ["k"]}}"#,
    );
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let body = GIT_CLEAN_AND_PUSHED.replace(
        "\"app_version\":\"1.2.0\"}",
        "\"app_version\":\"1.2.0\",\"actions\":{\"secrets\":{\"ipc\":true}}}",
    );
    let git = Git::at(write_fake(scripts.path(), "git", &body));

    let error = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .unwrap_err();
    assert!(matches!(error, PublishError::IpcNotConfirmed), "{error}");
}

#[test]
fn ipc_off_or_bridge_only_never_asks() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(
        project.path(),
        "1.2.0",
        r#", "actions": {"secrets": {"ipc": false, "bridge": true, "keys": ["k"]}}"#,
    );
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));

    run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .expect("bridge-only must not trip the ipc gate");
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

fn tracked_source_paths() -> Vec<PathBuf> {
    Vec::from([
        PathBuf::from("link-to-main"),
        PathBuf::from("src/main.php"),
        PathBuf::from("tfsapp.config.json"),
    ])
}

struct TestBlobs(BTreeMap<String, Vec<u8>>);

impl BlobSource for TestBlobs {
    fn copy_blob(&mut self, object: &str, destination: &mut dyn Write) -> Result<u64, GitError> {
        let bytes = self.0.get(object).ok_or_else(|| GitError::MissingObject {
            object: object.to_string(),
        })?;
        destination
            .write_all(bytes)
            .expect("a writable archive blob");
        Ok(bytes.len() as u64)
    }
}

fn tree_from_paths(root: &Path, paths: &[PathBuf]) -> (Vec<TreeEntry>, TestBlobs) {
    let mut blobs = BTreeMap::new();
    let entries = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let source = root.join(path);
            let metadata = fs::symlink_metadata(&source).expect("tracked source metadata");
            let bytes = if metadata.file_type().is_symlink() {
                fs::read_link(&source)
                    .expect("a readable symlink")
                    .as_os_str()
                    .as_bytes()
                    .to_vec()
            } else {
                fs::read(&source).expect("a readable source blob")
            };
            let object_id = format!("test-{index}");
            blobs.insert(object_id.clone(), bytes);
            TreeEntry {
                path: path.clone(),
                mode: if metadata.file_type().is_symlink() {
                    0o120000
                } else if metadata.permissions().mode() & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                },
                object_id,
            }
        })
        .collect();
    (entries, TestBlobs(blobs))
}

#[test]
fn the_archive_extracts_to_a_tree_hashing_the_same_as_the_source() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let tracked_paths = tracked_source_paths();
    let (entries, mut blobs) = tree_from_paths(project.path(), &tracked_paths);

    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
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
fn the_archive_uses_the_captured_blobs_even_if_the_worktree_changes_afterwards() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    let executable = project.path().join("src/main.php");
    let mut permissions = fs::metadata(&executable)
        .expect("source metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("an executable source file");
    let binary = project.path().join("binary.dat");
    fs::write(&binary, [0, 0xff, b'\n']).expect("a binary source file");
    let mut tracked_paths = tracked_source_paths();
    tracked_paths.push(PathBuf::from("binary.dat"));
    let (entries, mut blobs) = tree_from_paths(project.path(), &tracked_paths);

    fs::write(&executable, "mutated after the snapshot\n").expect("a later worktree change");
    fs::write(&binary, "mutated binary\n").expect("a later binary change");
    fs::remove_file(project.path().join("link-to-main")).expect("a later worktree removal");

    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
        .expect("the pinned archive to build");
    let root = archive::extract(&assets.archive_path, &scratch.path().join("extracted"))
        .expect("the archive to extract");

    assert_eq!(
        fs::read_to_string(root.join("src/main.php")).unwrap(),
        "<?php\n"
    );
    assert_eq!(fs::read(root.join("binary.dat")).unwrap(), [0, 0xff, b'\n']);
    assert!(root.join("link-to-main").is_symlink());
    assert_eq!(
        fs::metadata(root.join("src/main.php"))
            .expect("extracted metadata")
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
}

#[test]
fn archive_exclusions_are_applied_before_a_blob_is_requested() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    let (mut entries, mut blobs) = tree_from_paths(project.path(), &tracked_source_paths());
    entries.push(TreeEntry {
        path: PathBuf::from("vendor/never-read.php"),
        mode: 0o100644,
        object_id: "missing-excluded-object".to_string(),
    });

    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
        .expect("an excluded blob must not be requested");
    let root = archive::extract(&assets.archive_path, &scratch.path().join("extracted"))
        .expect("the archive to extract");
    assert!(!root.join("vendor").exists());
}

#[test]
fn ignored_files_on_disk_are_not_written_to_the_archive() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    fs::write(project.path().join(".env.local"), "APP_SECRET=development").expect("a local env");
    let secrets = project.path().join("config/secrets/prod");
    fs::create_dir_all(&secrets).expect("a prod secrets dir");
    fs::write(
        secrets.join("prod.decrypt.private.php"),
        "<?php return [\x27key\x27 => \x27secret\x27];",
    )
    .expect("a prod secrets key");
    let cache = project.path().join(".phpunit.cache");
    fs::create_dir_all(&cache).expect("a PHPUnit cache dir");
    fs::write(cache.join("test-results"), "cache").expect("a PHPUnit cache entry");
    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let tracked_paths = tracked_source_paths();
    let (entries, mut blobs) = tree_from_paths(project.path(), &tracked_paths);

    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
        .expect("the archive to build");
    let root = archive::extract(&assets.archive_path, &scratch.path().join("extracted"))
        .expect("the archive to extract");

    for ignored in [
        ".env.local",
        "config/secrets/prod/prod.decrypt.private.php",
        ".phpunit.cache/test-results",
    ] {
        assert!(
            !root.join(ignored).exists(),
            "{ignored} must not be published"
        );
    }
}

#[test]
fn a_real_repository_archive_contains_exactly_its_tracked_paths() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let root = project.path();
    write_manifest(root, "1.2.0", "");
    write_changelog(root, CHANGELOG);
    fs::create_dir_all(root.join("src")).expect("a source dir");
    fs::write(root.join("src/main.php"), "<?php\n").expect("a source file");
    fs::write(
        root.join(".gitignore"),
        ".env.local\n.env.*.local\n/.phpunit.cache/\n/config/secrets/prod/prod.decrypt.private.php\n",
    )
    .expect("a Symfony-shaped gitignore");

    for arguments in [
        Vec::from(["init", "--quiet"]),
        Vec::from(["config", "user.email", "test.invalid"]),
        Vec::from(["config", "user.name", "TFSApp test"]),
        Vec::from(["add", "."]),
        Vec::from(["commit", "--quiet", "-m", "initial source"]),
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .expect("git to run")
                .success(),
            "git fixture setup must succeed"
        );
    }

    fs::write(root.join(".env.local"), "APP_SECRET=development").expect("a local env");
    let secrets = root.join("config/secrets/prod");
    fs::create_dir_all(&secrets).expect("a prod secrets dir");
    fs::write(secrets.join("prod.decrypt.private.php"), "<?php return [];")
        .expect("a prod secrets key");
    let cache = root.join(".phpunit.cache");
    fs::create_dir_all(&cache).expect("a PHPUnit cache dir");
    fs::write(cache.join("test-results"), "cache").expect("a PHPUnit cache entry");

    let tracked_paths = Git::new().ls_files(root).expect("a tracked file list");
    let archive_paths: Vec<_> = tracked_paths
        .into_iter()
        .filter(|path| !excluded_from_archive(path))
        .collect();
    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let (entries, mut blobs) = tree_from_paths(root, &archive_paths);
    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
        .expect("the archive to build");

    let file = fs::File::open(&assets.archive_path).expect("a readable archive");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let listed_paths: Vec<_> = archive
        .entries()
        .expect("archive entries")
        .map(|entry| {
            entry
                .expect("a readable entry")
                .path()
                .expect("an entry path")
                .into_owned()
        })
        .collect();
    let prefix = PathBuf::from("demo-1.2.0");
    assert_eq!(
        listed_paths,
        Vec::from([
            prefix.clone(),
            prefix.join(".gitignore"),
            prefix.join("CHANGELOG.md"),
            prefix.join("src"),
            prefix.join("src/main.php"),
            prefix.join("tfsapp.config.json"),
        ])
    );
}

#[test]
fn a_real_pinned_repository_ignores_later_tracked_changes_and_ignored_secrets() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let remote = tempfile::tempdir().expect("a bare remote");
    let root = project.path();
    write_manifest(root, "1.2.0", "");
    write_changelog(root, CHANGELOG);
    fs::create_dir_all(root.join("src")).expect("a source dir");
    let executable = root.join("src/main.php");
    fs::write(&executable, "<?php echo 'pinned';\n").expect("an executable source file");
    let mut permissions = fs::metadata(&executable)
        .expect("source metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).expect("a source executable bit");
    fs::write(root.join(".gitignore"), ".env.local\n").expect("a gitignore");

    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .current_dir(remote.path())
            .status()
            .expect("git to run")
            .success(),
        "bare remote setup must succeed"
    );
    for arguments in [
        Vec::from(["init", "--quiet"]),
        Vec::from(["config", "user.email", "test.invalid"]),
        Vec::from(["config", "user.name", "TFSApp test"]),
        Vec::from(["add", "."]),
        Vec::from(["commit", "--quiet", "-m", "pinned source"]),
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .expect("git to run")
                .success(),
            "project setup must succeed"
        );
    }
    assert!(
        Command::new("git")
            .args(["remote", "add", "origin"])
            .arg(remote.path())
            .current_dir(root)
            .status()
            .expect("git to run")
            .success(),
        "remote setup must succeed"
    );
    assert!(
        Command::new("git")
            .args(["push", "--set-upstream", "origin", "HEAD"])
            .current_dir(root)
            .status()
            .expect("git to run")
            .success(),
        "push must succeed"
    );

    let git = Git::new();
    let snapshot = git
        .snapshot(root, Some("owner/repo"))
        .expect("a clean pushed snapshot");
    let entries = git.tree_entries(root, &snapshot).expect("the pinned tree");

    fs::write(&executable, "<?php echo 'changed';\n").expect("a later tracked change");
    fs::write(root.join("tfsapp.config.json"), "{}").expect("a later manifest change");
    fs::write(root.join("CHANGELOG.md"), "## 9.9.9\n").expect("a later changelog change");
    fs::write(root.join(".env.local"), "APP_SECRET=not-published").expect("an ignored secret");

    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let mut blobs = git.blob_reader(root).expect("a pinned blob reader");
    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
        .expect("an archive from the pinned commit");
    let extracted = archive::extract(&assets.archive_path, &scratch.path().join("extracted"))
        .expect("the archive to extract");

    assert_eq!(
        fs::read_to_string(extracted.join("src/main.php")).expect("pinned executable"),
        "<?php echo 'pinned';\n"
    );
    assert_eq!(
        fs::metadata(extracted.join("src/main.php"))
            .expect("extracted metadata")
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert!(
        !extracted.join(".env.local").exists(),
        "an ignored secret must not be archived"
    );
    assert!(fs::read_to_string(extracted.join("tfsapp.config.json"))
        .expect("pinned manifest")
        .contains("\"app_version\": \"1.2.0\""));
    assert!(fs::read_to_string(extracted.join("CHANGELOG.md"))
        .expect("pinned changelog")
        .contains("## 1.2.0"));
}

#[test]
fn the_sums_file_verifies_against_the_archive_it_names() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    write_source_files(project.path());
    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let tracked_paths = tracked_source_paths();
    let (entries, mut blobs) = tree_from_paths(project.path(), &tracked_paths);

    let assets = build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path())
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
    let mut tracked_paths = tracked_source_paths();
    tracked_paths.push(PathBuf::from("evil"));
    let (entries, mut blobs) = tree_from_paths(project.path(), &tracked_paths);

    let error =
        build_archive(&entries, &mut blobs, &[], "demo", "1.2.0", scratch.path()).unwrap_err();
    match &error {
        PublishError::EscapingSymlink { path } => {
            assert_eq!(path, &PathBuf::from("evil"));
        }
        other => panic!("expected EscapingSymlink, got {other}"),
    }
}

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

/// A project ready to publish: a valid manifest at `app_version`, a matching
/// `CHANGELOG.md` section (from the shared `CHANGELOG` fixture, so callers
/// must pass `"1.2.0"`), and a small source tree.
fn publishable_project(root: &Path, app_version: &str) {
    write_manifest(root, app_version, "");
    write_changelog(root, CHANGELOG);
    write_source_files(root);
}

/// The fake `gh` for the happy path: installed, authenticated, no colliding
/// release, and `release create` succeeds.
const GH_EVERY_GATE_PASSES: &str = r#"
case "$1" in
  --version) exit 0 ;;
esac
case "$1 $2" in
  "auth status") exit 0 ;;
  "release view") echo 'release not found' 1>&2; exit 1 ;;
  "release create")
    echo "https://github.com/owner/repo/releases/tag/v1.2.0"
    exit 0
    ;;
esac
exit 1
"#;

#[test]
fn publish_runs_the_whole_pipeline_and_leaves_no_scratch_behind() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");

    let git_scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(git_scripts.path(), "git", GIT_CLEAN_AND_PUSHED));
    let gh_scripts = tempfile::tempdir().expect("a temp dir for the fake gh");
    let gh = Gh::at(write_fake(gh_scripts.path(), "gh", GH_EVERY_GATE_PASSES));
    let (_base, paths) = temp_paths();

    let published = publish(&paths, project.path(), None, true, &git, &gh)
        .expect("the whole pipeline to succeed");
    assert!(published);

    assert!(
        !paths.scratch_dir().exists(),
        "scratch must be removed after a successful publish"
    );
    assert!(argv_log(gh_scripts.path())
        .contains("release create v1.2.0 --repo owner/repo --title v1.2.0"));
    assert!(argv_log(gh_scripts.path()).contains("--target deadbeefcafe1234"));
}

#[test]
fn publish_uses_pinned_metadata_notes_and_bytes_after_the_worktree_changes() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");

    let git_body = GIT_CLEAN_AND_PUSHED.replace(
        "  ls-tree)\n",
        "  ls-tree)\n    printf '%s' '{\"product_name\":\"Changed\",\"identifier\":\"dev.local.changed\",\"project_name\":\"changed\",\"app_version\":\"9.9.9\"}' > \"$2/tfsapp.config.json\"\n    printf '%s' '## 9.9.9\\n\\nChanged notes.\\n' > \"$2/CHANGELOG.md\"\n    printf '%s' '<?php changed\\n' > \"$2/src/main.php\"\n",
    );
    let git_scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(git_scripts.path(), "git", &git_body));
    let gh_scripts = tempfile::tempdir().expect("a temp dir for the fake gh");
    let gh = Gh::at(write_fake(
        gh_scripts.path(),
        "gh",
        r#"
case "$1" in
  --version) exit 0 ;;
esac
case "$1 $2" in
  "auth status") exit 0 ;;
  "release view") echo 'release not found' 1>&2; exit 1 ;;
  "release create")
    previous=''
    for argument in "$@"; do
      if [ "$previous" = notes ]; then
        cp "$argument" "$(dirname "$0")/published-notes.md"
        previous=''
      fi
      case "$argument" in
        --notes-file) previous=notes ;;
        *.tar.gz) cp "$argument" "$(dirname "$0")/published.tar.gz" ;;
      esac
    done
    echo "https://github.com/owner/repo/releases/tag/v1.2.0"
    exit 0
    ;;
esac
exit 1
"#,
    ));
    let (_base, paths) = temp_paths();

    assert!(publish(&paths, project.path(), None, true, &git, &gh).expect("a pinned publish"));
    assert!(argv_log(gh_scripts.path()).contains("release create v1.2.0"));
    assert!(argv_log(gh_scripts.path()).contains("--target deadbeefcafe1234"));
    assert_eq!(
        fs::read_to_string(gh_scripts.path().join("published-notes.md")).expect("copied notes"),
        "Added the frobnicator.\nFixed the widget."
    );

    let file = fs::File::open(gh_scripts.path().join("published.tar.gz")).expect("copied archive");
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut contents = BTreeMap::new();
    for entry in archive.entries().expect("archive entries") {
        let mut entry = entry.expect("readable entry");
        let path = entry.path().expect("entry path").into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("entry bytes");
        contents.insert(path, bytes);
    }
    assert_eq!(
        contents[&PathBuf::from("demo-1.2.0/tfsapp.config.json")],
        b"{\"product_name\":\"Demo App\",\"identifier\":\"dev.local.demo\",\"project_name\":\"demo\",\"app_version\":\"1.2.0\"}"
    );
    assert_eq!(
        contents[&PathBuf::from("demo-1.2.0/src/main.php")],
        b"<?php\n"
    );
}

#[test]
fn a_local_gate_failure_never_reaches_gh_and_leaves_no_scratch_behind() {
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", "");
    // No CHANGELOG.md at all — gate 7, reached only once the git gate (3-6)
    // has already passed.
    write_source_files(project.path());

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let body = GIT_CLEAN_AND_PUSHED.replace(
        "    printf '100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\tCHANGELOG.md\\0'\n",
        "",
    );
    let git = Git::at(write_fake(scripts.path(), "git", &body));
    let (_base, paths) = temp_paths();

    let error = publish(
        &paths,
        project.path(),
        Some("owner/repo"),
        true,
        &git,
        &gh_never_called(),
    )
    .unwrap_err();
    assert!(
        matches!(error, PublishError::MissingChangelog { .. }),
        "{error}"
    );
    assert!(!paths.scratch_dir().exists());
}

#[test]
fn a_dirty_tree_refuses_before_gh_is_ever_called() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(
        scripts.path(),
        "git",
        r#"
case "$3" in
  rev-parse)
    [ "$4" = "--show-toplevel" ] && { echo "/repo"; exit 0; }
    ;;
  status)
    printf '## main...origin/main\n?? untracked.txt\n'
    exit 0
    ;;
esac
exit 1
"#,
    ));
    let (_base, paths) = temp_paths();

    let error = publish(
        &paths,
        project.path(),
        Some("owner/repo"),
        true,
        &git,
        &gh_never_called(),
    )
    .unwrap_err();
    assert!(
        matches!(error, PublishError::Git(GitError::Dirty { .. })),
        "{error}"
    );
    assert!(!paths.scratch_dir().exists());
}

#[test]
fn gh_not_installed_refuses_before_the_archive_is_built() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));
    let (_base, paths) = temp_paths();

    let error = publish(
        &paths,
        project.path(),
        Some("owner/repo"),
        true,
        &git,
        &gh_never_called(),
    )
    .unwrap_err();
    match &error {
        PublishError::Gh(GhError::NotInstalled { .. }) => {}
        other => panic!("expected Gh(NotInstalled), got {other}"),
    }
    assert!(!paths.scratch_dir().exists());
}

#[test]
fn gh_not_authenticated_refuses_before_the_archive_is_built() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");

    let git_scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(git_scripts.path(), "git", GIT_CLEAN_AND_PUSHED));
    let gh_scripts = tempfile::tempdir().expect("a temp dir for the fake gh");
    let gh = Gh::at(write_fake(
        gh_scripts.path(),
        "gh",
        r#"
case "$1" in
  --version) exit 0 ;;
esac
case "$1 $2" in
  "auth status") exit 1 ;;
esac
exit 1
"#,
    ));
    let (_base, paths) = temp_paths();

    let error = publish(&paths, project.path(), Some("owner/repo"), true, &git, &gh).unwrap_err();
    assert!(
        matches!(error, PublishError::Gh(GhError::NotAuthenticated)),
        "{error}"
    );
    assert!(!paths.scratch_dir().exists());
    assert!(!argv_log(gh_scripts.path()).contains("release create"));
}

#[test]
fn an_existing_release_refuses_before_the_archive_is_built() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");

    let git_scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(git_scripts.path(), "git", GIT_CLEAN_AND_PUSHED));
    let gh_scripts = tempfile::tempdir().expect("a temp dir for the fake gh");
    let gh = Gh::at(write_fake(
        gh_scripts.path(),
        "gh",
        r#"
case "$1" in
  --version) exit 0 ;;
esac
case "$1 $2" in
  "auth status") exit 0 ;;
  "release view")
    echo '{"isDraft":false,"assets":[]}'
    exit 0
    ;;
esac
exit 1
"#,
    ));
    let (_base, paths) = temp_paths();

    let error = publish(&paths, project.path(), Some("owner/repo"), true, &git, &gh).unwrap_err();
    assert!(
        matches!(error, PublishError::Gh(GhError::ReleaseExists { .. })),
        "{error}"
    );
    assert!(!paths.scratch_dir().exists());
    assert!(!argv_log(gh_scripts.path()).contains("release create"));
}

#[test]
fn an_escaping_symlink_refuses_after_every_gh_gate_and_still_leaves_no_scratch_behind() {
    let project = tempfile::tempdir().expect("a temp project dir");
    publishable_project(project.path(), "1.2.0");
    symlink("../../outside", project.path().join("evil")).expect("an escaping symlink");

    let git_scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git_body = GIT_CLEAN_AND_PUSHED
        .replace(
            "CHANGELOG.md\\0link-to-main\\0src/main.php\\0tfsapp.config.json\\0'",
            "CHANGELOG.md\\0link-to-main\\0src/main.php\\0tfsapp.config.json\\0evil\\0'",
        )
        .replace(
            "    exit 0\n    ;;\n  cat-file)",
            "    printf '120000 blob eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\\tevil\\0'\n    exit 0\n    ;;\n  cat-file)",
        )
        .replace(
            "        d*) value='{\"product_name\":\"Demo App\",\"identifier\":\"dev.local.demo\",\"project_name\":\"demo\",\"app_version\":\"1.2.0\"}'; printf '%s blob %s\\n%s\\n' \"$object\" \"${#value}\" \"$value\" ;;",
            "        d*) value='{\"product_name\":\"Demo App\",\"identifier\":\"dev.local.demo\",\"project_name\":\"demo\",\"app_version\":\"1.2.0\"}'; printf '%s blob %s\\n%s\\n' \"$object\" \"${#value}\" \"$value\" ;;\n        e*) printf '%s blob 13\\n../../outside\\n' \"$object\" ;;",
        );
    let git = Git::at(write_fake(git_scripts.path(), "git", &git_body));
    let gh_scripts = tempfile::tempdir().expect("a temp dir for the fake gh");
    let gh = Gh::at(write_fake(gh_scripts.path(), "gh", GH_EVERY_GATE_PASSES));
    let (_base, paths) = temp_paths();

    let error = publish(&paths, project.path(), Some("owner/repo"), true, &git, &gh).unwrap_err();
    assert!(
        matches!(error, PublishError::EscapingSymlink { .. }),
        "{error}"
    );
    assert!(!paths.scratch_dir().exists());
    assert!(!argv_log(gh_scripts.path()).contains("release create"));
}

#[test]
fn local_publish_from_unpushed_commit_is_atomic_and_verifiable() {
    let project = tempfile::tempdir().unwrap();
    publishable_project(project.path(), "1.2.0");
    let scripts = tempfile::tempdir().unwrap();
    let body = GIT_CLEAN_AND_PUSHED.replace("## main...origin/main", "## main");
    let git = Git::at(write_fake(scripts.path(), "git", &body));
    let out = tempfile::tempdir().unwrap();
    assert!(publish_local(project.path(), out.path(), true, &git).unwrap());
    let folder = out.path().join("demo-1.2.0");
    let mut names: Vec<_> = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["NOTES.md", "SHA256SUMS.txt", "demo-1.2.0.tar.gz"]);
    let archive = folder.join("demo-1.2.0.tar.gz");
    let sums =
        release::parse_sha256sums(&fs::read_to_string(folder.join("SHA256SUMS.txt")).unwrap());
    let actual = release::sha256_file(&archive).unwrap();
    assert!(matches!(
        release::verify(&sums, "demo-1.2.0.tar.gz", &actual),
        release::VerifyOutcome::Match
    ));
    assert!(fs::read_to_string(folder.join("NOTES.md"))
        .unwrap()
        .ends_with("Built from commit deadbeefcafe1234\n"));
    let before = fs::read(&archive).unwrap();
    assert!(matches!(
        publish_local(project.path(), out.path(), true, &git),
        Err(PublishError::LocalReleaseExists { .. })
    ));
    assert_eq!(fs::read(&archive).unwrap(), before);
}

#[test]
fn local_publish_refuses_dirty_or_missing_destination_and_cleans_up_on_decline() {
    let project = tempfile::tempdir().unwrap();
    publishable_project(project.path(), "1.2.0");
    let scripts = tempfile::tempdir().unwrap();
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));
    let out = tempfile::tempdir().unwrap();
    assert!(matches!(
        publish_local(project.path(), &out.path().join("missing"), true, &git),
        Err(PublishError::OutputDirMissing { .. })
    ));
    assert!(!publish_local(project.path(), out.path(), false, &git).unwrap());
    assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
    let dirty = GIT_CLEAN_AND_PUSHED.replace(
        "printf '## main...origin/main\\n'",
        "printf '## main...origin/main\\n M src/main.php\\n'",
    );
    let dirty_git = Git::at(write_fake(scripts.path(), "dirty-git", &dirty));
    assert!(matches!(
        publish_local(project.path(), out.path(), true, &dirty_git),
        Err(PublishError::Git(GitError::Dirty { .. }))
    ));
    assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
}

// --- declared build outputs (plan 071) -------------------------------------

/// A committed, clean project in a real temporary Git repository — the
/// shape the working-tree checks need, since only real Git answers
/// `check-ignore`. The `.gitignore` is written before the one commit, so
/// it is tracked with everything else.
fn real_git_project(root: &Path, manifest_extra: &str, gitignore: &str) {
    write_manifest(root, "1.2.0", manifest_extra);
    write_changelog(root, CHANGELOG);
    fs::write(root.join(".gitignore"), gitignore).expect("a gitignore");
    for arguments in [
        Vec::from(["init", "--quiet"]),
        Vec::from(["config", "user.email", "test.invalid"]),
        Vec::from(["config", "user.name", "TFSApp test"]),
        Vec::from(["add", "."]),
        Vec::from(["commit", "--quiet", "-m", "source"]),
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .expect("git to run")
                .success(),
            "git fixture setup must succeed"
        );
    }
}

/// Run every gate the working-tree way: a local snapshot of a real,
/// already-committed repository.
fn local_gates(root: &Path) -> Result<super::LocalGates, PublishError> {
    run_local_gates(root, PublishTarget::Local, &Git::new())
}

fn declared_output() -> &'static str {
    r#", "build_outputs": ["public/build"]"#
}

#[test]
fn a_declared_output_that_is_absent_is_refused_naming_the_path() {
    let project = tempfile::tempdir().expect("a temp project dir");
    real_git_project(project.path(), declared_output(), "/public/build\n");

    let error = local_gates(project.path()).unwrap_err();
    match &error {
        PublishError::BuildOutputAbsent { declared } => assert_eq!(declared, "public/build"),
        other => panic!("expected BuildOutputAbsent, got {other}"),
    }
    assert!(error.to_string().contains("public/build"), "{error}");
}

#[test]
fn a_declared_output_that_is_empty_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    real_git_project(project.path(), declared_output(), "/public/build\n");
    fs::create_dir_all(project.path().join("public/build")).expect("an empty build dir");

    let error = local_gates(project.path()).unwrap_err();
    assert!(
        matches!(error, PublishError::BuildOutputEmpty { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("public/build"), "{error}");
}

#[test]
fn a_declared_output_that_is_not_a_directory_is_refused() {
    let project = tempfile::tempdir().expect("a temp project dir");
    // No trailing slash, so the rule covers a plain file too and the tree
    // stays clean.
    real_git_project(project.path(), declared_output(), "/public/build\n");
    fs::create_dir_all(project.path().join("public")).expect("a public dir");
    fs::write(project.path().join("public/build"), "not a directory").expect("a plain file");

    let error = local_gates(project.path()).unwrap_err();
    assert!(
        matches!(error, PublishError::BuildOutputNotADirectory { .. }),
        "{error}"
    );
}

#[test]
fn a_declared_output_resolving_outside_the_project_is_refused() {
    // The declared path itself never climbs — a symlinked parent does the
    // escaping, and only resolution can see it.
    let project = tempfile::tempdir().expect("a temp project dir");
    let outside = tempfile::tempdir().expect("a temp dir outside the project");
    real_git_project(project.path(), declared_output(), "/public\n");
    fs::create_dir_all(outside.path().join("build")).expect("an outside build dir");
    fs::write(outside.path().join("build/app.css"), "body {}").expect("a file");
    symlink(outside.path(), project.path().join("public")).expect("a symlinked parent");

    let error = local_gates(project.path()).unwrap_err();
    assert!(
        matches!(error, PublishError::BuildOutputEscapes { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("public/build"), "{error}");
}

#[test]
fn a_declared_output_the_ignore_rules_do_not_cover_is_refused() {
    // Every file inside is ignored, so the tree stays clean — but the
    // declared directory itself matches no rule, and a declared output
    // must be ignored, not merely full of ignored files.
    let project = tempfile::tempdir().expect("a temp project dir");
    real_git_project(project.path(), declared_output(), "*.css\n");
    let build = project.path().join("public/build");
    fs::create_dir_all(&build).expect("a build dir");
    fs::write(build.join("app.css"), "body {}").expect("an ignored file");

    let error = local_gates(project.path()).unwrap_err();
    assert!(
        matches!(error, PublishError::BuildOutputNotIgnored { .. }),
        "{error}"
    );
}

#[test]
fn a_declared_output_holding_a_tracked_file_is_refused_naming_the_file() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let root = project.path();
    real_git_project(root, declared_output(), "/public/build\n");
    let tracked = root.join("public/build/tracked.css");
    fs::create_dir_all(tracked.parent().expect("a build dir")).expect("a build dir");
    fs::write(&tracked, "body {}").expect("a tracked file");
    for arguments in [
        Vec::from(["add", "-f", "public/build/tracked.css"]),
        Vec::from(["commit", "--quiet", "-m", "tracked output"]),
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .expect("git to run")
                .success(),
            "git fixture setup must succeed"
        );
    }

    let error = local_gates(root).unwrap_err();
    match &error {
        PublishError::BuildOutputTrackedFile { declared, file } => {
            assert_eq!(declared, "public/build");
            assert_eq!(file, &PathBuf::from("public/build/tracked.css"));
        }
        other => panic!("expected BuildOutputTrackedFile, got {other}"),
    }
}

#[test]
fn a_symlink_inside_a_declared_output_is_refused() {
    // The same extraction rule install applies (plan 062), applied before
    // a byte of the archive is written.
    let project = tempfile::tempdir().expect("a temp project dir");
    real_git_project(project.path(), declared_output(), "/public/build\n");
    let build = project.path().join("public/build");
    fs::create_dir_all(&build).expect("a build dir");
    fs::write(build.join("app.css"), "body {}").expect("a file");
    symlink("../CHANGELOG.md", build.join("link")).expect("a symlink inside");

    let error = local_gates(project.path()).unwrap_err();
    match &error {
        PublishError::BuildOutputIrregularEntry { entry, .. } => {
            assert_eq!(entry, &PathBuf::from("public/build/link"));
        }
        other => panic!("expected BuildOutputIrregularEntry, got {other}"),
    }
}

#[test]
fn the_pinned_manifest_decides_what_is_declared_not_the_worktrees() {
    // The working tree's manifest declares an output that does not exist;
    // the pinned commit's declares nothing. Only the pinned declaration
    // counts, so no working-tree check ever runs (plan 071's "Important
    // rules": the declaring manifest is the pinned one, never the working
    // tree's).
    let project = tempfile::tempdir().expect("a temp project dir");
    write_manifest(project.path(), "1.2.0", declared_output());
    write_changelog(project.path(), CHANGELOG);

    let scripts = tempfile::tempdir().expect("a temp dir for the fake git");
    let git = Git::at(write_fake(scripts.path(), "git", GIT_CLEAN_AND_PUSHED));

    let gates = run_local_gates(
        project.path(),
        PublishTarget::Forge(Some("owner/repo")),
        &git,
    )
    .expect("the pinned declaration is the one that counts");
    assert!(gates.build_outputs.is_empty());
}

#[test]
fn a_declared_output_ships_in_the_archive_with_its_stats_line() {
    let project = tempfile::tempdir().expect("a temp project dir");
    let root = project.path();
    real_git_project(root, declared_output(), "/public/build\n");
    let build = root.join("public/build");
    fs::create_dir_all(&build).expect("a build dir");
    fs::write(build.join("entrypoints.json"), "0123456789").expect("an entrypoints file");
    let script = build.join("app.js");
    fs::write(&script, "console.log(1);\n").expect("a script");
    let mut permissions = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("an executable script");

    // A known age, so the stats line is assertable: every file in the
    // output exactly three days old.
    let three_days_ago = SystemTime::now()
        .checked_sub(Duration::from_secs(3 * 86_400))
        .expect("a computable instant");
    let epoch = three_days_ago
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("an epoch")
        .as_secs();
    for file in [&build.join("entrypoints.json"), &script] {
        assert!(
            Command::new("touch")
                .arg("-d")
                .arg(format!("@{epoch}"))
                .arg(file)
                .status()
                .expect("touch to run")
                .success(),
            "touch fixture must succeed"
        );
    }

    let mut gates = local_gates(root).expect("every gate to pass");
    assert_eq!(gates.build_outputs.len(), 1);
    let output = &gates.build_outputs[0];
    assert_eq!(output.declared, "public/build");
    assert_eq!(output.files.len(), 2);
    assert_eq!(output.total_size, 26);
    assert_eq!(
        output.stats_line(),
        "build_outputs: public/build — 2 files, 26 B, newest 3 days ago"
    );

    let scratch = tempfile::tempdir().expect("a temp scratch dir");
    let assets = build_archive(
        &gates.entries,
        &mut gates.blobs,
        &gates.build_outputs,
        "demo",
        "1.2.0",
        scratch.path(),
    )
    .expect("the archive to build");
    let extracted = archive::extract(&assets.archive_path, &scratch.path().join("extracted"))
        .expect("the archive to extract");

    assert_eq!(
        fs::read_to_string(extracted.join("public/build/entrypoints.json"))
            .expect("the entrypoints file"),
        "0123456789"
    );
    assert_eq!(
        fs::metadata(extracted.join("public/build/app.js"))
            .expect("script metadata")
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(
        source::tree_hash(&extracted).expect("a hash of the extracted tree"),
        source::tree_hash(root).expect("a hash of the source tree"),
        "the archive must round-trip to the tracked tree plus the declared output"
    );
}

#[test]
fn a_declared_file_that_changes_size_after_the_walk_is_refused() {
    // A build still running while publishing: the walk measured one size,
    // the archive would read another. Both directions are refused rather
    // than written into a header that no longer matches its bytes.
    for (label, rewritten) in [("shorter", "short"), ("longer", "a much longer body")] {
        let project = tempfile::tempdir().expect("a temp project dir");
        let root = project.path();
        real_git_project(root, declared_output(), "/public/build\n");
        let build = root.join("public/build");
        fs::create_dir_all(&build).expect("a build dir");
        fs::write(build.join("app.js"), "console.log(1);\n").expect("a script");

        let mut gates = local_gates(root).expect("every gate to pass");
        fs::write(build.join("app.js"), rewritten).expect("the build rewrites it");

        let scratch = tempfile::tempdir().expect("a temp scratch dir");
        let error = build_archive(
            &gates.entries,
            &mut gates.blobs,
            &gates.build_outputs,
            "demo",
            "1.2.0",
            scratch.path(),
        )
        .unwrap_err();
        match &error {
            PublishError::BuildOutputChanged { file } => {
                assert_eq!(file, &PathBuf::from("public/build/app.js"), "{label}");
            }
            other => panic!("{label}: expected BuildOutputChanged, got {other}"),
        }
    }
}

#[test]
fn the_stats_line_counts_in_the_singular_when_there_is_one() {
    assert_eq!(super::counted(1, "file"), "1 file");
    assert_eq!(super::counted(2, "file"), "2 files");
    let one_hour_ago = SystemTime::now()
        .checked_sub(Duration::from_secs(3600 + 30))
        .expect("a computable instant");
    assert_eq!(super::format_age(one_hour_ago), "1 hour ago");
}
