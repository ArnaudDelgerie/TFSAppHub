//! `run <id> <alias>` (plan 013, CONTRACT.md's "`run <alias>` (plan 047)" and
//! §6's rules 1–3) — an installed app's declared `bin/console` command,
//! executed in the foreground with that app's own §3 environment.
//!
//! **A transplant, not a design.** The station's `desktop/src-tauri/src/run.rs`
//! (600 lines, its most delicate module) already implements every rule here,
//! and every primitive it stands on — `try_lock_file`, `wait_for_lock_release`,
//! `is_owner_live`, `terminate_if_identifier_matches`,
//! `install_signal_forwarding`, `spawn_signal_forwarder`,
//! `set_own_process_group` — already lives in
//! `tfsapp_core::process` (plan 002). What is genuinely new is one step in
//! front of it — resolving *which* app — and the fact that the hub has two
//! apps' worth of state to keep apart while doing it. `../TFSAppWorkstation/
//! .project/hub/003-cli-surface.md` §4 already answers why two different apps
//! running commands at once costs zero design work: `runs/` is keyed on the
//! app's own `identifier`, not on the hub-local `id`, so it is already
//! per-app.
//!
//! **Plan 047 turned the single `run.lock` file into `runs/`, one entry per
//! launcher** (`../decision/005-concurrency-belongs-to-the-alias.md`): an app
//! now runs as many commands at once as its aliases declare themselves
//! `concurrent`. [`scan_runs`] is the one shared reader every guard in this
//! module and in `lifecycle.rs` consults.
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

