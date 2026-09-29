use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use super::{parse_branch_line, parse_ls_files, parse_ls_tree, parse_status, Git, GitError};

/// A fake `git` at `dir/git`, logging its own argv (space-joined) to
/// `dir/argv.log` before dispatching — see `gh_tests.rs`'s own copy of this
/// helper for the rationale.
fn write_fake_git(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("git");
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/argv.log\"\n{body}\n"),
    )
    .expect("a fake git script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("+x");
    path
}

fn argv_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
}

/// The fake `git` for the happy path: a work tree, a clean status on branch
/// `main` tracking `origin/main` with no divergence, a fixed HEAD sha, and
/// `origin`'s URL configurable per test via `$GIT_TEST_REMOTE_URL` — read at
/// dispatch time since the script is shared across the two remote-spelling
/// tests.
fn clean_and_pushed_script(remote_url: &str) -> String {
    format!(
        r#"
case "$3" in
  rev-parse)
    case "$4" in
      --show-toplevel) echo "/repo"; exit 0 ;;
      HEAD) echo "abc123deadbeef"; exit 0 ;;
    esac
    ;;
  status)
    printf '## main...origin/main\n'
    exit 0
    ;;
  remote)
    if [ "$4" = "get-url" ]; then
      echo "{remote_url}"
      exit 0
    fi
    ;;
esac
exit 1
"#
    )
}

#[test]
fn a_spawn_failure_names_the_real_reason_instead_of_only_claiming_not_installed() {
    let git = Git::at(PathBuf::from("/nonexistent/git"));

    let error = git
        .ensure_pushed(Path::new("/some/project"), None)
        .unwrap_err();
    assert!(matches!(error, GitError::NotInstalled { .. }), "{error}");
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
fn not_a_work_tree_refuses_before_anything_else() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(scripts.path(), "exit 1\n"));

    let error = git
        .ensure_pushed(Path::new("/some/project"), None)
        .unwrap_err();
    assert!(matches!(error, GitError::NotAWorkTree { .. }), "{error}");
}

#[test]
fn a_dirty_project_directory_refuses_naming_the_paths() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        r#"
case "$3" in
  rev-parse)
    [ "$4" = "--show-toplevel" ] && { echo "/repo"; exit 0; }
    ;;
  status)
    printf '## main...origin/main\n M app/src/foo.php\n?? app/new-file.txt\n'
    exit 0
    ;;
esac
exit 1
"#,
    ));

    let error = git.ensure_pushed(Path::new("/repo/app"), None).unwrap_err();
    match &error {
        GitError::Dirty { paths } => {
            assert_eq!(paths, &["app/src/foo.php", "app/new-file.txt"]);
        }
        other => panic!("expected Dirty, got {other}"),
    }
    assert!(error.to_string().contains("app/src/foo.php"));
}

#[test]
fn no_upstream_refuses_naming_the_push_command() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
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

    let error = git.ensure_pushed(Path::new("/repo"), None).unwrap_err();
    match &error {
        GitError::NoUpstream { branch } => assert_eq!(branch, "main"),
        other => panic!("expected NoUpstream, got {other}"),
    }
    assert!(error.to_string().contains("git push -u origin main"));
}

#[test]
fn ahead_of_upstream_refuses_naming_git_push() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        r#"
case "$3" in
  rev-parse)
    [ "$4" = "--show-toplevel" ] && { echo "/repo"; exit 0; }
    ;;
  status)
    printf '## main...origin/main [ahead 2]\n'
    exit 0
    ;;
esac
exit 1
"#,
    ));

    let error = git.ensure_pushed(Path::new("/repo"), None).unwrap_err();
    match &error {
        GitError::Ahead { branch, ahead } => {
            assert_eq!(branch, "main");
            assert_eq!(*ahead, 2);
        }
        other => panic!("expected Ahead, got {other}"),
    }
    assert!(error.to_string().contains("git push"));
}

#[test]
fn behind_only_is_accepted() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        r#"
case "$3" in
  rev-parse)
    case "$4" in
      --show-toplevel) echo "/repo"; exit 0 ;;
      HEAD) echo "abc123"; exit 0 ;;
    esac
    ;;
  status)
    printf '## main...origin/main [behind 3]\n'
    exit 0
    ;;
  remote)
    [ "$4" = "get-url" ] && { echo "https://github.com/ArnaudDelgerie/TFSAppDemo"; exit 0; }
    ;;
esac
exit 1
"#,
    ));

    let commit = git
        .ensure_pushed(Path::new("/repo"), None)
        .expect("being behind alone must not refuse");
    assert_eq!(commit.repo.as_deref(), Some("ArnaudDelgerie/TFSAppDemo"));
}

