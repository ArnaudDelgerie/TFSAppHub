use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use super::{ensure_current_at, resolve_running_image, Outcome};

fn write_source(dir: &std::path::Path, contents: &[u8]) -> PathBuf {
    let path = dir.join("source-hub");
    fs::write(&path, contents).expect("a source binary");
    path
}

#[test]
fn it_writes_the_copy_when_none_exists_yet() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let source = write_source(dir.path(), b"hub v1");
    let target = dir.path().join("bin/tfsapp-hub");

    let outcome = ensure_current_at(&source, &target).expect("the copy is written");

    assert_eq!(outcome, Outcome::Written);
    assert_eq!(fs::read(&target).expect("the copy exists"), b"hub v1");
}

#[test]
fn the_copy_is_executable() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let source = write_source(dir.path(), b"hub v1");
    let target = dir.path().join("bin/tfsapp-hub");

    ensure_current_at(&source, &target).expect("the copy is written");

    let mode = fs::metadata(&target).expect("it exists").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn a_stale_copy_is_refreshed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let source = write_source(dir.path(), b"hub v1");
    let target = dir.path().join("bin/tfsapp-hub");
    fs::create_dir_all(target.parent().unwrap()).expect("the bin dir");
    fs::write(&target, b"an older copy").expect("a stale copy already there");

    let outcome = ensure_current_at(&source, &target).expect("the copy is refreshed");

    assert_eq!(outcome, Outcome::Refreshed);
    assert_eq!(fs::read(&target).expect("the copy exists"), b"hub v1");
}

#[test]
fn a_current_copy_is_left_untouched() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let source = write_source(dir.path(), b"hub v1");
    let target = dir.path().join("bin/tfsapp-hub");

    ensure_current_at(&source, &target).expect("the first write");
    // A permission a real refresh would overwrite with 0755 — if the second
    // call is truly a no-op, this survives.
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).expect("loosened");

    let outcome = ensure_current_at(&source, &target).expect("the second call");

    assert_eq!(outcome, Outcome::Current);
    let mode = fs::metadata(&target).expect("it exists").permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "a no-op must not have rewritten the file");
}

#[test]
fn nothing_is_copied_when_the_source_already_is_the_target() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = write_source(dir.path(), b"the running hub itself");

    let outcome = ensure_current_at(&path, &path).expect("no copy is needed");

    assert_eq!(outcome, Outcome::WeAreIt);
}

#[test]
fn the_appimage_path_is_preferred_over_current_exe() {
    let appimage = PathBuf::from("/tmp/.mount_XXXX/usr/bin/tfsapp-hub-appimage-marker");

    let resolved = resolve_running_image(Some(appimage.clone()), || {
        panic!("current_exe must not be consulted when $APPIMAGE is set")
    })
    .expect("the appimage path resolves");

    assert_eq!(resolved, appimage);
}

#[test]
fn current_exe_is_the_fallback_with_no_appimage() {
    let fallback = PathBuf::from("/usr/local/bin/tfsapp-hub");

    let resolved = resolve_running_image(None, || Ok(fallback.clone()))
        .expect("the fallback resolves");

    assert_eq!(resolved, fallback);
}