/// [`format_alias_list`], with each alias's active instance count appended
/// (plan 047 step 5, `../decision/005-concurrency-belongs-to-the-alias.md`,
/// "What is lost, honestly": the record set trades one hidden flock for a
/// list nobody sees unless it is made readable). `active_counts` maps an
/// alias name to how many `runs/` entries the scan found naming it; an alias
/// absent from the map, or mapped to `0`, prints no count at all.
pub fn format_alias_list_with_activity(
    aliases: &BTreeMap<String, RunAlias>,
    active_counts: &BTreeMap<String, usize>,
) -> String {
    if aliases.is_empty() {
        return "no run aliases are declared by this app".to_string();
    }
    aliases
        .iter()
        .map(|(name, alias)| {
            let concurrency = if alias.concurrent {
                "concurrent"
            } else {
                "standalone-only"
            };
            match active_counts.get(name).copied().unwrap_or(0) {
                0 => format!("  {name} -> {} ({concurrency})", alias.command),
                1 => format!("  {name} -> {} ({concurrency}, active)", alias.command),
                count => format!(
                    "  {name} -> {} ({concurrency}, {count} active)",
                    alias.command
                ),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One active `run` command found across *every* installed app —
/// `run --stop`/`run --replace` with no id (plan 047 step 5): the recovery
/// listing decision 005's "A user can now start ten commands and not know
/// it" buys back. `id` is the hub-local id ([`format_alias_list`] and
/// [`ActiveRun`] never need it, since they already work within one app's own
/// resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveInstance {
    pub id: String,
    pub alias: String,
    pub pid: Option<u32>,
}

/// Format every active instance, one line each as `<id>  <alias>  <pid>` —
/// `pid` prints `unknown` for the same reason [`stop_outcome_message`] does,
/// never a fatal condition on its own. Empty is not an error: a hub with
/// nothing running answers exactly as plainly as [`list::render`] does with
/// nothing installed.
pub fn format_active_instances(instances: &[ActiveInstance]) -> String {
    if instances.is_empty() {
        return "no run command is active for any installed app".to_string();
    }
    instances
        .iter()
        .map(|instance| {
            let pid = instance
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            format!("{}  {}  {pid}", instance.id, instance.alias)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// --- `runs/`: one entry per launcher (plan 047,
// `../decision/005-concurrency-belongs-to-the-alias.md`) --------------------
//
// Replaces the single fixed `run.lock` file: one entry per launcher, named
// after the launcher's own pid, each exclusively flocked by its owner and
// carrying `<alias>\n<child pid>`.

/// One `runs/` entry's parsed content: the alias that owns it, and the
/// spawned child's pid once recorded. `pid` is `None` in the narrow window
/// between the entry's flock being acquired and the post-spawn rewrite, never
/// an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEntry {
    pub alias: String,
    pub pid: Option<u32>,
}

/// Format one `runs/` entry's content: `<alias>` alone when `pid` is `None`
/// (the acquisition-time write, before the child exists), `<alias>\n<pid>`
/// once it does. Pure counterpart to [`parse_run_entry`] — round-trips
/// through it.
pub fn format_run_entry(alias: &str, pid: Option<u32>) -> String {
    match pid {
        Some(pid) => format!("{alias}\n{pid}"),
        None => alias.to_string(),
    }
}

/// Parse one `runs/` entry's content: `None` for empty content (nothing ever
/// written — should not happen while the entry's flock is held, but this is
/// the pure, total counterpart callers can match on regardless). Otherwise
/// the first line is the alias; a present, numeric second line is the pid,
/// and anything else (absent, non-numeric, a stale trailing newline) parses
/// as "no pid yet" rather than an error — the record format is advisory, not
/// a wire contract with a hard failure mode.
pub fn parse_run_entry(contents: &str) -> Option<RunEntry> {
    let mut lines = contents.lines();
    let alias = lines.next()?.to_string();
    let pid = lines
        .next()
        .and_then(|line| line.trim().parse::<u32>().ok());
    Some(RunEntry { alias, pid })
}

/// The file name a `runs/` entry is written under: its launcher's own pid.
/// Naming an entry after its launcher is what closes the unlink-versus-flock
/// race a lock directory usually carries — nobody creates an entry under
/// another's name, and nobody unlinks an entry whose pid is live
/// (`../decision/005-concurrency-belongs-to-the-alias.md`, "What is lost,
/// honestly").
pub fn run_entry_file_name(launcher_pid: u32) -> String {
    format!("{launcher_pid}.lock")
}

/// One entry's status once a scanner has tried to flock it and, if that
/// succeeded, identity-probed the pid its record names — the exact ternary
/// `../plan/035-the-run-record-outlives-its-flock.md` gave `run.lock` as a
/// single file, applied once to one `runs/` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEntryStatus {
    /// The scanner could not acquire the entry's flock: its launcher is
    /// still alive. `alias`/`pid` are a best-effort read of its record —
    /// `None` in the acquisition-to-spawn window, or if the record could not
    /// be read at all.
    LiveLauncher {
        alias: Option<String>,
        pid: Option<u32>,
    },
    /// The flock was free but the record names a pid that is alive and
    /// identity-proven: the launcher died without cleaning up, but its
    /// child is still active.
    ActiveOrphan { alias: String, pid: u32 },
    /// The flock was free and the record is empty, unparseable, or names a
    /// pid that is dead or fails the identity proof. Inert.
    Stale,
}

/// Pure per-entry ternary. `record` is read regardless of whether the flock
/// was acquired — a best-effort read, since the write it might race is a
/// truncate-then-rewrite by the entry's own owner, never a second writer.
pub fn run_entry_status(
    flock_acquired: bool,
    record: Option<&RunEntry>,
    pid_alive: bool,
    identifier_matches: bool,
) -> RunEntryStatus {
    if !flock_acquired {
        return RunEntryStatus::LiveLauncher {
            alias: record.map(|record| record.alias.clone()),
            pid: record.and_then(|record| record.pid),
        };
    }
    match record {
        Some(RunEntry {
            alias,
            pid: Some(pid),
        }) if pid_alive && identifier_matches => RunEntryStatus::ActiveOrphan {
            alias: alias.clone(),
            pid: *pid,
        },
        _ => RunEntryStatus::Stale,
    }
}

/// One active `runs/` entry [`scan_runs`] found — a live launcher or an
/// active orphan; a stale entry is unlinked on the spot instead of reported.
/// `alias`/`pid` are best-effort reads of the entry's record, `None` only in
/// the narrow acquisition-to-write race any single reader could observe.
/// `orphaned` distinguishes the two active cases for callers whose message or
/// stop action differs between them. `path` is the entry's own file, kept for
/// callers that act on this specific entry ([`stop_one_entry`]) or report it
/// (`run --stop`'s bare listing, plan 047 step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRunEntry {
    pub alias: Option<String>,
    pub pid: Option<u32>,
    pub orphaned: bool,
    pub path: std::path::PathBuf,
}

/// Scan `<data_dir>/runs/`, applying [`run_entry_status`] to every entry: a
/// live launcher or an active orphan is reported; a stale entry is unlinked
/// on the spot rather than reported — the scan is its own janitor, so every
/// caller that asks "what's active" also cleans up after whoever left an
/// entry behind. Never unlinks an entry whose recorded pid is live, which is
/// what keeps the unlink-versus-flock race closed
/// (`../decision/005-concurrency-belongs-to-the-alias.md`). A missing
/// `runs/` directory is nothing running, not an error. The result is sorted
/// by entry path for a deterministic order.
pub fn scan_runs(data_dir: &Path, identifier: &str) -> std::io::Result<Vec<ActiveRunEntry>> {
    let runs_dir = data_dir.join("runs");
    let entries = match std::fs::read_dir(&runs_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut active = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        match tfsapp_core::process::try_lock_file(&path)? {
            Some(_lock) => {
                let record = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|contents| parse_run_entry(&contents));
                let (pid_alive, identifier_matches) =
                    match record.as_ref().and_then(|record| record.pid) {
                        Some(pid) => (
                            tfsapp_core::process::process_exists(pid),
                            tfsapp_core::process::process_environ_has_identifier(pid, identifier),
                        ),
                        None => (false, false),
                    };
                match run_entry_status(true, record.as_ref(), pid_alive, identifier_matches) {
                    RunEntryStatus::ActiveOrphan { alias, pid } => active.push(ActiveRunEntry {
                        alias: Some(alias),
                        pid: Some(pid),
                        orphaned: true,
                        path,
                    }),
                    RunEntryStatus::Stale => {
                        let _ = std::fs::remove_file(&path);
                    }
                    RunEntryStatus::LiveLauncher { .. } => {
                        unreachable!("an acquired flock never decides as a live launcher")
                    }
                }
            }
            None => {
                let record = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|contents| parse_run_entry(&contents));
                match run_entry_status(false, record.as_ref(), false, false) {
                    RunEntryStatus::LiveLauncher { alias, pid } => active.push(ActiveRunEntry {
                        alias,
                        pid,
                        orphaned: false,
                        path,
                    }),
                    _ => unreachable!("a free flock always decides as a live launcher"),
                }
            }
        }
    }
    active.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(active)
}

/// How long [`stop_one_entry`] waits for an entry's flock to free after
/// signalling the recorded child — must outlast `terminate`'s own
/// SIGTERM-then-3s-SIGKILL escalation plus a little slack for the launcher to
/// actually observe `child.wait()` return and drop the lock.
const STOP_LOCK_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcome of stopping one target for `run --stop`/the stop half of
/// `run --replace <id> <alias>` — exactly the six states [`stop_one_entry`]
/// can end in. Kept as plain data — no message text or exit-code logic
/// inside the enum itself — so [`stop_outcome_message`] below can be
/// unit-tested directly against each variant without any process I/O.
pub enum StopOutcome {
    /// `runs/` had nothing active: no `run` command is active for this app.
    NotRunning,
    /// A record with a pid was found, signalled, and its entry's lock freed.
    Stopped { alias: String },
    /// An entry named a live child whose launcher had already gone; the
    /// child was terminated and observed gone.
    StoppedOrphan { alias: String },
    /// An entry named a live child whose launcher had already gone, but it
    /// remained live after the bounded stop wait.
    OrphanStillRunning { alias: String, pid: u32 },
    /// An entry's lock is held but its record has no pid yet — the narrow
    /// window between lock acquisition and the post-spawn rewrite.
    PidUnknown { alias: String },
    /// A record with a pid was found and signalled, but its entry's lock was
    /// still held once `STOP_LOCK_RELEASE_TIMEOUT` elapsed.
    LockHeld {
        alias: String,
        pid: u32,
        entry_path: std::path::PathBuf,
    },
}

/// Render `outcome` as the message `run --stop`/`--replace` prints.
pub fn stop_outcome_message(outcome: &StopOutcome) -> String {
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
        StopOutcome::LockHeld {
            alias,
            pid,
            entry_path,
        } => format!(
            "signalled the run command \"{alias}\" (pid {pid}) but {} was still held after \
             waiting — it may be wedged outside its own child process",
            entry_path.display()
        ),
    }
}

