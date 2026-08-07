use std::{
    fs::{self, File, OpenOptions},
    os::unix::{
        io::{AsRawFd, RawFd},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Reap a stale FrankenPHP sidecar left by a previous crash, identified by
/// the pid persisted under `pid_file`, and take over its liveness lock for
/// this launch. Returns the held lock on success; `None` means a live
/// sibling instance already holds it, in which case `pid_file` is left
/// untouched — nothing is signalled. Called exactly once per launch: from
/// `main()` pre-`Builder` for packaged mode (the lock is then threaded
/// through `.setup()` into `Sidecar`), or from `start_dev_sidecar` for dev.
///
/// Liveness is proven with a non-blocking exclusive flock on
/// `<pid_file>.lock`, held by the launcher process for its whole lifetime
/// (see `Sidecar`) and released automatically by the OS if it dies or
/// crashes without running `Sidecar::stop`. Acquiring the lock here means
/// any previous launcher is confirmed dead, so a pid found in `pid_file` is
/// by definition orphaned. Even then, a pid is only ever signalled if its
/// `/proc/<pid>/environ` still carries `identifier` — closing the window
/// where the OS has recycled that pid onto another project's live
/// FrankenPHP.
pub fn cleanup_previous_sidecar(pid_file: &Path, identifier: &str) -> Option<File> {
    let lock = try_lock_file(&lock_path(pid_file)).ok().flatten()?;

    if let Ok(contents) = fs::read_to_string(pid_file) {
        for line in contents.lines() {
            let Ok(pid) = line.trim().parse::<u32>() else {
                continue;
            };
            match reap_decision(
                process_exists(pid),
                process_environ_has_identifier(pid, identifier),
            ) {
                ReapDecision::Terminate => {
                    println!("Stopping stale sidecar pid {pid}");
                    terminate(pid);
                }
                ReapDecision::RefuseAlreadyGone => {}
                ReapDecision::RefuseIdentityMismatch => {
                    println!(
                        "Refusing to signal pid {pid} from {}: it no longer carries this app's \
                         identifier (likely reused by an unrelated process)",
                        pid_file.display()
                    );
                }
            }
        }
    }
    let _ = fs::remove_file(pid_file);
    Some(lock)
}

/// The reap decision for a single pid recorded in a stale pid file
/// (`cleanup_previous_sidecar` above, plan 061 — extending plan 052's
/// identity proof from the two `run` termination edges to this crash-reap
/// path): signal it only when it is both still alive and still carries this
/// app's exact identifier. `RefuseAlreadyGone` (silent — the pid simply
/// exited before this launch got around to reaping it, nothing to log) and
/// `RefuseIdentityMismatch` (logged — a live process failed the identity
/// check, exactly the reused-pid case this guard exists to catch) are kept
/// distinct so a caller can log only the second. Pure so the three states
/// are unit-testable without a real process.
pub enum ReapDecision {
    Terminate,
    RefuseAlreadyGone,
    RefuseIdentityMismatch,
}

pub fn reap_decision(pid_exists: bool, identity_matches: bool) -> ReapDecision {
    match (pid_exists, identity_matches) {
        (_, true) => ReapDecision::Terminate,
        (false, false) => ReapDecision::RefuseAlreadyGone,
        (true, false) => ReapDecision::RefuseIdentityMismatch,
    }
}

/// Put `command`'s future child in its own process group at spawn time —
/// `setpgid(0, 0)`, called between `fork` and `exec` via `pre_exec` — so its
/// whole descendant tree (a `run` alias's own children, FrankenPHP's PHP
/// workers, a Messenger worker's own spawns) can be reached by signalling the
/// group instead of only the one recorded pid (plan 061). Safe to run in that
/// narrow window: `setpgid` is on the async-signal-safe list and touches only
/// this not-yet-`exec`'d process's own kernel process-group membership — it
/// neither allocates nor locks, the two things `pre_exec`'s own documentation
/// warns are unsound between `fork` and `exec`. A group leader's PGID equals
/// its PID, so every pid file this app already writes keeps meaning what it
/// always meant and doubles as the group id `terminate` below signals.
pub fn set_own_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

/// What `terminate` below should signal: the whole process group when `pid`
/// is its own group leader — the case for every child spawned through
/// `set_own_process_group` above, since a leader's PGID always equals its
/// PID — or just the single pid as a fallback for a pid file written before
/// plan 061, or a child spawned outside that helper. `kill(1)`'s own
/// negative-pid convention (`kill -- -N` targets the group `N`, `kill -- N`
/// targets the single process `N`) is what `operand` below renders.
#[derive(Debug, PartialEq, Eq)]
pub enum SignalTarget {
    Group(u32),
    Pid(u32),
}

impl SignalTarget {
    /// The positional argument `terminate` passes to `kill` after `--`, so a
    /// pid that happens to look like an option (there is none here, but a
    /// negative group id would otherwise be mistaken for one) is always
    /// parsed as the target rather than a flag.
    fn operand(&self) -> String {
        match self {
            SignalTarget::Group(pid) => format!("-{pid}"),
            SignalTarget::Pid(pid) => pid.to_string(),
        }
    }
}

/// Pure decision behind `SignalTarget` above, factored out so the fallback
/// is unit-testable without a real process. `pgid` is `None` when `pid` is
/// already gone (`getpgid` failing is itself proof of that) — there is
/// nothing to signal as a group either, so that also falls back to `Pid`.
pub fn signal_target(pid: u32, pgid: Option<u32>) -> SignalTarget {
    match pgid {
        Some(pgid) if pgid == pid => SignalTarget::Group(pid),
        _ => SignalTarget::Pid(pid),
    }
}

/// `pid`'s current process group id, or `None` if `pid` no longer exists.
fn process_group_id(pid: u32) -> Option<u32> {
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    if pgid < 0 {
        None
    } else {
        Some(pgid as u32)
    }
}

/// Send SIGTERM to `pid`'s process group, then escalate to SIGKILL if it
/// hasn't exited within 3s (plan 061: the group, not just `pid` itself, so a
/// FrankenPHP grandchild or a `run` alias's own child can't survive its
/// parent's teardown). Some orphaned FrankenPHP processes — observed running
/// from an AppImage's squashfs-mounted `/tmp/.mount_*` path — don't respond
/// to a plain SIGTERM at all, so a single `kill` is not enough to guarantee
/// the stale sidecar is actually gone. The target is decided once, up front,
/// and reused for both signals — the group either got the same members for
/// both, or `pid` itself already exited and the second signal is a no-op.
pub fn terminate(pid: u32) {
    let operand = signal_target(pid, process_group_id(pid)).operand();
    let _ = Command::new("kill").arg("--").arg(&operand).status();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
    if process_exists(pid) {
        println!("pid {pid} did not exit after SIGTERM, sending SIGKILL");
        let _ = Command::new("kill")
            .arg("-9")
            .arg("--")
            .arg(&operand)
            .status();
    }
}

pub fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// `<pid_file>.lock` — kept separate from `pid_file` itself so the lock's
/// lifetime never depends on the pid content being rewritten.
pub fn lock_path(pid_file: &Path) -> PathBuf {
    let mut name = pid_file.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    pid_file.with_file_name(name)
}

/// Try to take a non-blocking exclusive lock on `path` (created if missing).
/// `Ok(Some(file))` means the lock was acquired — hold `file` for as long as
/// the lock must be held, it releases on drop. `Ok(None)` means another live
/// process already holds it.
pub fn try_lock_file(path: &Path) -> std::io::Result<Option<File>> {
    // This handle only ever holds an flock; it never reads or writes the
    // file's bytes, so truncating on open is neither meaningful nor harmful.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(Some(file))
    } else {
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(None),
            _ => Err(error),
        }
    }
}

