use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use super::{release_view_failure, Gh, GhError, ReleaseViewFailure};

/// A fake `gh` at `dir/gh`: every call is first appended (as its
/// space-joined argv) to `dir/argv.log`, then dispatched to `body` — a shell
/// `case` (or bare `exit`) supplied per test, since each test only cares
/// about the one or two calls its own scenario makes.
fn write_fake_gh(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("gh");
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/argv.log\"\n{body}\n"),
    )
    .expect("a fake gh script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("+x");
    path
}

fn argv_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
}

fn write_assets(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let notes_path = dir.join("notes.md");
    fs::write(&notes_path, "Added the frobnicator.\n").expect("a notes file");
    let archive_path = dir.join("demo-1.2.0.tar.gz");
    fs::write(&archive_path, b"archive").expect("an archive");
    let sums_path = dir.join("SHA256SUMS.txt");
    fs::write(&sums_path, "deadbeef  demo-1.2.0.tar.gz\n").expect("a sums file");
    (notes_path, archive_path, sums_path)
}

#[test]
fn the_installed_gate_refuses_when_gh_cannot_be_run() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh = Gh::at(scripts.path().join("no-such-gh"));

    let error = gh.ensure_installed().unwrap_err();
    assert!(matches!(error, GhError::NotInstalled { .. }), "{error}");
    assert!(
        error.to_string().contains("No such file"),
        "expected the spawn's own io::Error in the message, got: {error}"
    );
    assert!(
        std::error::Error::source(&error).is_some(),
        "NotInstalled should expose its io::Error via Error::source too"
    );
}

#[test]
fn the_installed_gate_passes_when_gh_reports_a_version() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(scripts.path(), "exit 0\n");
    let gh = Gh::at(gh_path);

    gh.ensure_installed().expect("a zero exit to pass the gate");
}

#[test]
fn the_authenticated_gate_refuses_when_gh_auth_status_fails() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "auth status") exit 1 ;;
esac
exit 0
"#,
    );
    let gh = Gh::at(gh_path);

    let error = gh.ensure_authenticated().unwrap_err();
    assert!(matches!(error, GhError::NotAuthenticated), "{error}");
    assert!(error.to_string().contains("gh auth login"));
}

#[test]
fn the_authenticated_gate_passes_when_gh_auth_status_succeeds() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "auth status") exit 0 ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    gh.ensure_authenticated()
        .expect("a zero exit from auth status to pass the gate");
}

#[test]
fn the_version_guard_passes_when_no_release_carries_the_tag() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release view") echo 'release not found' 1>&2; exit 1 ;;
esac
exit 0
"#,
    );
    let gh = Gh::at(gh_path);

    gh.ensure_no_existing_release("owner/repo", "v1.2.0")
        .expect("no colliding release should pass the gate");
}

#[test]
fn only_ghs_not_found_reply_means_no_release_exists() {
    assert_eq!(
        release_view_failure("release not found"),
        ReleaseViewFailure::NotFound
    );
    assert_eq!(
        release_view_failure("HTTP 502: Bad Gateway"),
        ReleaseViewFailure::Other
    );
    assert_eq!(
        release_view_failure("failed to connect to github.com"),
        ReleaseViewFailure::Other
    );
}

#[test]
fn the_version_guard_propagates_a_release_view_failure() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release view") echo 'failed to connect to github.com' 1>&2; exit 1 ;;
esac
exit 0
"#,
    );
    let gh = Gh::at(gh_path);

    let error = gh
        .ensure_no_existing_release("owner/repo", "v1.2.0")
        .unwrap_err();
    assert!(
        matches!(error, GhError::ReleaseViewFailed { .. }),
        "{error}"
    );
}

#[test]
fn the_version_guard_refuses_naming_an_existing_release() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release view")
    echo '{"isDraft":false,"assets":[]}'
    exit 0
    ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    let error = gh
        .ensure_no_existing_release("owner/repo", "v1.2.0")
        .unwrap_err();
    match &error {
        GhError::ReleaseExists { repo, tag, draft } => {
            assert_eq!(repo, "owner/repo");
            assert_eq!(tag, "v1.2.0");
            assert!(!draft);
        }
        other => panic!("expected ReleaseExists, got {other}"),
    }
    assert!(!error.to_string().contains("draft"));
}

#[test]
fn the_version_guard_refuses_naming_a_draft() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release view")
    echo '{"isDraft":true,"assets":[]}'
    exit 0
    ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    let error = gh
        .ensure_no_existing_release("owner/repo", "v1.2.0")
        .unwrap_err();
    match &error {
        GhError::ReleaseExists { draft, .. } => assert!(draft),
        other => panic!("expected ReleaseExists, got {other}"),
    }
    assert!(error.to_string().contains("draft"));
}

