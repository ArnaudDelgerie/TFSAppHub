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

// --- cleanup_previous_sidecar ------------------------------------------------

#[test]
fn cleanup_previous_sidecar_zero_budget_is_the_old_non_blocking_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    let holder = spawn_lock_holder(&lock_path(&pid_file));

    let start = Instant::now();
    let lock = cleanup_previous_sidecar(&pid_file, "test-identifier", Duration::ZERO);

    assert!(lock.is_none());
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "must not have polled"
    );

    kill_and_wait(holder);
}

#[test]
fn cleanup_previous_sidecar_ignores_leftover_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    let temporary = dir.path().join("sidecar.pid.tmp");
    fs::write(&temporary, "999999999\n").unwrap();

    let lock = cleanup_previous_sidecar(&pid_file, "test-identifier", Duration::ZERO);

    assert!(lock.is_some());
    assert_eq!(fs::read_to_string(&temporary).unwrap(), "999999999\n");
    assert!(!pid_file.exists());
}

#[test]
fn cleanup_previous_sidecar_waits_for_a_lock_released_mid_flight_then_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");
    // A pid that cannot exist, so the reap silently refuses rather than
    // signalling anything — this test is about the wait and the pid-file
    // removal, not the reap decision itself, which `reap_decision` covers.
    fs::write(&pid_file, "999999999\n").unwrap();
    let mut holder = spawn_lock_holder(&lock_path(&pid_file));

    let waiting = {
        let pid_file = pid_file.clone();
        thread::spawn(move || {
            cleanup_previous_sidecar(&pid_file, "test-identifier", Duration::from_secs(2))
        })
    };
    thread::sleep(Duration::from_millis(150));
    assert!(!waiting.is_finished(), "it must still be waiting");
    holder.kill().unwrap();
    holder.wait().unwrap();

    let lock = waiting.join().expect("the waiter did not panic");

    assert!(lock.is_some());
    assert!(!pid_file.exists(), "a reaped pid file is removed");
}

#[test]
fn cleanup_previous_sidecar_reaps_a_stale_server_and_every_stale_worker() {
    // Plan 045 step 4's regression: a stale sidecar.pid holding a server and
    // three workers must get all four reaped by the next launch. Nothing
    // about this function changes for it — the per-line loop above already
    // covers N workers — and this test is what keeps that true.
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sidecar.pid");

    let server = spawn_child_with_identifier("test-identifier");
    let worker_a = spawn_child_with_identifier("test-identifier");
    let worker_b = spawn_child_with_identifier("test-identifier");
    let worker_c = spawn_child_with_identifier("test-identifier");
    let pids = [server.id(), worker_a.id(), worker_b.id(), worker_c.id()];

    fs::write(
        &pid_file,
        pids.iter()
            .map(|pid| format!("{pid}\n"))
            .collect::<String>(),
    )
    .unwrap();

    let reapers: Vec<_> = [server, worker_a, worker_b, worker_c]
        .into_iter()
        .map(|mut child| {
            thread::spawn(move || {
                let _ = child.wait();
            })
        })
        .collect();

    let lock = cleanup_previous_sidecar(&pid_file, "test-identifier", Duration::ZERO);
    assert!(lock.is_some());

    for reaper in reapers {
        reaper.join().unwrap();
    }

    for pid in pids {
        assert!(!process_exists(pid), "pid {pid} must have been reaped");
    }
    assert!(!pid_file.exists(), "a reaped pid file is removed");
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
    // The probe only observes: a lock file nothing ever wrote must not be
    // resurrected by the reading of it (plan 061) — no file, no holder, and
    // no file afterwards either.
    assert!(
        !lock_path(&pid_file).exists(),
        "is_owner_live must not create the lock file it probes"
    );
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

// --- lock_file_exclusive -----------------------------------------------------

#[test]
fn shared_locks_coexist_but_exclude_an_exclusive_holder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lifecycle.lock");

    let first = try_lock_file_shared(&path)
        .unwrap()
        .expect("first shared lock");
    let second = try_lock_file_shared(&path)
        .unwrap()
        .expect("second shared lock");
    assert!(try_lock_file(&path).unwrap().is_none());
    drop(first);
    assert!(try_lock_file(&path).unwrap().is_none());
    drop(second);

    let exclusive = try_lock_file(&path).unwrap().expect("exclusive lock");
    assert!(try_lock_file_shared(&path).unwrap().is_none());
    drop(exclusive);
    assert!(try_lock_file_shared(&path).unwrap().is_some());
}

#[test]
fn lock_file_exclusive_takes_a_free_lock_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.lock");

    let _held = lock_file_exclusive(&path).unwrap();

    assert!(try_lock_file(&path).unwrap().is_none());
}

