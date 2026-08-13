//! `run <id> <alias>` (plan 013, CONTRACT.md's "`run <alias>` (plan 047)" and
//! §6's rules 1–3) — an installed app's declared `bin/console` command,
//! executed in the foreground with that app's own §3 environment.
//!
//! **A transplant, not a design.** The station's `desktop/src-tauri/src/run.rs`
//! (600 lines, its most delicate module) already implements every rule here,
//! and every primitive it stands on — `try_lock_file`, `wait_for_lock_release`,
//! `is_owner_live`, `terminate_if_identifier_matches`,
//! `install_signal_forwarding`, `spawn_signal_forwarder`,
//! `spawn_coexistence_watchdog`, `set_own_process_group` — already lives in
//! `tfsapp_core::process` (plan 002). What is genuinely new is one step in
//! front of it — resolving *which* app — and the fact that the hub has two
//! apps' worth of state to keep apart while doing it. `../TFSAppWorkstation/
//! .project/hub/003-cli-surface.md` §4 already answers why two different apps
//! running commands at once costs zero design work: `run.lock` is keyed on the
//! app's own `identifier`, not on the hub-local `id`, so it is already
//! per-app.
//!
//! **Rule 1's message changes owner.** The station tells the user to launch
//! the app first, because on the station only a launch writes
//! `data/config.json`. Here `install` stamps it at the install event's success
//! point, so a missing record means a data directory that was wiped or never
//! installed (an `open` will re-stamp it, since `lifecycle::prepare_launch`
//! treats a missing record the same way an install does), and an older record
//! points at `tfsapp-hub update <id>` — the same message the launch guard
//! uses for the same state.
//!
//! **This module holds the pure decisions only** — argv-independent, no
//! process, no filesystem beyond what a caller hands it as bytes. The
//! imperative flow (app resolution, the lock, the spawn, the signal
//! forwarding) is plan 013 step 2's addition.

use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

use crate::{
    lifecycle::{LifecycleDecisionError, LifecycleEvent},
    manifest::RunAlias,
};

/// Whether `data/config.json`'s recorded version proves the app layer is up
/// to date enough for a `run` command to touch it (rule 1, CONTRACT.md §2/§6)
/// — reuses `lifecycle::lifecycle_decision`, the exact same semver compare the
/// launch-time guard makes, rather than a second mechanism. Only an exact
/// match passes: a missing record (never installed, or a data dir an `open`
/// has not re-stamped yet), an older one (a pending install/update) or a
/// newer one (a downgrade) all refuse, each with a message pointing at the
/// hub's own way out.
pub enum VersionGate {
    UpToDate,
    Refuse(String),
}

/// `config_file` is only read for the `Downgrade`/`InvalidVersion` messages,
/// which name it as where to look; `id` is what every message's suggested
/// command is run against.
pub fn check_version_gate(
    id: &str,
    config_file: &Path,
    recorded: Option<&str>,
    current: &semver::Version,
) -> VersionGate {
    match crate::lifecycle::lifecycle_decision(recorded, current) {
        Ok(LifecycleEvent::None) => VersionGate::UpToDate,
        Ok(LifecycleEvent::Install) => VersionGate::Refuse(format!(
            "{id} has no recorded data version yet — open it once (`tfsapp-hub open {id}`) so \
             the hub can stamp it, then retry"
        )),
        Ok(LifecycleEvent::Update) => VersionGate::Refuse(format!(
            "{id}'s installed source is version {current}, but its data dir was last written by \
             version {} — its update never completed, so its migrations may not have run. Run \
             `tfsapp-hub update {id}` to finish it.",
            recorded.unwrap_or("?"),
        )),
        Err(LifecycleDecisionError::Downgrade { recorded, current }) => {
            VersionGate::Refuse(format!(
            "{id}'s data was written by app version {recorded}, but the installed app is version \
             {current} — downgrading is not automated; edit {} to resolve",
            config_file.display(),
        ))
        }
        Err(LifecycleDecisionError::InvalidVersion(error)) => VersionGate::Refuse(format!(
            "cannot read the version recorded in {}: {error}",
            config_file.display(),
        )),
    }
}