#[test]
fn the_version_guard_reports_unparseable_json_rather_than_panicking() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release view")
    echo 'not json'
    exit 0
    ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    let error = gh
        .ensure_no_existing_release("owner/repo", "v1.2.0")
        .unwrap_err();
    assert!(matches!(error, GhError::UnreadableJson { .. }), "{error}");
}

#[test]
fn create_release_posts_the_exact_argv_and_never_generate_notes() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release create")
    echo "https://github.com/owner/repo/releases/tag/v1.2.0"
    exit 0
    ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    let assets = tempfile::tempdir().expect("a temp dir for the assets");
    let (notes_path, archive_path, sums_path) = write_assets(assets.path());

    let url = gh
        .create_release(
            "owner/repo",
            "v1.2.0",
            &notes_path,
            "deadbeefcafe",
            "demo-1.2.0.tar.gz",
            &archive_path,
            &sums_path,
        )
        .expect("the fake gh to accept the call");

    assert_eq!(url, "https://github.com/owner/repo/releases/tag/v1.2.0");

    let expected = format!(
        "release create v1.2.0 --repo owner/repo --title v1.2.0 --notes-file {} --target \
         deadbeefcafe {} {}\n",
        notes_path.display(),
        archive_path.display(),
        sums_path.display()
    );
    assert_eq!(argv_log(scripts.path()), expected);
}

#[test]
fn a_failed_upload_is_cleaned_up_when_the_release_is_missing_an_asset() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release create")
    echo "422 asset upload failed" 1>&2
    exit 1
    ;;
  "release view")
    echo '{"isDraft":false,"assets":[{"name":"demo-1.2.0.tar.gz"}]}'
    exit 0
    ;;
  "release delete")
    exit 0
    ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    let assets = tempfile::tempdir().expect("a temp dir for the assets");
    let (notes_path, archive_path, sums_path) = write_assets(assets.path());

    let error = gh
        .create_release(
            "owner/repo",
            "v1.2.0",
            &notes_path,
            "deadbeefcafe",
            "demo-1.2.0.tar.gz",
            &archive_path,
            &sums_path,
        )
        .unwrap_err();

    match &error {
        GhError::CreateFailed {
            stderr,
            incomplete_release_deleted,
        } => {
            assert!(stderr.contains("422"));
            assert!(incomplete_release_deleted);
        }
        other => panic!("expected CreateFailed, got {other}"),
    }
    assert!(error.to_string().contains("was deleted"));
    assert!(argv_log(scripts.path()).contains("release delete v1.2.0 --repo owner/repo --yes"));
}

#[test]
fn a_failed_upload_is_left_alone_when_the_release_already_has_both_assets() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release create")
    echo "a network blip" 1>&2
    exit 1
    ;;
  "release view")
    echo '{"isDraft":false,"assets":[{"name":"demo-1.2.0.tar.gz"},{"name":"SHA256SUMS.txt"}]}'
    exit 0
    ;;
  "release delete")
    echo "must not be called" 1>&2
    exit 1
    ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);

    let assets = tempfile::tempdir().expect("a temp dir for the assets");
    let (notes_path, archive_path, sums_path) = write_assets(assets.path());

    let error = gh
        .create_release(
            "owner/repo",
            "v1.2.0",
            &notes_path,
            "deadbeefcafe",
            "demo-1.2.0.tar.gz",
            &archive_path,
            &sums_path,
        )
        .unwrap_err();

    match &error {
        GhError::CreateFailed {
            incomplete_release_deleted,
            ..
        } => assert!(!incomplete_release_deleted),
        other => panic!("expected CreateFailed, got {other}"),
    }
    assert!(!error.to_string().contains("was deleted"));
    assert!(!argv_log(scripts.path()).contains("release delete"));
}

#[test]
fn a_failed_upload_propagates_a_failed_cleanup_lookup() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let gh_path = write_fake_gh(
        scripts.path(),
        r#"
case "$1 $2" in
  "release create") echo '422 asset upload failed' 1>&2; exit 1 ;;
  "release view") echo 'failed to connect to github.com' 1>&2; exit 1 ;;
esac
exit 1
"#,
    );
    let gh = Gh::at(gh_path);
    let assets = tempfile::tempdir().expect("a temp dir for the assets");
    let (notes_path, archive_path, sums_path) = write_assets(assets.path());

    let error = gh
        .create_release(
            "owner/repo",
            "v1.2.0",
            &notes_path,
            "deadbeefcafe",
            "demo-1.2.0.tar.gz",
            &archive_path,
            &sums_path,
        )
        .unwrap_err();

    assert!(
        matches!(error, GhError::ReleaseViewFailed { .. }),
        "{error}"
    );
}