#[test]
fn lock_file_exclusive_waits_for_the_holder_instead_of_failing() {
    // The difference from `try_lock_file` that makes it worth having: a second
    // writer arriving mid-write must queue, not give up. The holder is a
    // separate process, since flock is per open file description.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.lock");
    let holder = spawn_lock_holder(&path);

    let waiting = {
        let path = path.clone();
        thread::spawn(move || lock_file_exclusive(&path).map(|_| ()))
    };
    thread::sleep(Duration::from_millis(50));
    assert!(!waiting.is_finished(), "it must still be waiting");

    kill_and_wait(holder);

    waiting
        .join()
        .expect("the waiter did not panic")
        .expect("the lock is taken once the holder is gone");
}

// --- lock_file_exclusive_timeout ----------------------------------------------

#[test]
fn lock_file_exclusive_timeout_none_while_the_holder_is_alive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("serving.lock");
    let holder = spawn_lock_holder(&path);

    assert!(
        lock_file_exclusive_timeout(&path, Duration::from_millis(300))
            .unwrap()
            .is_none()
    );

    kill_and_wait(holder);
}

#[test]
fn lock_file_exclusive_timeout_returns_the_held_lock_once_the_holder_releases_mid_wait() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("serving.lock");
    let mut holder = spawn_lock_holder(&path);

    let waiting = {
        let path = path.clone();
        thread::spawn(move || lock_file_exclusive_timeout(&path, Duration::from_secs(2)))
    };
    thread::sleep(Duration::from_millis(150));
    assert!(!waiting.is_finished(), "it must still be waiting");
    holder.kill().unwrap();
    holder.wait().unwrap();

    let held = waiting
        .join()
        .expect("the waiter did not panic")
        .expect("no I/O error")
        .expect("the lock is taken once the holder is gone");
    // Genuinely excludes a third acquisition: the file that comes back is
    // still the one holding the flock, not a dropped-and-forgotten handle.
    assert!(try_lock_file(&path).unwrap().is_none());
    drop(held);
}

#[test]
fn lock_file_exclusive_timeout_zero_budget_behaves_like_try_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("serving.lock");
    let holder = spawn_lock_holder(&path);

    assert!(lock_file_exclusive_timeout(&path, Duration::ZERO)
        .unwrap()
        .is_none());

    kill_and_wait(holder);

    assert!(lock_file_exclusive_timeout(&path, Duration::ZERO)
        .unwrap()
        .is_some());
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

#[test]
fn wait_for_lock_release_treats_a_missing_entry_as_released_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // No holder ever existed, so no file does either: released, and the wait
    // must not write the entry it was only ever observing (plan 061).
    let lock_file = dir.path().join("run.lock");

    assert!(wait_for_lock_release(&lock_file, Duration::from_millis(100)).unwrap());
    assert!(
        !lock_file.exists(),
        "the wait must not recreate a lock file that was never there"
    );
}

#[test]
fn wait_for_lock_release_leaves_no_empty_file_behind_when_unlinked_mid_wait() {
    let dir = tempfile::tempdir().unwrap();
    let lock_file = dir.path().join("run.lock");
    let mut child = spawn_lock_holder(&lock_file);

    // What a launcher's teardown does while `run --stop` is waiting on the
    // lock: unlink the entry, then release the lock by dying. The old
    // `O_CREAT` probe recreated the file empty at exactly this point and
    // left it littering `runs/` (plan 061's e2e observation).
    fs::remove_file(&lock_file).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(wait_for_lock_release(&lock_file, Duration::from_secs(2)).unwrap());
    assert!(
        !lock_file.exists(),
        "the wait must not resurrect the entry its launcher unlinked"
    );
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
    // The reaper is here for one reason, and since plan 014 only one:
    // `process_environ_has_identifier` returns false for a zombie (its
    // `environ` is no longer readable), so a reaper that won the race before
    // the guard's own check would make it wrongly refuse. Starting the thread
    // here, blocked in `wait()` while the child is still alive, means reaping
    // can only happen once `terminate` has actually signalled it — and it
    // leaves no corpse behind for the rest of the suite.
    //
    // It is **not** needed to keep `terminate` from escalating any more: the
    // poll no longer counts an unreaped zombie as a live process. That was
    // the second constraint this comment used to name, and it was a defect
    // being worked around rather than a property of the test.
    let mut child = spawn_child_with_identifier("test-identifier");
    let pid = child.id();
    let reaper = thread::spawn(move || {
        let _ = child.wait();
    });

    terminate_if_identifier_matches(pid, "test-identifier");
    reaper.join().unwrap();

    assert!(!process_exists(pid));
}

// --- process_exists ----------------------------------------------------------