/// Whether one target's `runs/` entry is free once [`stop_one_entry`]
/// returns for it — `NotRunning` and `Stopped` mean it is launchable again;
/// `PidUnknown`, `OrphanStillRunning`, and `LockHeld` mean it is not. Shared
/// by `--stop`'s exit code and `--replace`'s decision to abort rather than
/// continue into rule 2.
pub fn stop_outcome_succeeded(outcome: &StopOutcome) -> bool {
    matches!(
        outcome,
        StopOutcome::NotRunning | StopOutcome::Stopped { .. } | StopOutcome::StoppedOrphan { .. }
    )
}

/// `run --stop <id> [alias]`/the stop half of `run --replace <id> <alias>`'s
/// whole imperative flow (plan 047 step 3): [`scan_runs`] for whatever
/// `runs/` currently records, narrows to `alias_filter`'s instances when
/// given, and stops each target through [`stop_one_entry`]. Never returns an
/// empty `Vec`: nothing matching is reported as a single `NotRunning`, so
/// every caller can print every outcome and take an aggregate exit without a
/// separate empty check — one wedged command does not hide the ones that
/// stopped (`../decision/005-concurrency-belongs-to-the-alias.md`,
/// "`--stop <id>` becomes blunt"). `alias_filter: None` is that bluntness:
/// every active command for the app. Deliberately does **not** take rule 1's
/// version gate as an argument or call it itself: recovering an installation
/// whose app layer is stale is one of this command's own jobs.
pub fn stop_active_runs(
    data_dir: &Path,
    identifier: &str,
    alias_filter: Option<&str>,
) -> std::io::Result<Vec<StopOutcome>> {
    let targets: Vec<_> = scan_runs(data_dir, identifier)?
        .into_iter()
        .filter(|entry| match alias_filter {
            Some(alias) => entry.alias.as_deref() == Some(alias),
            None => true,
        })
        .collect();
    if targets.is_empty() {
        return Ok(vec![StopOutcome::NotRunning]);
    }
    targets
        .into_iter()
        .map(|entry| stop_one_entry(entry, identifier))
        .collect()
}

