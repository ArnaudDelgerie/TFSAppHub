use super::*;

/// Spawn a long-lived controlled child (`sleep 30`) carrying
/// `TFS_APP_IDENTIFIER=<value>` in its environment, for tests that need a
/// real `/proc/<pid>/environ` to read. The caller owns the returned `Child`
/// and must kill/wait it (`kill_and_wait` below) even on assertion failure,
/// so a failed test never leaves a process behind.
///
/// Waits for `/proc/<pid>/environ` to actually be readable before returning:
/// right after `fork()`, before the child has `exec`'d `sh`, that file can be
/// briefly empty or unreadable, which otherwise makes this flaky under load.
fn spawn_child_with_identifier(value: &str) -> std::process::Child {
    let child = Command::new("sh")
        .args(["-c", "sleep 30"])
        .env("TFS_APP_IDENTIFIER", value)
        .spawn()
        .unwrap();
    wait_for_environ_readable(child.id());
    child
}

fn wait_for_environ_readable(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| !environ.is_empty()) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn kill_and_wait(mut child: std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// --- lock_path -------------------------------------------------------------

#[test]
fn lock_path_appends_lock_suffix() {
    assert_eq!(
        lock_path(Path::new("/data/sidecar.pid")),
        PathBuf::from("/data/sidecar.pid.lock")
    );
}

// --- is_owner_live -----------------------------------------------------------

#[test]
fn is_owner_live_false_when_lock_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    assert!(!is_owner_live(&pid_file).unwrap());
}

#[test]
fn is_owner_live_true_when_lock_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    let _held = try_lock_file(&lock_path(&pid_file)).unwrap().unwrap();
    assert!(is_owner_live(&pid_file).unwrap());
}

#[test]
fn is_owner_live_false_again_after_lock_released() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    {
        let _held = try_lock_file(&lock_path(&pid_file)).unwrap().unwrap();
        assert!(is_owner_live(&pid_file).unwrap());
    }
    assert!(!is_owner_live(&pid_file).unwrap());
}

// --- wait_for_lock_release ---------------------------------------------------

/// Spawn a controlled child that holds an exclusive flock on `path` until
/// killed — a genuinely separate process holding the lock, the same shape
/// `run <alias>` itself is (a different process than the one calling
/// `wait_for_lock_release`). Deliberately not `flock(1)` given a command
/// (`flock path sleep 30`): that form forks the sleep as a *child* of the
/// `flock` process, which inherits the same locked file description across
/// `fork` (no `O_CLOEXEC` on it) — killing only the `flock` parent then
/// leaves the lock held via the orphaned, still-running `sleep`. Instead,
/// `exec 9>"$1"` opens the lock path on this single shell process's own
/// fd 9, `flock -n 9` locks it (that temporary child exits once it has,
/// leaving the lock in effect via the shell's own fd 9 — the standard
/// shell lock-file idiom), and the final `exec sleep 30` replaces the shell
/// with `sleep` *in the same process*, fd 9 surviving the exec. One process,
/// one fd, for the entire lifetime — killing it releases the lock
/// immediately, with no descendant left holding a stray reference to it.
/// Blocks until the lock is observed held so the caller never races it.
fn spawn_lock_holder(path: &Path) -> std::process::Child {
    let child = Command::new("sh")
        .args([
            "-c",
            r#"exec 9>"$1"; flock -n 9 || exit 1; exec sleep 30"#,
            "sh",
        ])
        .arg(path)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if try_lock_file(path).unwrap().is_none() {
            return child;
        }
        thread::sleep(Duration::from_millis(5));
    }
    child
}

#[test]
fn wait_for_lock_release_false_while_the_holder_is_alive() {
    let dir = tempfile::tempdir().unwrap();
    let lock_file = dir.path().join("run.lock");
    let child = spawn_lock_holder(&lock_file);

    assert!(!wait_for_lock_release(&lock_file, Duration::from_millis(300)).unwrap());

    kill_and_wait(child);
}

#[test]
fn wait_for_lock_release_true_once_the_holder_is_killed_and_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let lock_file = dir.path().join("run.lock");
    let mut child = spawn_lock_holder(&lock_file);
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(wait_for_lock_release(&lock_file, Duration::from_secs(2)).unwrap());
}

// --- signal_target -----------------------------------------------------------

#[test]
fn signal_target_is_the_group_when_pid_is_its_own_leader() {
    assert_eq!(signal_target(100, Some(100)), SignalTarget::Group(100));
}

#[test]
fn signal_target_falls_back_to_the_pid_when_it_is_not_a_group_leader() {
    // A pid file written before plan 061, or a child spawned outside
    // `set_own_process_group`: its pgid is some other process's (typically
    // an ancestor's), never its own.
    assert_eq!(signal_target(100, Some(1)), SignalTarget::Pid(100));
}

#[test]
fn signal_target_falls_back_to_the_pid_when_it_is_already_gone() {
    assert_eq!(signal_target(100, None), SignalTarget::Pid(100));
}

#[test]
fn signal_target_group_operand_is_the_negative_pid() {
    assert_eq!(SignalTarget::Group(100).operand(), "-100");
}

#[test]
fn signal_target_pid_operand_is_the_plain_pid() {
    assert_eq!(SignalTarget::Pid(100).operand(), "100");
}

