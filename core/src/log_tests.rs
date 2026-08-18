use super::*;

// --- append_stdio ------------------------------------------------------

#[test]
fn append_stdio_appends_without_truncating_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.log");
    fs::write(&path, "existing-line\n").unwrap();

    let (stdout, _stderr) = append_stdio(&path).unwrap();
    let status = std::process::Command::new("sh")
        .args(["-c", "printf 'new-line\\n'"])
        .stdout(stdout)
        .status()
        .unwrap();
    assert!(status.success());

    let contents = fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "existing-line\nnew-line\n");
}

#[test]
fn append_stdio_stdout_and_stderr_share_one_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.log");

    let (stdout, stderr) = append_stdio(&path).unwrap();
    let status = std::process::Command::new("sh")
        .args(["-c", "printf 'out-line\\n'; printf 'err-line\\n' >&2"])
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .unwrap();
    assert!(status.success());

    let contents = fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "out-line\nerr-line\n");
}

// --- sidecar_log_stdio -----------------------------------------------------

#[test]
fn sidecar_log_stdio_appends_without_truncating_existing_content() {
    let log_dir = tempfile::tempdir().unwrap();
    fs::write(log_dir.path().join("sidecar.log"), "existing-line\n").unwrap();

    let (stdout, _stderr) = sidecar_log_stdio(log_dir.path()).unwrap();
    let status = std::process::Command::new("sh")
        .args(["-c", "printf 'new-line\\n'"])
        .stdout(stdout)
        .status()
        .unwrap();
    assert!(status.success());

    let contents = fs::read_to_string(log_dir.path().join("sidecar.log")).unwrap();
    assert_eq!(contents, "existing-line\nnew-line\n");
}

#[test]
fn sidecar_log_stdio_stdout_and_stderr_both_append_to_the_same_file() {
    let log_dir = tempfile::tempdir().unwrap();

    let (stdout, stderr) = sidecar_log_stdio(log_dir.path()).unwrap();
    let status = std::process::Command::new("sh")
        .args(["-c", "printf 'out-line\\n'; printf 'err-line\\n' >&2"])
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .unwrap();
    assert!(status.success());

    let contents = fs::read_to_string(log_dir.path().join("sidecar.log")).unwrap();
    assert!(contents.contains("out-line"));
    assert!(contents.contains("err-line"));
}

// --- rotate_log --------------------------------------------------------

#[test]
fn rotate_log_leaves_a_file_under_the_cap_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidecar.log");
    fs::write(&path, "short").unwrap();

    rotate_log(&path);

    assert_eq!(fs::read_to_string(&path).unwrap(), "short");
    assert!(!dir.path().join("sidecar.log.1").exists());
}

#[test]
fn rotate_log_rolls_a_file_at_the_cap_to_generation_1_and_frees_the_live_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidecar.log");
    fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();

    rotate_log(&path);

    assert!(!path.exists());
    let rotated = fs::metadata(dir.path().join("sidecar.log.1")).unwrap();
    assert_eq!(rotated.len(), MAX_LOG_BYTES);
}

#[test]
fn rotate_log_shifts_existing_generations_and_drops_the_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidecar.log");
    fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
    fs::write(dir.path().join("sidecar.log.1"), "gen1").unwrap();
    fs::write(dir.path().join("sidecar.log.2"), "gen2").unwrap();
    fs::write(dir.path().join("sidecar.log.3"), "gen3-oldest").unwrap();

    rotate_log(&path);

    assert!(!path.exists());
    assert_eq!(
        fs::metadata(dir.path().join("sidecar.log.1"))
            .unwrap()
            .len(),
        MAX_LOG_BYTES
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("sidecar.log.2")).unwrap(),
        "gen1"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("sidecar.log.3")).unwrap(),
        "gen2"
    );
}

#[test]
fn rotate_log_missing_file_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidecar.log");

    rotate_log(&path);

    assert!(!path.exists());
    assert!(!dir.path().join("sidecar.log.1").exists());
}

#[test]
fn rotate_log_does_not_panic_when_the_directory_is_unwritable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidecar.log");
    fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();

    let mut perms = fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o500);
    fs::set_permissions(dir.path(), perms.clone()).unwrap();

    rotate_log(&path);

    perms.set_mode(0o700);
    fs::set_permissions(dir.path(), perms).unwrap();
}

// --- rotate_logs ---------------------------------------------------------

#[test]
fn rotate_logs_rotates_both_commands_log_and_sidecar_log() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("commands.log"),
        vec![b'x'; MAX_LOG_BYTES as usize],
    )
    .unwrap();
    fs::write(
        dir.path().join("sidecar.log"),
        vec![b'x'; MAX_LOG_BYTES as usize],
    )
    .unwrap();

    rotate_logs(dir.path());

    assert!(!dir.path().join("commands.log").exists());
    assert!(!dir.path().join("sidecar.log").exists());
    assert!(dir.path().join("commands.log.1").exists());
    assert!(dir.path().join("sidecar.log.1").exists());
}

#[test]
fn rotate_logs_rotates_every_worker_log_present_orphans_included() {
    // worker-1.log and worker-2.log stand for two declared slots; worker-3.log
    // stands for a slot a manifest no longer declares (plan 046) — nothing at
    // this layer tells them apart, which is exactly the point: rotation finds
    // whatever `worker-<n>.log` files are actually in the directory.
    let dir = tempfile::tempdir().unwrap();
    for name in ["worker-1.log", "worker-2.log", "worker-3.log"] {
        fs::write(dir.path().join(name), vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
    }

    rotate_logs(dir.path());

    for name in ["worker-1.log", "worker-2.log", "worker-3.log"] {
        assert!(
            !dir.path().join(name).exists(),
            "{name} should have rotated"
        );
        assert!(
            dir.path().join(format!("{name}.1")).exists(),
            "{name}.1 should exist"
        );
    }
}

#[test]
fn rotate_logs_does_not_mistake_a_rotated_generation_for_a_live_worker_log() {
    // worker-1.log.1 is already a rotated generation, not a live log — its
    // name does not end in `.log`, so it must be left alone rather than
    // shifted again into worker-1.log.2.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("worker-1.log.1"),
        vec![b'x'; MAX_LOG_BYTES as usize],
    )
    .unwrap();

    rotate_logs(dir.path());

    assert!(dir.path().join("worker-1.log.1").exists());
    assert!(!dir.path().join("worker-1.log.2").exists());
}

#[test]
fn rotate_logs_does_not_panic_when_the_directory_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("worker-1.log"),
        vec![b'x'; MAX_LOG_BYTES as usize],
    )
    .unwrap();

    let mut perms = fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o300);
    fs::set_permissions(dir.path(), perms.clone()).unwrap();

    rotate_logs(dir.path());

    perms.set_mode(0o700);
    fs::set_permissions(dir.path(), perms).unwrap();
}