/// Stop one `runs/` entry: terminates the recorded child through
/// `tfsapp_core::process::terminate_if_identifier_matches` (never a bare,
/// unguarded terminate — the identity proof against pid reuse), and waits for
/// its entry's lock to actually free before reporting.
fn stop_one_entry(entry: ActiveRunEntry, identifier: &str) -> std::io::Result<StopOutcome> {
    if entry.orphaned {
        let alias = entry
            .alias
            .expect("an active orphan always names its alias");
        let pid = entry
            .pid
            .expect("an active orphan always records its child pid");
        tfsapp_core::process::terminate_if_identifier_matches(pid, identifier);
        let deadline = Instant::now() + STOP_LOCK_RELEASE_TIMEOUT;
        while tfsapp_core::process::process_exists(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        return if tfsapp_core::process::process_exists(pid) {
            Ok(StopOutcome::OrphanStillRunning { alias, pid })
        } else {
            let _ = std::fs::remove_file(&entry.path);
            Ok(StopOutcome::StoppedOrphan { alias })
        };
    }

    let Some(alias) = entry.alias else {
        return Ok(StopOutcome::PidUnknown {
            alias: "an unknown alias".to_string(),
        });
    };
    let Some(pid) = entry.pid else {
        return Ok(StopOutcome::PidUnknown { alias });
    };

    tfsapp_core::process::terminate_if_identifier_matches(pid, identifier);
    if tfsapp_core::process::wait_for_lock_release(&entry.path, STOP_LOCK_RELEASE_TIMEOUT)? {
        Ok(StopOutcome::Stopped { alias })
    } else {
        Ok(StopOutcome::LockHeld {
            alias,
            pid,
            entry_path: entry.path,
        })
    }
}

/// One active `run` command, resolved for rule 2's lifted verdict — the
/// [`ActiveRunEntry`] a scan found, with `concurrent` looked up by the
/// caller from the app's manifest, keyed on `alias`. This module knows
/// nothing of manifests, hence the two separate types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub alias: String,
    pub pid: Option<u32>,
    pub concurrent: bool,
}