/// Poll `path`'s flock (plan 057, `run --stop`/`--replace`) on a short
/// interval until it is free or `timeout` elapses. `Ok(true)` means the lock
/// was observed free before the deadline — the momentary lock this function
/// itself takes to observe that is dropped immediately, exactly like
/// `is_owner_live` above, never retained. `Ok(false)` means the deadline
/// passed with the lock still held. Only an `Err` from `try_lock_file`
/// itself (not "still held") short-circuits the wait.
pub fn wait_for_lock_release(path: &Path, timeout: Duration) -> std::io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if try_lock_file(path)?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a live app-window owner currently holds the sidecar liveness
/// lock (`<pid_file>.lock`, "Sidecar liveness lock" CONTRACT.md §6) — a
/// **read-only** probe, unlike `cleanup_previous_sidecar` above: it never
/// reaps a stale pid or removes `pid_file`, since a `run` command (plan
/// 047) merely checking liveness must not have the side effects a genuine
/// launch's reap does. `Ok(true)` means a live owner holds the lock;
/// `Ok(false)` means the lock was free — no live owner — and the
/// momentarily-acquired lock is dropped immediately rather than held.
pub fn is_owner_live(pid_file: &Path) -> std::io::Result<bool> {
    match try_lock_file(&lock_path(pid_file))? {
        Some(_lock) => Ok(false),
        None => Ok(true),
    }
}