// --- reap_decision -----------------------------------------------------------

#[test]
fn reap_decision_terminates_on_a_matching_live_pid() {
    assert!(matches!(reap_decision(true, true), ReapDecision::Terminate));
}

#[test]
fn reap_decision_refuses_silently_when_the_pid_is_already_gone() {
    assert!(matches!(
        reap_decision(false, false),
        ReapDecision::RefuseAlreadyGone
    ));
}

#[test]
fn reap_decision_refuses_loudly_on_a_live_identity_mismatch() {
    assert!(matches!(
        reap_decision(true, false),
        ReapDecision::RefuseIdentityMismatch
    ));
}

// --- process_environ_has_identifier -----------------------------------------

#[test]
fn process_environ_has_identifier_true_for_exact_match() {
    let child = spawn_child_with_identifier("test-identifier");
    assert!(process_environ_has_identifier(
        child.id(),
        "test-identifier"
    ));
    kill_and_wait(child);
}

#[test]
fn process_environ_has_identifier_false_for_wrong_value() {
    let child = spawn_child_with_identifier("other-identifier");
    assert!(!process_environ_has_identifier(
        child.id(),
        "test-identifier"
    ));
    kill_and_wait(child);
}

#[test]
fn process_environ_has_identifier_false_when_variable_absent() {
    let child = Command::new("sh").args(["-c", "sleep 30"]).spawn().unwrap();
    wait_for_environ_readable(child.id());
    assert!(!process_environ_has_identifier(
        child.id(),
        "test-identifier"
    ));
    kill_and_wait(child);
}

#[test]
fn process_environ_has_identifier_false_for_prefix_only_match() {
    let child = spawn_child_with_identifier("test-identifier-extra");
    assert!(!process_environ_has_identifier(
        child.id(),
        "test-identifier"
    ));
    kill_and_wait(child);
}

#[test]
fn process_environ_has_identifier_false_for_suffix_only_match() {
    let child = spawn_child_with_identifier("extra-test-identifier");
    assert!(!process_environ_has_identifier(
        child.id(),
        "test-identifier"
    ));
    kill_and_wait(child);
}

// --- terminate_if_identifier_matches ----------------------------------------

#[test]
fn terminate_if_identifier_matches_leaves_a_wrong_identifier_process_alive() {
    let child = spawn_child_with_identifier("wrong-identifier");
    let pid = child.id();

    terminate_if_identifier_matches(pid, "test-identifier");

    assert!(process_exists(pid));
    kill_and_wait(child);
}

#[test]
fn terminate_if_identifier_matches_terminates_and_a_reaper_reaps_a_matching_process() {
    // Two opposite constraints must both hold for this test to prove
    // anything: `process_exists` reads `/proc/<pid>`, which still exists for
    // an unreaped zombie, so without a reaper `terminate` would poll for the
    // full 3s and then SIGKILL a corpse regardless of whether the guard
    // worked; but `process_environ_has_identifier` returns false for a
    // zombie (its `environ` is no longer readable), so a reaper that won the
    // race before the check would make the guard wrongly refuse. Starting
    // the reaper thread here, blocked in `wait()` while the child is still
    // alive, satisfies both: the identity check below runs on a live
    // process, and reaping only happens once `terminate` actually signals it.
    let mut child = spawn_child_with_identifier("test-identifier");
    let pid = child.id();
    let reaper = thread::spawn(move || {
        let _ = child.wait();
    });

    terminate_if_identifier_matches(pid, "test-identifier");
    reaper.join().unwrap();

    assert!(!process_exists(pid));
}

// --- spawn_on_signal --------------------------------------------------------

#[test]
fn spawn_on_signal_runs_the_action_once_a_byte_arrives() {
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_fd, write_fd) = (fds[0], fds[1]);
    let (sender, receiver) = std::sync::mpsc::channel();

    spawn_on_signal(read_fd, move || {
        let _ = sender.send(());
    });

    // Nothing has been written yet: the thread is blocked in `read`, so the
    // action must not have run.
    assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());

    let byte: u8 = 1;
    assert_eq!(
        unsafe { libc::write(write_fd, &byte as *const u8 as *const libc::c_void, 1) },
        1
    );

    assert!(receiver.recv_timeout(Duration::from_secs(2)).is_ok());
    unsafe {
        libc::close(read_fd);
        libc::close(write_fd);
    }
}

#[test]
fn spawn_on_signal_reads_a_byte_written_before_the_thread_exists() {
    // The launcher installs the handler in `.setup()` and only then spawns
    // the reader thread, so a signal caught in between must not be lost —
    // the pipe holds it and `read` returns it immediately.
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_fd, write_fd) = (fds[0], fds[1]);
    let byte: u8 = 1;
    assert_eq!(
        unsafe { libc::write(write_fd, &byte as *const u8 as *const libc::c_void, 1) },
        1
    );

    let (sender, receiver) = std::sync::mpsc::channel();
    spawn_on_signal(read_fd, move || {
        let _ = sender.send(());
    });

    assert!(receiver.recv_timeout(Duration::from_secs(2)).is_ok());
    unsafe {
        libc::close(read_fd);
        libc::close(write_fd);
    }
}