/// Rule 2, lifted (`../decision/005-concurrency-belongs-to-the-alias.md`):
/// whether a newcomer alias may start, given its own `concurrent` flag and
/// every alias the scan found active. `true` stacks with itself and with
/// other `concurrent` aliases; `false` refuses whenever anything is active,
/// and is refused by anything already active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStartVerdict {
    MayStart,
    /// `newcomer_non_concurrent` distinguishes the two refusal shapes: the
    /// newcomer's own flag blocked it (refuses regardless of `blocker`'s
    /// flag), or a non-`concurrent` `blocker` is what blocked an otherwise
    /// `concurrent` newcomer. The two cases have different fixes.
    Blocked {
        blocker: ActiveRun,
        newcomer_non_concurrent: bool,
    },
}

/// Resolve a scan's raw entries against the app's manifest, filling in each
/// entry's `concurrent` flag by the alias its record names — an entry naming
/// an alias the manifest no longer declares, or none at all, is treated as
/// non-`concurrent` (the conservative default: it cannot be proven safe to
/// stack beside). Shared by `run::start`'s own guard and
/// `lifecycle::prepare_launch`'s launch-side gate, so the two verdicts never
/// diverge on what "active" means.
pub fn resolve_active_runs(
    active: Vec<ActiveRunEntry>,
    run_aliases: &BTreeMap<String, RunAlias>,
) -> Vec<ActiveRun> {
    active
        .into_iter()
        .map(|entry| ActiveRun {
            alias: entry
                .alias
                .clone()
                .unwrap_or_else(|| "an unknown alias".to_string()),
            pid: entry.pid,
            concurrent: entry
                .alias
                .as_deref()
                .and_then(|name| run_aliases.get(name))
                .is_some_and(|declared| declared.concurrent),
        })
        .collect()
}

/// Pure verdict over an already-resolved `active` list — the caller has
/// already matched each entry's alias against the manifest to fill in
/// `concurrent`.
pub fn run_start_verdict(newcomer_concurrent: bool, active: &[ActiveRun]) -> RunStartVerdict {
    if !newcomer_concurrent {
        if let Some(blocker) = active.first() {
            return RunStartVerdict::Blocked {
                blocker: blocker.clone(),
                newcomer_non_concurrent: true,
            };
        }
    } else if let Some(blocker) = active.iter().find(|run| !run.concurrent) {
        return RunStartVerdict::Blocked {
            blocker: blocker.clone(),
            newcomer_non_concurrent: false,
        };
    }
    RunStartVerdict::MayStart
}