/// Terminate `pid` only if it still carries `identifier`'s exact
/// `TFS_APP_IDENTIFIER` marker at the moment of the call (plan 052). This is
/// the guard `spawn_signal_forwarder` and `spawn_coexistence_watchdog` need
/// before calling `terminate` below: both react on a detached thread, woken
/// up an unbounded time after `pid` was recorded, so by the time they fire
/// the original child may already have exited and been reaped, letting the
/// OS recycle its number onto an unrelated process. `cleanup_previous_sidecar`
/// above closes the identical race on its own call site with the same
/// primitive. Checking immediately before signalling is what keeps the proof
/// from going stale itself; `terminate` itself stays unchanged and ungated,
/// since callers that already hold a fresh, non-recycled pid (e.g. the reap
/// path right above) don't need this extra check.
pub fn terminate_if_identifier_matches(pid: u32, identifier: &str) {
    if process_environ_has_identifier(pid, identifier) {
        terminate(pid);
    }
}

/// Whether `/proc/<pid>/environ` carries `TFS_APP_IDENTIFIER=<identifier>`
/// exactly — the marker every sidecar is launched with (dev and packaged
/// alike), used to confirm a pid still designates *this project's*
/// FrankenPHP before it is signalled.
pub fn process_environ_has_identifier(pid: u32, identifier: &str) -> bool {
    let needle = format!("TFS_APP_IDENTIFIER={identifier}");
    fs::read(format!("/proc/{pid}/environ"))
        .map(|environ| {
            environ
                .split(|&byte| byte == 0)
                .any(|var| var == needle.as_bytes())
        })
        .unwrap_or(false)
}