#[test]
fn a_clean_pushed_tree_resolves_the_repo_from_an_https_remote() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        &clean_and_pushed_script("https://github.com/ArnaudDelgerie/TFSAppDemo"),
    ));

    let commit = git
        .ensure_pushed(Path::new("/repo/app"), None)
        .expect("a clean, pushed tree must pass every gate");
    assert_eq!(commit.branch, "main");
    assert_eq!(commit.sha, "abc123deadbeef");
    assert_eq!(commit.repo.as_deref(), Some("ArnaudDelgerie/TFSAppDemo"));
}

#[test]
fn a_clean_pushed_tree_resolves_the_repo_from_an_ssh_remote() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        &clean_and_pushed_script("git@github.com:ArnaudDelgerie/TFSAppDemo.git"),
    ));

    let commit = git
        .ensure_pushed(Path::new("/repo/app"), None)
        .expect("the scp-like remote spelling must resolve just as well");
    assert_eq!(commit.repo.as_deref(), Some("ArnaudDelgerie/TFSAppDemo"));
}

#[test]
fn an_explicit_repo_wins_and_the_remote_is_never_read() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        &clean_and_pushed_script("https://github.com/ArnaudDelgerie/TFSAppDemo"),
    ));

    let commit = git
        .ensure_pushed(Path::new("/repo/app"), Some("someone-else/other-repo"))
        .expect("an explicit --repo needs no remote lookup at all");
    assert_eq!(commit.repo.as_deref(), Some("someone-else/other-repo"));
    assert!(
        !argv_log(scripts.path()).contains("remote"),
        "the remote must never be read once --repo is given"
    );
}

#[test]
fn an_unrecognised_remote_url_refuses_naming_repo_flag() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        &clean_and_pushed_script("git@gitlab.com:owner/repo.git"),
    ));

    let error = git.ensure_pushed(Path::new("/repo/app"), None).unwrap_err();
    match &error {
        GitError::UnrecognisedRemote { remote, url } => {
            assert_eq!(remote, "origin");
            assert_eq!(url, "git@gitlab.com:owner/repo.git");
        }
        other => panic!("expected UnrecognisedRemote, got {other}"),
    }
    assert!(error.to_string().contains("--repo"));
}

#[test]
fn parse_status_reads_the_branch_header_and_the_dirty_paths() {
    let status = parse_status("## main...origin/main [ahead 1, behind 2]\n M a.php\n?? b.php\n");
    assert_eq!(status.branch, "main");
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!(status.ahead, 1);
    assert_eq!(status.behind, 2);
    assert_eq!(status.dirty_paths, vec!["a.php", "b.php"]);
}

#[test]
fn parse_branch_line_reads_a_branch_with_no_upstream() {
    let (branch, upstream, ahead, behind) = parse_branch_line("## main");
    assert_eq!(branch, "main");
    assert_eq!(upstream, None);
    assert_eq!((ahead, behind), (0, 0));
}

#[test]
fn parse_ls_files_keeps_nul_delimited_paths_verbatim() {
    assert!(parse_ls_files(b"").is_empty());
    assert_eq!(
        parse_ls_files(b"src/main.php\0"),
        vec![PathBuf::from("src/main.php")]
    );
    assert_eq!(
        parse_ls_files(b"a file.php\0a\nnewline.php\0"),
        vec![PathBuf::from("a file.php"), PathBuf::from("a\nnewline.php")]
    );
}

#[test]
fn ls_files_uses_nul_delimiters_and_returns_relative_paths() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        r#"
case "$3" in
  ls-files)
    [ "$4" = "-z" ] && { printf 'src/main.php\0a file.php\0'; exit 0; }
    ;;
esac
exit 1
"#,
    ));

    assert_eq!(
        git.ls_files(Path::new("/repo/app"))
            .expect("a tracked file list"),
        vec![PathBuf::from("src/main.php"), PathBuf::from("a file.php")]
    );
    assert!(argv_log(scripts.path()).contains("-C /repo/app ls-files -z"));
}

#[test]
fn parse_ls_tree_keeps_paths_modes_and_object_order_under_a_nested_prefix() {
    let sha_a = b"0123456789012345678901234567890123456789";
    let sha_b = b"abcdefabcdefabcdefabcdefabcdefabcdefabcd";
    let mut output = Vec::new();
    output.extend_from_slice(b"100755 blob ");
    output.extend_from_slice(sha_a);
    output.extend_from_slice(b"\tapps/demo/bin/run\0");
    output.extend_from_slice(b"120000 blob ");
    output.extend_from_slice(sha_b);
    output.extend_from_slice(b"\tapps/demo/a\nlink\0");

    let entries = parse_ls_tree(&output, Path::new("apps/demo/")).expect("a valid pinned tree");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, PathBuf::from("bin/run"));
    assert_eq!(entries[0].mode, 0o100755);
    assert!(entries[0].is_executable());
    assert_eq!(entries[1].path, PathBuf::from("a\nlink"));
    assert!(entries[1].is_symlink());
    assert_eq!(
        entries[1].object_id,
        "abcdefabcdefabcdefabcdefabcdefabcdefabcd"
    );
}