/// Rule 3's window-side verdict, narrowed to its motive
/// (`../decision/005-concurrency-belongs-to-the-alias.md`, point 3): a launch
/// with a lifecycle event to run refuses over *any* active command, since the
/// launch path is the only one that can show progress; a launch with nothing
/// to run refuses only over a non-`concurrent` one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchVerdict {
    MayOpen,
    Refuse { blocker: ActiveRun },
}

pub fn launch_verdict(has_lifecycle_event: bool, active: &[ActiveRun]) -> LaunchVerdict {
    let blocker = if has_lifecycle_event {
        active.first()
    } else {
        active.iter().find(|run| !run.concurrent)
    };
    match blocker {
        Some(blocker) => LaunchVerdict::Refuse {
            blocker: blocker.clone(),
        },
        None => LaunchVerdict::MayOpen,
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
    lifecycle_gate, open,
    paths::Paths,
    php, registry,
};

/// `run <id>` with no alias: list the app's declared aliases (the hub's own
/// addition to the station's grammar — see `cli::RunInvocation::List`). The
/// station discovers its aliases through `--help`, which the hub cannot do
/// since they belong to an app and not to the binary.
///
/// Marks each alias with how many instances are active (plan 047 step 5): a
/// scan of its own `runs/`, tallied by alias — the same scan every other
/// guard in this module reads, so the count can never disagree with what a
/// start or stop would see.
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

    let identifier = &spec.identity.identifier;
    let data_dir = match paths.app_data_dir(identifier) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let active = match scan_runs(&data_dir, identifier) {
        Ok(active) => active,
        Err(error) => {
            eprintln!(
                "tfsapp-hub: cannot scan {}: {error}",
                data_dir.join("runs").display()
            );
            return EXIT_FAILED;
        }
    };
    let mut active_counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in active {
        if let Some(alias) = entry.alias {
            *active_counts.entry(alias).or_insert(0) += 1;
        }
    }

    println!("{id}'s declared run aliases:");
    println!(
        "{}",
        format_alias_list_with_activity(&spec.manifest.run, &active_counts)
    );
    EXIT_OK
}

/// `run --stop`/`run --replace` with no id (plan 047 step 5,
/// `../decision/005-concurrency-belongs-to-the-alias.md`, "A user can now
/// start ten commands and not know it"): every active `run` command across
/// every installed app, enumerated through the registry the way `list::run`
/// is, followed by the usage line of whichever typed form asked for it —
/// `--stop` or `--replace`, so the message points back at the command that
/// produced it.
pub fn list_active(usage: &str) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let registry = match registry::load(&paths) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    let mut instances = Vec::new();
    for entry in &registry.apps {
        let data_dir = match paths.app_data_dir(&entry.identifier) {
            Ok(data_dir) => data_dir,
            Err(error) => {
                eprintln!("tfsapp-hub: {error}");
                return EXIT_FAILED;
            }
        };
        let active = match scan_runs(&data_dir, &entry.identifier) {
            Ok(active) => active,
            Err(error) => {
                eprintln!(
                    "tfsapp-hub: cannot scan {}: {error}",
                    data_dir.join("runs").display()
                );
                return EXIT_FAILED;
            }
        };
        instances.extend(active.into_iter().map(|run| ActiveInstance {
            id: entry.id.clone(),
            alias: run.alias.unwrap_or_else(|| "an unknown alias".to_string()),
            pid: run.pid,
        }));
    }

    println!("{}", format_active_instances(&instances));
    println!("usage: tfsapp-hub {usage}");
    EXIT_OK
}