/// Format the declared aliases (CONTRACT.md §2), one line each, naming the
/// console command and whether it is `concurrent`. `BTreeMap` already
/// iterates in sorted order, so unlike the station's own `HashMap`-backed
/// version this needs no separate sort. Shared by the unknown-alias refusal
/// and by `run <id>`'s own listing form.
pub fn format_alias_list(aliases: &BTreeMap<String, RunAlias>) -> String {
    if aliases.is_empty() {
        return "no run aliases are declared by this app".to_string();
    }
    aliases
        .iter()
        .map(|(name, alias)| {
            format!(
                "  {name} -> {} ({})",
                alias.command,
                if alias.concurrent {
                    "concurrent"
                } else {
                    "standalone-only"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `run.lock`'s own parsed content: the alias name that holds the lock, and
/// the spawned child's pid once recorded. `pid` is `None` in the narrow
/// window between the lock being acquired and the post-spawn rewrite, never
/// an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLockRecord {
    pub alias: String,
    pub pid: Option<u32>,
}

/// Format `run.lock`'s content: `<alias>` alone when `pid` is `None` (the
/// lock-acquisition-time write, before the child exists), `<alias>\n<pid>`
/// once it does. Pure counterpart to [`parse_run_lock`] — round-trips through
/// it.
pub fn format_run_lock(alias: &str, pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("{alias}\n{pid}"),
        None => alias.to_string(),
    }
}

/// Parse `run.lock`'s content: `None` for empty content (nothing ever written
/// — should not happen while the lock is held, but this is the pure, total
/// counterpart callers can match on regardless). Otherwise the first line is
/// the alias; a present, numeric second line is the pid, and anything else
/// (absent, non-numeric, a stale trailing newline) parses as "no pid yet"
/// rather than an error — the record format is advisory, not a wire contract
/// with a hard failure mode.
pub fn parse_run_lock(contents: &str) -> Option<RunLockRecord> {
    let mut lines = contents.lines();
    let alias = lines.next()?.to_string();
    let pid = lines
        .next()
        .and_then(|line| line.trim().parse::<u32>().ok());
    Some(RunLockRecord { alias, pid })
}

/// Result of examining a free run lock record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanedRun {
    ActiveOrphan { alias: String, pid: u32 },
    Stale,
}

/// Pure policy for a run lock whose flock is already known to be free.
pub fn orphaned_run_decision(
    record: Option<&RunLockRecord>,
    pid_alive: bool,
    identifier_matches: bool,
) -> OrphanedRun {
    match record {
        Some(RunLockRecord {
            alias,
            pid: Some(pid),
        }) if pid_alive && identifier_matches => OrphanedRun::ActiveOrphan {
            alias: alias.clone(),
            pid: *pid,
        },
        _ => OrphanedRun::Stale,
    }
}

/// Probe a free run lock record for a child that outlived its launcher.
/// Callers must first establish that the flock is free, or hold it themselves.
pub fn probe_orphaned_run(run_lock_path: &Path, identifier: &str) -> OrphanedRun {
    let record = std::fs::read_to_string(run_lock_path)
        .ok()
        .and_then(|contents| parse_run_lock(&contents));
    let (pid_alive, identifier_matches) = match record.as_ref().and_then(|record| record.pid) {
        Some(pid) => (
            tfsapp_core::process::process_exists(pid),
            tfsapp_core::process::process_environ_has_identifier(pid, identifier),
        ),
        None => (false, false),
    };
    orphaned_run_decision(record.as_ref(), pid_alive, identifier_matches)
}

/// How long `stop_active_run` waits for `run.lock` to free after signalling
/// the recorded child — must outlast `terminate`'s own SIGTERM-then-3s-SIGKILL
/// escalation plus a little slack for the launcher to actually observe
/// `child.wait()` return and drop the lock.
const STOP_LOCK_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcome of `run --stop`/the stop half of `run --replace <id> <alias>`
/// — exactly the six states [`stop_active_run`] can end in. Kept as plain
/// data — no message text or exit-code logic inside the enum itself — so
/// [`stop_outcome_message`] below can be unit-tested directly against each
/// variant without any process I/O.
pub enum StopOutcome {
    /// `run.lock` was free: no `run` command is active for this app.
    NotRunning,
    /// A record with a pid was found, signalled, and the lock freed.
    Stopped { alias: String },
    /// A free lock record named a live child whose launcher had already gone;
    /// the child was terminated and observed gone.
    StoppedOrphan { alias: String },
    /// A free lock record named a live child whose launcher had already gone,
    /// but it remained live after the bounded stop wait.
    OrphanStillRunning { alias: String, pid: u32 },
    /// The lock is held but its record has no pid yet — the narrow window
    /// between lock acquisition and the post-spawn rewrite.
    PidUnknown { alias: String },
    /// A record with a pid was found and signalled, but the lock was still
    /// held once `STOP_LOCK_RELEASE_TIMEOUT` elapsed.
    LockHeld { alias: String, pid: u32 },
}

/// Render `outcome` as the message `run --stop`/`--replace` prints.
/// `run_lock_path` is only used by the `LockHeld` message, which names the
/// file the way every other lock-contention message in this module does.
pub fn stop_outcome_message(outcome: &StopOutcome, run_lock_path: &Path) -> String {
    match outcome {
        StopOutcome::NotRunning => "no run command is currently active".to_string(),
        StopOutcome::Stopped { alias } => format!("stopped the active run command \"{alias}\""),
        StopOutcome::StoppedOrphan { alias } => format!(
            "stopped the orphaned run command \"{alias}\" after its launcher had already gone"
        ),
        StopOutcome::OrphanStillRunning { alias, pid } => format!(
            "signalled the orphaned run command \"{alias}\" (pid {pid}) but it was still live after \
             waiting"
        ),
        StopOutcome::PidUnknown { alias } => format!(
            "the run command \"{alias}\" is active but has not yet recorded its child process — \
             retry in a moment"
        ),
        StopOutcome::LockHeld { alias, pid } => format!(
            "signalled the run command \"{alias}\" (pid {pid}) but {} was still held after \
             waiting — it may be wedged outside its own child process",
            run_lock_path.display()
        ),
    }
}

/// Whether `run.lock` is free once [`stop_active_run`] returns — `NotRunning`
/// and `Stopped` mean the app is launchable again; `PidUnknown` and
/// `OrphanStillRunning`, `PidUnknown`, and `LockHeld` mean it is not. Shared by `--stop`'s exit code and
/// `--replace`'s decision to abort rather than continue into rule 2.
pub fn stop_outcome_succeeded(outcome: &StopOutcome) -> bool {
    matches!(
        outcome,
        StopOutcome::NotRunning | StopOutcome::Stopped { .. } | StopOutcome::StoppedOrphan { .. }
    )
}

/// `run --stop`/the stop half of `run --replace <id> <alias>`'s whole
/// imperative flow: resolves whatever `run.lock` currently records,
/// terminates the recorded child through
/// `tfsapp_core::process::terminate_if_identifier_matches` (never a bare,
/// unguarded terminate — the identity proof against pid reuse), and waits for
/// the lock to actually free before reporting. Deliberately does **not** take
/// rule 1's version gate as an argument or call it itself: recovering an
/// installation whose app layer is stale is one of this command's own jobs.
pub fn stop_active_run(data_dir: &Path, identifier: &str) -> std::io::Result<StopOutcome> {
    let run_lock_path = data_dir.join("run.lock");
    // A free lock is momentarily reacquired here to observe it, then
    // immediately dropped. Its durable record may still name a child whose
    // launcher died, so it must be probed before calling the app idle.
    if tfsapp_core::process::try_lock_file(&run_lock_path)?.is_some() {
        return match probe_orphaned_run(&run_lock_path, identifier) {
            OrphanedRun::Stale => Ok(StopOutcome::NotRunning),
            OrphanedRun::ActiveOrphan { alias, pid } => {
                tfsapp_core::process::terminate_if_identifier_matches(pid, identifier);
                let deadline = Instant::now() + STOP_LOCK_RELEASE_TIMEOUT;
                while tfsapp_core::process::process_exists(pid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if tfsapp_core::process::process_exists(pid) {
                    Ok(StopOutcome::OrphanStillRunning { alias, pid })
                } else {
                    Ok(StopOutcome::StoppedOrphan { alias })
                }
            }
        };
    }

    let record = std::fs::read_to_string(&run_lock_path)
        .ok()
        .and_then(|contents| parse_run_lock(&contents));
    let Some(record) = record else {
        return Ok(StopOutcome::PidUnknown {
            alias: "an unknown alias".to_string(),
        });
    };
    let Some(pid) = record.pid else {
        return Ok(StopOutcome::PidUnknown {
            alias: record.alias,
        });
    };

    tfsapp_core::process::terminate_if_identifier_matches(pid, identifier);
    if tfsapp_core::process::wait_for_lock_release(&run_lock_path, STOP_LOCK_RELEASE_TIMEOUT)? {
        Ok(StopOutcome::Stopped {
            alias: record.alias,
        })
    } else {
        Ok(StopOutcome::LockHeld {
            alias: record.alias,
            pid,
        })
    }
}

// --- The imperative flow ----------------------------------------------------
//
// Everything above is pure; everything below touches the registry, the
// filesystem and a process. The app-resolution step in front of all three
// forms — `open::resolve` — is what plan 013's Overview means by "the hub has
// two apps' worth of state to keep apart": it already refuses `broken`, warns
// on `needs-revalidation` and proves the registry's identifier and the
// installed snapshot's agree, so none of that has to be re-derived here.

use std::io::{Seek, SeekFrom, Write};

use crate::{
    app_env,
    cli::{EXIT_FAILED, EXIT_OK},
    open,
    paths::Paths,
    php,
};

/// `run <id>` with no alias: list the app's declared aliases (the hub's own
/// addition to the station's grammar — see `cli::RunInvocation::List`). The
/// station discovers its aliases through `--help`, which the hub cannot do
/// since they belong to an app and not to the binary.
pub fn list(id: &str) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let spec = match open::resolve(&paths, id) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    for warning in &spec.warnings {
        eprintln!("tfsapp-hub: warning: {warning}");
    }

    println!("{id}'s declared run aliases:");
    println!("{}", format_alias_list(&spec.manifest.run));
    EXIT_OK
}

/// `run --stop <id>`/the stop half of `run --replace <id> <alias>`'s whole
/// imperative flow: packaged-only in spirit — an app must be installed to
/// have a `run.lock` at all — but deliberately **not** gated on rule 1's
/// version check above: recovering an installation whose app layer is stale
/// is one of this command's own jobs, so it must work even then.
pub fn stop(id: &str) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let spec = match open::resolve(&paths, id) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    for warning in &spec.warnings {
        eprintln!("tfsapp-hub: warning: {warning}");
    }

    let identifier = &spec.identity.identifier;
    let data_dir = match paths.create_app_data_dir(identifier) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match stop_active_run(&data_dir, identifier) {
        Ok(outcome) => {
            let run_lock_path = data_dir.join("run.lock");
            println!(
                "tfsapp-hub: {}",
                stop_outcome_message(&outcome, &run_lock_path)
            );
            if stop_outcome_succeeded(&outcome) {
                EXIT_OK
            } else {
                EXIT_FAILED
            }
        }
        Err(error) => {
            eprintln!("tfsapp-hub: cannot stop the active run command: {error}");
            EXIT_FAILED
        }
    }
}

/// `run <id> <alias> [args...]`/`run --replace <id> <alias> [args...]`'s
/// whole imperative flow (CONTRACT.md §6's "Running a declared command"):
/// resolves the alias against the installed manifest, gates on the app layer
/// being up to date (rule 1), on being the only active `run` command for
/// *this* app (rule 2, an exclusive `run.lock` flock keyed on the app's own
/// `identifier` — a different app's `run.lock` is a different file and is
/// unaffected) and on the per-alias concurrency permission (rule 3), then
/// spawns the declared `bin/console` command in the foreground — inherited
/// stdio, `SIGINT`/`SIGTERM` forwarded, a coexistence watchdog when a window
/// was already live at start — and exits with its status.
///
/// This is the one hub command whose exit code is the child's rather than the
/// hub's own: a child that happens to exit `2` is indistinguishable from a
/// usage error, and that is accepted here exactly as the station accepts it.
/// Every hub-side failure below exits `EXIT_FAILED` instead.
pub fn start(id: &str, alias_name: &str, args: &[String], replace: bool) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let spec = match open::resolve(&paths, id) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    for warning in &spec.warnings {
        eprintln!("tfsapp-hub: warning: {warning}");
    }

    let Some(alias) = spec.manifest.run.get(alias_name) else {
        eprintln!(
            "tfsapp-hub: unknown run alias \"{alias_name}\" for {id}.\nDeclared aliases:\n{}",
            format_alias_list(&spec.manifest.run)
        );
        return EXIT_FAILED;
    };

    let identifier = spec.identity.identifier.clone();
    let data_dir = match paths.create_app_data_dir(&identifier) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let data_subdir = data_dir.join("data");

    // Rule 1 (CONTRACT.md §6): the app layer must already be migrated —
    // proven by `data/config.json`'s recorded version matching this app's
    // own, the same file the launch-time version guard reads.
    let config_file = crate::lifecycle::data_config_path(&data_subdir);
    let recorded = match crate::lifecycle::read_data_version(&data_subdir) {
        Ok(recorded) => recorded,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let current = match semver::Version::parse(&spec.manifest.app_version) {
        Ok(current) => current,
        // The installer refuses a non-semver `app_version`, so reaching this
        // means the installed snapshot's manifest was edited since.
        Err(error) => {
            eprintln!(
                "tfsapp-hub: {id} declares version {:?}, which is not valid semver ({error}) — \
                 reinstall the app.",
                spec.manifest.app_version
            );
            return EXIT_FAILED;
        }
    };
    if let VersionGate::Refuse(message) =
        check_version_gate(id, &config_file, recorded.as_deref(), &current)
    {
        eprintln!("tfsapp-hub: {message}");
        return EXIT_FAILED;
    }

    // `--replace`: stop whatever currently holds `run.lock` before rule 2
    // below gets a chance to refuse over it. Runs after the version gate (a
    // stale app layer must still refuse a plain `run <id> <alias>`) but
    // before rule 2 (there would be nothing left to stop once it already
    // refused). A free lock makes this a no-op (`stop_active_run` itself
    // reports `NotRunning`), so `--replace` is safe to pass unconditionally.
    if replace {
        match stop_active_run(&data_dir, &identifier) {
            Ok(outcome) if stop_outcome_succeeded(&outcome) => {}
            Ok(outcome) => {
                eprintln!(
                    "tfsapp-hub: cannot replace \"{alias_name}\": {}",
                    stop_outcome_message(&outcome, &data_dir.join("run.lock"))
                );
                return EXIT_FAILED;
            }
            Err(error) => {
                eprintln!("tfsapp-hub: cannot stop the active run command: {error}");
                return EXIT_FAILED;
            }
        }
    }

    // Rule 2 (CONTRACT.md §6): at most one `run` command per app at a time —
    // the same non-blocking exclusive-flock primitive the sidecar liveness
    // lock uses, just a different file. Held for this process's whole
    // lifetime (bound to `run_lock`), released on drop — including if this
    // process dies without a clean exit.
    let run_lock_path = data_dir.join("run.lock");
    let mut run_lock = match tfsapp_core::process::try_lock_file(&run_lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "tfsapp-hub: another run command is already active for {id} ({})",
                run_lock_path.display()
            );
            return EXIT_FAILED;
        }
        Err(error) => {
            eprintln!(
                "tfsapp-hub: cannot acquire {}: {error}",
                run_lock_path.display()
            );
            return EXIT_FAILED;
        }
    };
    // Recorded so a would-be app-launch refusal (step 3) can name the active
    // alias rather than just the lock path. No pid yet — the child hasn't
    // been spawned; rewritten with the pid right after it is, below.
    let _ = run_lock.set_len(0);
    let _ = run_lock.write_all(format_run_lock(alias_name, None).as_bytes());

    // Rule 3 (CONTRACT.md §6): probe whether an app window is already live —
    // the exact same liveness lock the launcher itself uses. A window live
    // and this alias not declared `concurrent` refuses outright: the app is
    // always owner-first, so the only way to get both running is opening the
    // window first, then starting a `concurrent` alias.
    let pid_file = data_dir.join("sidecar.pid");
    let owner_live = match tfsapp_core::process::is_owner_live(&pid_file) {
        Ok(owner_live) => owner_live,
        Err(error) => {
            eprintln!("tfsapp-hub: cannot probe {}: {error}", pid_file.display());
            return EXIT_FAILED;
        }
    };
    if owner_live && !alias.concurrent {
        eprintln!(
            "tfsapp-hub: \"{alias_name}\" cannot run while {id} is running (it is not declared \
             concurrent) — close the app first, or declare this alias concurrent."
        );
        return EXIT_FAILED;
    }

    // Execution: the exact same `<bundled frankenphp> php-cli <app_dir>/
    // bin/console …` invocation shape and env the lifecycle commands use, but
    // with the terminal's own stdio inherited (`Command`'s default) rather
    // than captured, since `run` is an interactive foreground subcommand, not
    // a silent hook.
    let environment = match app_env::resolve(
        &spec.manifest,
        &spec.app_dir,
        &identifier,
        &data_dir,
        app_env::Mode::Run,
    ) {
        Ok(environment) => environment,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let toolchain = match php::toolchain(&paths) {
        Ok(toolchain) => toolchain,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let mut envs = environment.vars.clone();
    envs.extend(toolchain.shim_env());

    let console = spec.app_dir.join("bin/console");
    let alias_argv: Vec<&str> = alias.command.split_whitespace().collect();

    let signal_read_fd = match tfsapp_core::process::install_signal_forwarding() {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("tfsapp-hub: cannot install signal forwarding: {error}");
            return EXIT_FAILED;
        }
    };

    let mut command = tfsapp_core::sidecar::command_with_env(&toolchain.frankenphp, &envs);
    command
        .arg("php-cli")
        .arg(&console)
        .args(&alias_argv)
        .args(args)
        .current_dir(&spec.app_dir);
    tfsapp_core::process::set_own_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("tfsapp-hub: cannot start \"{}\": {error}", alias.command);
            return EXIT_FAILED;
        }
    };
    let child_pid = child.id();

    // Rewrite `run.lock`'s record with the now-known child pid. Reuses the
    // same handle the lock-acquisition-time write used above, so it must
    // `seek` back to the start in addition to `set_len(0)` — `set_len`
    // truncates but does not move the file cursor.
    let _ = run_lock.set_len(0);
    let _ = run_lock.seek(SeekFrom::Start(0));
    let _ = run_lock.write_all(format_run_lock(alias_name, Some(child_pid)).as_bytes());

    // Both detached threads below get their own clone of `identifier`: each
    // reacts an unbounded time after `child_pid` was recorded, so it
    // re-checks `/proc/<child_pid>/environ` immediately before signalling
    // rather than trusting a pid that may since have been reaped and
    // recycled onto an unrelated process.
    tfsapp_core::process::spawn_signal_forwarder(signal_read_fd, child_pid, identifier.clone());

    // Coexistence watchdog (rule 3): only when a window was already live at
    // this command's own start — reachable only by a `concurrent` alias,
    // since a non-concurrent one already refused above.
    if owner_live {
        tfsapp_core::process::spawn_coexistence_watchdog(pid_file, child_pid, identifier);
    }

    let status = child.wait();
    drop(run_lock);
    match status {
        Ok(status) => status.code().unwrap_or(EXIT_FAILED),
        Err(error) => {
            eprintln!("tfsapp-hub: cannot wait for \"{}\": {error}", alias.command);
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