/// The self-pipe's write end (plan 047, `run <alias>`'s foreground
/// execution): a raw fd, not a `File`, because it must be reachable from an
/// `extern "C"` signal handler, where only async-signal-safe calls
/// (`write()` among them) are sound to make. `-1` means "not installed yet".
static SIGNAL_PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// The actual signal handler: writes one byte to the self-pipe and returns
/// immediately. Deliberately does **nothing** else — allocating, locking, or
/// calling anything not on the short async-signal-safe list from inside a
/// signal handler is undefined behavior, which is exactly why the real
/// reaction (terminating the child) happens on an ordinary thread that
/// blocks reading the other end instead (`spawn_signal_forwarder`).
extern "C" fn write_signal_to_pipe(_signal: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte: u8 = 1;
        unsafe {
            libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

/// Install SIGINT/SIGTERM handlers for `run <alias>`'s own process (plan
/// 047, CONTRACT.md §2/§6) and return the self-pipe's read end.
///
/// `std::process::Child::wait()` silently retries on `EINTR`, so a signal
/// caught while a thread is blocked in `wait()` is invisible to that thread
/// — installing a handler alone would never let this process notice a
/// caught signal in order to forward it. The standard fix is the self-pipe
/// trick: the handler above only writes one byte (async-signal-safe), and a
/// *different* thread blocks reading the pipe (`spawn_signal_forwarder`) to
/// learn that a signal arrived and react to it, leaving the thread actually
/// calling `wait()` on the child alone.
pub fn install_signal_forwarding() -> std::io::Result<RawFd> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    SIGNAL_PIPE_WRITE_FD.store(write_fd, Ordering::SeqCst);
    unsafe {
        libc::signal(
            libc::SIGINT,
            write_signal_to_pipe as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            write_signal_to_pipe as *const () as libc::sighandler_t,
        );
    }
    Ok(read_fd)
}

/// Spawn the thread that blocks reading `install_signal_forwarding`'s pipe
/// and, the first time a byte arrives (a caught SIGINT/SIGTERM), forwards a
/// termination to `child_pid` via `terminate_if_identifier_matches` above —
/// the same SIGTERM-then-SIGKILL-after-3s primitive every other stop path in
/// this launcher uses, gated on `child_pid` still carrying `identifier`
/// (plan 052) since this thread only reacts an unbounded time after
/// `child_pid` was recorded, long enough for it to have been reaped and
/// recycled onto an unrelated process by the time the signal arrives.
pub fn spawn_signal_forwarder(read_fd: RawFd, child_pid: u32, identifier: String) {
    spawn_on_signal(read_fd, move || {
        terminate_if_identifier_matches(child_pid, &identifier);
    });
}

/// Run `action` on a detached thread the first time a byte arrives on
/// `install_signal_forwarding`'s pipe — i.e. the first caught SIGINT/SIGTERM.
/// The generic half of `spawn_signal_forwarder` above, shared with the
/// launcher's own orderly shutdown (`lifecycle::install_shutdown_on_signal`),
/// which reacts by stopping the sidecar rather than by forwarding to a child.
/// A byte written before this thread exists is not lost: it waits in the pipe
/// and the `read` returns it immediately.
pub fn spawn_on_signal<F: FnOnce() + Send + 'static>(read_fd: RawFd, action: F) {
    thread::spawn(move || {
        let mut byte = [0u8; 1];
        if unsafe { libc::read(read_fd, byte.as_mut_ptr() as *mut libc::c_void, 1) } > 0 {
            action();
        }
    });
}

/// Spawn the coexistence watchdog (rule 3, CONTRACT.md §2/§6, plan 047):
/// polls `is_owner_live(&pid_file)` once a second and, the moment the app
/// window's owner drops, terminates `child_pid` — only ever spawned when a
/// window was confirmed live at `run <alias>`'s own start (reachable only by
/// a `concurrent` alias, since a non-concurrent one already refused to start
/// in that case); a lone `run` command never gets one, since there is
/// nothing to watch. An I/O error probing liveness is treated the same as
/// "the owner is gone" rather than looping forever on an unreadable lock
/// file. The termination itself goes through `terminate_if_identifier_matches`
/// (plan 052), gated on `child_pid` still carrying `identifier`, since this
/// thread only reacts to a poll tick after the owner drops — long enough for
/// `child_pid` to have been reaped and recycled onto an unrelated process by
/// then.
pub fn spawn_coexistence_watchdog(pid_file: PathBuf, child_pid: u32, identifier: String) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(1));
        match is_owner_live(&pid_file) {
            Ok(true) => continue,
            Ok(false) | Err(_) => {
                terminate_if_identifier_matches(child_pid, &identifier);
                return;
            }
        }
    });
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