#[test]
fn parse_ls_tree_refuses_malformed_records_and_gitlinks() {
    let malformed = parse_ls_tree(b"100644 blob no-tab\0", Path::new(""))
        .expect_err("a tab separates the path");
    assert!(matches!(malformed, GitError::MalformedTree { .. }));

    let gitlink = parse_ls_tree(
        b"160000 commit 0123456789012345678901234567890123456789\tvendor/dependency\0",
        Path::new(""),
    )
    .expect_err("submodules have no V0.1 archive representation");
    assert!(matches!(gitlink, GitError::UnsupportedTreeEntry { .. }));
}

#[test]
fn snapshot_pins_the_upstream_sha_and_discovers_a_nested_project_prefix() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        r#"
case "$3" in
  rev-parse)
    case "$4" in
      --show-toplevel) echo "/repo" ;;
      --show-prefix) printf 'apps/demo/\n' ;;
      HEAD) echo "abc123deadbeef" ;;
    esac
    exit 0 ;;
  status) printf '## main...origin/main\n'; exit 0 ;;
  merge-base) exit 0 ;;
  remote) echo 'https://github.com/ArnaudDelgerie/TFSAppDemo'; exit 0 ;;
esac
exit 1
"#,
    ));

    let snapshot = git
        .snapshot(Path::new("/repo/apps/demo"), None)
        .expect("a clean upstream commit");
    assert_eq!(snapshot.commit.sha, "abc123deadbeef");
    assert_eq!(snapshot.project_prefix, PathBuf::from("apps/demo/"));
    assert!(
        argv_log(scripts.path()).contains("merge-base --is-ancestor abc123deadbeef origin/main")
    );
}

#[test]
fn one_batch_child_streams_each_blob_and_refuses_missing_or_malformed_responses() {
    let scripts = tempfile::tempdir().expect("a temp dir");
    let git = Git::at(write_fake_git(
        scripts.path(),
        r#"
case "$3" in
  cat-file)
    while IFS= read -r object; do
      case "$object" in
        good) printf 'good blob 4\n\377\000ok\n' ;;
        missing) printf 'missing missing\n' ;;
        bad) printf 'bad tree 1\nx\n' ;;
      esac
    done
    exit 0 ;;
esac
exit 1
"#,
    ));
    let mut reader = git.blob_reader(Path::new("/repo")).expect("batch child");
    let mut bytes = Vec::new();
    reader.copy_blob("good", &mut bytes).expect("streamed blob");
    assert_eq!(bytes, b"\xff\0ok");
    assert!(matches!(
        reader.copy_blob("missing", &mut Vec::new()),
        Err(GitError::MissingObject { .. })
    ));
    assert!(matches!(
        reader.copy_blob("bad", &mut Vec::new()),
        Err(GitError::MalformedBatch { .. })
    ));
}

#[test]
fn tree_entries_enumerates_a_project_nested_inside_its_repository() {
    // A real repository, not a fake `git`: the pathspec under test is the
    // exact one a project below the repository root (Papermark's `app/`)
    // makes Git misread, and only real Git proves `:/` fixes it.
    let repository = tempfile::tempdir().expect("a temp repo dir");
    let project = repository.path().join("apps/demo");
    fs::create_dir_all(&project).expect("a nested project dir");
    fs::write(project.join("tfsapp.config.json"), "{}").expect("a manifest");
    // Git runs from the repository root here, so `apps/demo` stays a
    // subdirectory of one repository rather than a repository of its own.
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
                .current_dir(repository.path())
                .status()
                .expect("git to run")
                .success(),
            "git fixture setup must succeed"
        );
    }
    let git = Git::new();
    let snapshot = git
        .local_snapshot(&project)
        .expect("a clean local work tree");
    assert_eq!(snapshot.project_prefix, PathBuf::from("apps/demo/"));
    let entries = git
        .tree_entries(&project, &snapshot)
        .expect("the nested project's tree");
    let paths: Vec<&Path> = entries.iter().map(|entry| entry.path.as_path()).collect();
    assert!(
        paths.contains(&Path::new("tfsapp.config.json")),
        "the project's own tree, project-relative: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with("apps/")),
        "no path outside the project's subtree: {paths:?}"
    );
}