/// `run --stop <id> [alias]`/the stop half of `run --replace <id> <alias>`'s
/// whole imperative flow: packaged-only in spirit — an app must be installed
/// to have a `runs/` directory at all — but deliberately **not** gated on
/// rule 1's version check above: recovering an installation whose app layer
/// is stale is one of this command's own jobs, so it must work even then.
/// `alias` narrows the stop to that alias's instances (plan 047 step 3);
/// `None` stops every active command for the app. Prints one line per
/// target and exits `EXIT_OK` only when every one of them succeeded — a
/// wedged target does not hide the others in the exit code any more than in
/// the printed output.
pub fn stop(id: &str, alias: Option<&str>) -> i32 {
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
    let _activity = match lifecycle_gate::acquire_activity(&paths, identifier) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let data_dir = match paths.create_app_data_dir(identifier) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match stop_active_runs(&data_dir, identifier, alias) {
        Ok(outcomes) => {
            let mut all_succeeded = true;
            for outcome in &outcomes {
                println!("tfsapp-hub: {}", stop_outcome_message(outcome));
                all_succeeded &= stop_outcome_succeeded(outcome);
            }
            if all_succeeded {
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
/// *this* app (rule 2, a scan of `runs/` keyed on the app's own `identifier`
/// — a different app's `runs/` is a different directory and is unaffected)
/// and on the per-alias concurrency permission (rule 3), then spawns the
/// declared `bin/console` command in the foreground — inherited stdio,
/// `SIGINT`/`SIGTERM` forwarded — and exits with its status. Rule 2 is lifted
/// (plan 047 step 3, `../decision/005-concurrency-belongs-to-the-alias.md`):
/// a `concurrent` alias stacks with itself and with other `concurrent`
/// aliases; a non-`concurrent` one still refuses beside anything, and is
/// refused by anything already active. Its lifetime belongs to whoever
/// started it, not to a window that happened to be open at the time (plan
/// 047 step 4, decision 005 point 4: the coexistence watchdog that used to
/// tie the two together is gone).
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

    // `--replace` on a `concurrent` alias (plan 047 step 3,
    // `../decision/005-concurrency-belongs-to-the-alias.md`, point 5): there
    // is nothing to replace when instances stack, so this refuses before
    // touching anything rather than silently replacing one of several.
    if replace && alias.concurrent {
        eprintln!(
            "tfsapp-hub: \"{alias_name}\" is declared concurrent — there is nothing for \
             --replace to replace. Start another instance instead with `tfsapp-hub run {id} \
             {alias_name}`."
        );
        return EXIT_FAILED;
    }

    let identifier = spec.identity.identifier.clone();
    // This covers all foreground forms, including one `--replace` stop/start
    // transition. Its retained handle lives through `child.wait()` below.
    let _activity = match lifecycle_gate::acquire_activity(&paths, &identifier) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
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

    // `--replace`: stop everything `runs/` currently records for this app
    // before rule 2 below gets a chance to refuse over it — the alias being
    // started is non-`concurrent` here (the check above already refused a
    // `concurrent` one), so rule 2 would refuse beside *any* active entry,
    // not just ones sharing its own alias. Runs after the version gate (a
    // stale app layer must still refuse a plain `run <id> <alias>`) but
    // before rule 2 (there would be nothing left to stop once it already
    // refused). Nothing active makes this a no-op (`stop_active_runs` itself
    // reports a single `NotRunning`), so `--replace` is safe to pass
    // unconditionally.
    if replace {
        match stop_active_runs(&data_dir, &identifier, None) {
            Ok(outcomes) => {
                if let Some(failed) = outcomes
                    .iter()
                    .find(|outcome| !stop_outcome_succeeded(outcome))
                {
                    eprintln!(
                        "tfsapp-hub: cannot replace \"{alias_name}\": {}",
                        stop_outcome_message(failed)
                    );
                    return EXIT_FAILED;
                }
            }
            Err(error) => {
                eprintln!("tfsapp-hub: cannot stop the active run command: {error}");
                return EXIT_FAILED;
            }
        }
    }

    // Rule 2, lifted (plan 047 step 3,
    // `../decision/005-concurrency-belongs-to-the-alias.md`): resolve this
    // alias's own `concurrent` flag and, for each active entry the scan
    // found, its alias's flag from the manifest — an entry naming an alias
    // the manifest no longer declares, or none at all, is treated as
    // non-`concurrent` (the conservative default: it cannot be proven safe
    // to stack beside) — then let `run_start_verdict` decide.
    let runs_dir = data_dir.join("runs");
    if let Err(error) = std::fs::create_dir_all(&runs_dir) {
        eprintln!("tfsapp-hub: cannot create {}: {error}", runs_dir.display());
        return EXIT_FAILED;
    }
    let active = match scan_runs(&data_dir, &identifier) {
        Ok(active) => active,
        Err(error) => {
            eprintln!("tfsapp-hub: cannot scan {}: {error}", runs_dir.display());
            return EXIT_FAILED;
        }
    };
    let active_runs = resolve_active_runs(active, &spec.manifest.run);
    if let RunStartVerdict::Blocked {
        blocker,
        newcomer_non_concurrent,
    } = run_start_verdict(alias.concurrent, &active_runs)
    {
        let pid = blocker
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        if newcomer_non_concurrent {
            eprintln!(
                "tfsapp-hub: cannot start \"{alias_name}\": it is not declared concurrent, and \
                 \"{}\" (pid {pid}) is already active for {id} — stop it first with \
                 `tfsapp-hub run --stop {id} {}`, or declare \"{alias_name}\" concurrent.",
                blocker.alias, blocker.alias
            );
        } else {
            eprintln!(
                "tfsapp-hub: cannot start \"{alias_name}\": \"{}\" (pid {pid}) is active for {id} \
                 and is not declared concurrent — stop it first with `tfsapp-hub run --stop {id} \
                 {}`.",
                blocker.alias, blocker.alias
            );
        }
        return EXIT_FAILED;
    }

    // This launcher's own entry, named after its own pid — never contested,
    // since no other live process can share this pid
    // (`../decision/005-concurrency-belongs-to-the-alias.md`, "What is lost,
    // honestly"). Held for this process's whole lifetime (bound to
    // `run_entry_lock`), released on drop — including if this process dies
    // without a clean exit.
    let entry_path = runs_dir.join(run_entry_file_name(std::process::id()));
    let mut run_entry_lock = match tfsapp_core::process::try_lock_file(&entry_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "tfsapp-hub: cannot acquire {} — already held, which should not happen for a \
                 launcher's own pid",
                entry_path.display()
            );
            return EXIT_FAILED;
        }
        Err(error) => {
            eprintln!(
                "tfsapp-hub: cannot acquire {}: {error}",
                entry_path.display()
            );
            return EXIT_FAILED;
        }
    };
    // Recorded so a would-be app-launch refusal (step 3) can name the active
    // alias rather than just the lock path. No pid yet — the child hasn't
    // been spawned; rewritten with the pid right after it is, below.
    let _ = run_entry_lock.set_len(0);
    let _ = run_entry_lock.write_all(format_run_entry(alias_name, None).as_bytes());

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

    // Rewrite this entry's record with the now-known child pid. Reuses the
    // same handle the lock-acquisition-time write used above, so it must
    // `seek` back to the start in addition to `set_len(0)` — `set_len`
    // truncates but does not move the file cursor.
    let _ = run_entry_lock.set_len(0);
    let _ = run_entry_lock.seek(SeekFrom::Start(0));
    let _ = run_entry_lock.write_all(format_run_entry(alias_name, Some(child_pid)).as_bytes());

    // Both detached threads below get their own clone of `identifier`: each
    // reacts an unbounded time after `child_pid` was recorded, so it
    // re-checks `/proc/<child_pid>/environ` immediately before signalling
    // rather than trusting a pid that may since have been reaped and
    // recycled onto an unrelated process.
    tfsapp_core::process::spawn_signal_forwarder(signal_read_fd, child_pid, identifier.clone());

    let status = child.wait();
    // Unlinked on clean exit — a process that dies without reaching this
    // line leaves its entry for the next scan to read (as a live orphan, if
    // the child outlived it, or as stale once it hasn't).
    let _ = std::fs::remove_file(&entry_path);
    drop(run_entry_lock);
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