#[test]
fn process_exists_true_for_a_live_process() {
    let child = spawn_child_with_identifier("test-identifier");
    assert!(process_exists(child.id()));
    kill_and_wait(child);
}

#[test]
fn process_exists_false_for_an_unreaped_zombie() {
    // The whole of plan 014's cause 1: the child is dead, but this process
    // has not `wait`ed on it, so `/proc/<pid>` is still there — and a corpse
    // is not something a caller polling this can wait for or kill again.
    let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
    let pid = child.id();
    wait_for_state(pid, 'Z');

    assert!(!process_exists(pid));

    child.wait().unwrap();
}

#[test]
fn process_exists_false_for_a_pid_that_never_existed() {
    assert!(!process_exists(999_999_999));
}

#[test]
fn stat_state_reads_past_a_command_name_containing_spaces_and_parentheses() {
    // The classic misparse: splitting on whitespace picks a fragment of the
    // executable name instead of the state.
    assert_eq!(stat_state("42 (sh (2) x) Z 1 42 0").unwrap(), 'Z');
    assert_eq!(stat_state("42 (frankenphp) S 1 42 0").unwrap(), 'S');
    assert_eq!(stat_live_group("42 (sh (2) x) S 1 7 0"), Some(7));
    assert_eq!(stat_live_group("42 (sh (2) x) Z 1 7 0"), None);
}

/// Block until `/proc/<pid>/stat` reports `state`, so a test never races the
/// kernel's own bookkeeping. Gives up quietly after a second — the assertion
/// that follows is what reports the failure, with a better message than a
/// panic in here would.
fn wait_for_state(pid: u32, state: char) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if process_state(pid) == Some(state) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

// --- terminate ---------------------------------------------------------------

/// Spawn a process-group leader (as every sidecar is, via
/// [`set_own_process_group`]) running `script`, and wait until it is really
/// its own group's leader before handing it back.
fn spawn_group_leader(script: &str) -> std::process::Child {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    set_own_process_group(&mut command);
    let child = command.spawn().unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if process_group_id(child.id()) == Some(child.id()) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    child
}

#[test]
fn terminate_returns_at_once_when_the_group_obeys_the_sigterm() {
    // Plan 014, cause 1, as the teardown itself meets it: nothing reaps the
    // child while `terminate` runs — the caller's own `child.wait()` comes
    // after — so the poll is looking at a zombie for the whole of its budget.
    // It must still answer "gone", and in milliseconds.
    let mut child = spawn_group_leader("sleep 30");
    let pid = child.id();

    let start = Instant::now();
    terminate(pid);
    let elapsed = start.elapsed();

    // Well under the 3s budget, which is also what proves no escalation line
    // was printed: that line is only ever reached after the deadline.
    assert!(
        elapsed < Duration::from_secs(1),
        "terminate polled for {elapsed:?} on a process that obeyed at once"
    );
    child.wait().unwrap();
}

#[test]
fn terminate_still_escalates_for_a_group_member_outliving_a_corpse_leader() {
    // The regression the narrowed predicate could have hidden: the leader is
    // a corpse within milliseconds, but a member of its group — a FrankenPHP
    // worker, here a `sleep` that ignores SIGTERM exactly as one that never
    // finishes draining does — is still running, and the group SIGKILL is the
    // only thing that gets it.
    let mut child = spawn_group_leader(r#"trap "" TERM; sleep 30 & exit 0"#);
    let leader = child.id();
    wait_for_state(leader, 'Z');
    assert!(
        group_has_live_process(leader),
        "the member must still be running while its leader is already a corpse"
    );

    let start = Instant::now();
    terminate(leader);

    assert!(
        start.elapsed() >= Duration::from_secs(3),
        "it must have spent the whole budget before escalating"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while group_has_live_process(leader) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !group_has_live_process(leader),
        "the group SIGKILL must have reached the member"
    );
    child.wait().unwrap();
}

// --- reap_decision on a corpse -----------------------------------------------

#[test]
fn reap_decision_refuses_silently_rather_than_loudly_for_a_zombie_in_a_pid_file() {
    // The second face of cause 1 (plan 014): `/proc/<zombie>/environ` is
    // `EACCES`, so the identity check answers false — which used to combine
    // with a `process_exists` that answered true into
    // `RefuseIdentityMismatch`, printing "likely reused by an unrelated
    // process" about a pid nothing had recycled.
    let mut child = spawn_child_with_identifier("test-identifier");
    let pid = child.id();
    child.kill().unwrap();
    wait_for_state(pid, 'Z');

    let decision = reap_decision(
        process_exists(pid),
        process_environ_has_identifier(pid, "test-identifier"),
    );

    assert!(matches!(decision, ReapDecision::RefuseAlreadyGone));
    child.wait().unwrap();
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
