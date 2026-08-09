use std::os::unix::fs::PermissionsExt;

use super::*;

fn write_executable(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Mirrors `build.rs`'s `ensure_resource_stub`: an empty, non-executable file
/// that exists only so a fresh clone compiles before `make resources` has run.
fn write_stub(path: &Path) {
    fs::write(path, []).unwrap();
}

// --- resolve_frankenphp_binary ----------------------------------------------

#[test]
fn the_first_executable_candidate_wins() {
    let dir = tempfile::tempdir().unwrap();
    let packaged = dir.path().join("packaged");
    let dev = dir.path().join("dev");
    write_executable(&packaged, b"packaged");
    write_executable(&dev, b"dev");

    let resolved = resolve_frankenphp_binary(&[packaged.clone(), dev]).unwrap();
    assert_eq!(resolved, packaged);
}

#[test]
fn a_missing_first_candidate_falls_through_to_the_second() {
    let dir = tempfile::tempdir().unwrap();
    let packaged = dir.path().join("packaged"); // never written
    let dev = dir.path().join("dev");
    write_executable(&dev, b"dev");

    let resolved = resolve_frankenphp_binary(&[packaged, dev.clone()]).unwrap();
    assert_eq!(resolved, dev);
}

#[test]
fn a_zero_byte_stub_is_skipped_even_though_it_exists() {
    // The rule the whole ordered resolution rests on: a build.rs stub must
    // never win over a real download, packaged or dev.
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("packaged");
    let dev = dir.path().join("dev");
    write_stub(&stub);
    write_executable(&dev, b"dev");

    let resolved = resolve_frankenphp_binary(&[stub, dev.clone()]).unwrap();
    assert_eq!(resolved, dev);
}

#[test]
fn a_present_but_non_executable_candidate_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let not_executable = dir.path().join("packaged");
    let dev = dir.path().join("dev");
    fs::write(&not_executable, b"not executable").unwrap(); // default perms, no +x
    write_executable(&dev, b"dev");

    let resolved = resolve_frankenphp_binary(&[not_executable, dev.clone()]).unwrap();
    assert_eq!(resolved, dev);
}

#[test]
fn no_usable_candidate_falls_back_to_the_system_install_or_names_every_path_tried() {
    let dir = tempfile::tempdir().unwrap();
    let packaged = dir.path().join("packaged");
    let dev = dir.path().join("dev");

    let result = resolve_frankenphp_binary(&[packaged.clone(), dev.clone()]);

    // /usr/bin/frankenphp is this function's one untestable hardcoded
    // dependency — whichever way this machine happens to answer, the
    // resolution has to be consistent with it.
    let system_wide = Path::new("/usr/bin/frankenphp");
    if system_wide.is_file() {
        assert_eq!(result.unwrap(), system_wide);
    } else {
        let error = result.unwrap_err().to_string();
        assert!(error.contains(&packaged.display().to_string()), "{error}");
        assert!(error.contains(&dev.display().to_string()), "{error}");
        assert!(error.contains("/usr/bin/frankenphp"), "{error}");
    }
}

// --- is_present --------------------------------------------------------

#[test]
fn is_present_is_false_for_a_zero_byte_file() {
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("stub");
    write_stub(&stub);
    assert!(!is_present(&stub));
}

#[test]
fn is_present_is_true_for_a_non_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert!(is_present(&file));
}

#[test]
fn is_present_is_false_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!is_present(&dir.path().join("missing")));
}
