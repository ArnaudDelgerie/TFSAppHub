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

// Step 1 lands the pure decisions with no caller yet — `main::dispatch` does
// not route `Command::Run` until step 2 wires the imperative flow around
// them. Remove the allow once that lands, rather than letting it linger.
#![allow(dead_code)]

use std::{collections::BTreeMap, path::Path, time::Duration};

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

/// How long `stop_active_run` waits for `run.lock` to free after signalling
/// the recorded child — must outlast `terminate`'s own SIGTERM-then-3s-SIGKILL
/// escalation plus a little slack for the launcher to actually observe
/// `child.wait()` return and drop the lock.
const STOP_LOCK_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The outcome of `run --stop`/the stop half of `run --replace <id> <alias>`
/// — exactly the four states [`stop_active_run`] can end in. Kept as plain
/// data — no message text or exit-code logic inside the enum itself — so
/// [`stop_outcome_message`] below can be unit-tested directly against each
/// variant without any process I/O.
pub enum StopOutcome {
    /// `run.lock` was free: no `run` command is active for this app.
    NotRunning,
    /// A record with a pid was found, signalled, and the lock freed.
    Stopped { alias: String },
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
/// `LockHeld` mean it is not. Shared by `--stop`'s exit code and
/// `--replace`'s decision to abort rather than continue into rule 2.
pub fn stop_outcome_succeeded(outcome: &StopOutcome) -> bool {
    matches!(
        outcome,
        StopOutcome::NotRunning | StopOutcome::Stopped { .. }
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
    // immediately dropped — the same "never retain, just probe" pattern
    // `is_owner_live` uses. Nothing is running, so there is nothing to stop.
    if tfsapp_core::process::try_lock_file(&run_lock_path)?.is_some() {
        return Ok(StopOutcome::NotRunning);
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

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
