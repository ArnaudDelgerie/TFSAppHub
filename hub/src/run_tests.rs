use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use super::*;

// --- check_version_gate ------------------------------------------------------

#[test]
fn version_gate_up_to_date_on_exact_match() {
    let current = semver::Version::parse("1.2.3").unwrap();
    let config_file = Path::new("/data/config.json");
    assert!(matches!(
        check_version_gate("demo", config_file, Some("1.2.3"), &current),
        VersionGate::UpToDate
    ));
}

#[test]
fn version_gate_refuses_missing_record_pointing_at_open() {
    let current = semver::Version::parse("1.2.3").unwrap();
    let config_file = Path::new("/data/config.json");
    let VersionGate::Refuse(message) = check_version_gate("demo", config_file, None, &current)
    else {
        panic!("expected a refusal");
    };
    assert!(message.contains("tfsapp-hub open demo"), "{message}");
}

#[test]
fn version_gate_refuses_older_record_pointing_at_update() {
    let current = semver::Version::parse("1.2.3").unwrap();
    let config_file = Path::new("/data/config.json");
    let VersionGate::Refuse(message) =
        check_version_gate("demo", config_file, Some("1.0.0"), &current)
    else {
        panic!("expected a refusal");
    };
    assert!(message.contains("tfsapp-hub update demo"), "{message}");
}

#[test]
fn version_gate_refuses_newer_record_as_a_downgrade() {
    let current = semver::Version::parse("1.2.3").unwrap();
    let config_file = Path::new("/data/config.json");
    let VersionGate::Refuse(message) =
        check_version_gate("demo", config_file, Some("2.0.0"), &current)
    else {
        panic!("expected a refusal");
    };
    assert!(message.contains("downgrading"), "{message}");
    assert!(message.contains("/data/config.json"), "{message}");
}

#[test]
fn version_gate_refuses_invalid_record_naming_the_file() {
    let current = semver::Version::parse("1.2.3").unwrap();
    let config_file = Path::new("/data/config.json");
    let VersionGate::Refuse(message) =
        check_version_gate("demo", config_file, Some("not-a-version"), &current)
    else {
        panic!("expected a refusal");
    };
    assert!(message.contains("/data/config.json"), "{message}");
}

// --- stop_outcome_message / stop_outcome_succeeded --------------------------

#[test]
fn stop_outcome_message_not_running() {
    assert_eq!(
        stop_outcome_message(&StopOutcome::NotRunning),
        "no run command is currently active"
    );
}

#[test]
fn stop_outcome_message_stopped_names_the_alias() {
    let outcome = StopOutcome::Stopped {
        alias: "mcp-serve".to_string(),
    };
    assert_eq!(
        stop_outcome_message(&outcome),
        "stopped the active run command \"mcp-serve\""
    );
}

#[test]
fn stop_outcome_message_stopped_orphan_names_the_alias_and_gone_launcher() {
    let outcome = StopOutcome::StoppedOrphan {
        alias: "mcp-serve".to_string(),
    };
    let message = stop_outcome_message(&outcome);
    assert!(message.contains("mcp-serve"));
    assert!(message.contains("launcher had already gone"));
}

#[test]
fn stop_outcome_message_pid_unknown_names_the_alias_and_suggests_a_retry() {
    let outcome = StopOutcome::PidUnknown {
        alias: "mcp-serve".to_string(),
    };
    let message = stop_outcome_message(&outcome);
    assert!(message.contains("mcp-serve"));
    assert!(message.contains("retry"));
}

#[test]
fn stop_outcome_message_lock_held_names_the_alias_pid_and_entry_path() {
    let outcome = StopOutcome::LockHeld {
        alias: "mcp-serve".to_string(),
        pid: 4321,
        entry_path: PathBuf::from("/data/runs/1.lock"),
    };
    let message = stop_outcome_message(&outcome);
    assert!(message.contains("mcp-serve"));
    assert!(message.contains("4321"));
    assert!(message.contains("/data/runs/1.lock"));
}

#[test]
fn stop_outcome_succeeded_true_for_not_running_and_stopped() {
    assert!(stop_outcome_succeeded(&StopOutcome::NotRunning));
    assert!(stop_outcome_succeeded(&StopOutcome::Stopped {
        alias: "mcp-serve".to_string()
    }));
    assert!(stop_outcome_succeeded(&StopOutcome::StoppedOrphan {
        alias: "mcp-serve".to_string()
    }));
}

#[test]
fn stop_outcome_succeeded_false_for_pid_unknown_and_lock_held() {
    assert!(!stop_outcome_succeeded(&StopOutcome::PidUnknown {
        alias: "mcp-serve".to_string()
    }));
    assert!(!stop_outcome_succeeded(&StopOutcome::LockHeld {
        alias: "mcp-serve".to_string(),
        pid: 4321,
        entry_path: PathBuf::from("/data/runs/1.lock"),
    }));
    assert!(!stop_outcome_succeeded(&StopOutcome::OrphanStillRunning {
        alias: "mcp-serve".to_string(),
        pid: 4321
    }));
}

// --- scan_runs / stop_active_run ---------------------------------------------

/// Spawn a controlled child that holds `entry_path`'s exclusive flock — a
/// genuinely separate process, the same shape a real `run <id> <alias>`
/// launcher is — and carries `identifier` in its own environment, the way the
/// launcher marks every child it spawns. Deliberately not `flock(1)` given a
/// command (`flock path sleep 30`): that form forks the sleep as a *child* of
/// the `flock` process, which inherits the same locked file description
/// across `fork` (no `O_CLOEXEC` on it) — killing only the `flock` parent then
/// leaves the lock held via the orphaned, still-running `sleep`, exactly the
/// false failure this test must not produce. Instead, `exec 9>"$1"` opens the
/// lock path on this single shell process's own fd 9, `flock -n 9` locks it
/// (that temporary child exits once it has, leaving the lock in effect via
/// the shell's own fd 9 — the standard shell lock-file idiom), and the final
/// `exec sleep 30` replaces the shell with `sleep` *in the same process*, fd 9
/// and the environment both surviving the exec. One process, one fd, for its
/// entire lifetime — killing it (`terminate_if_identifier_matches`'s job
/// here) releases the lock immediately, with no descendant left holding a
/// stray reference. Blocks until the lock is observed held so the caller
/// never races it.
fn spawn_run_entry_holder(entry_path: &Path, identifier: &str) -> std::process::Child {
    let child = std::process::Command::new("sh")
        .args([
            "-c",
            r#"exec 9>"$1"; flock -n 9 || exit 1; exec sleep 30"#,
            "sh",
        ])
        .arg(entry_path)
        .env("TFS_APP_IDENTIFIER", identifier)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        if tfsapp_core::process::try_lock_file(entry_path)
            .unwrap()
            .is_none()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    child
}

#[test]
fn scan_runs_empty_when_the_directory_does_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        scan_runs(dir.path(), "test-identifier").unwrap(),
        Vec::new()
    );
}

#[test]
fn scan_runs_finds_a_live_launcher_from_its_held_entry() {
    let dir = tempfile::tempdir().unwrap();
    let runs_dir = dir.path().join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let entry_path = runs_dir.join("1.lock");
    let identifier = "test-identifier";
    let mut holder = spawn_run_entry_holder(&entry_path, identifier);
    std::fs::write(&entry_path, format_run_entry("mcp-serve", Some(4321))).unwrap();

    let active = scan_runs(dir.path(), identifier).unwrap();

    assert_eq!(active.len(), 1);
    assert_eq!(active[0].alias, Some("mcp-serve".to_string()));
    assert_eq!(active[0].pid, Some(4321));
    assert!(!active[0].orphaned);
    holder.kill().unwrap();
    holder.wait().unwrap();
}

#[test]
fn scan_runs_finds_an_active_orphan_then_unlinks_it_once_stale() {
    let dir = tempfile::tempdir().unwrap();
    let runs_dir = dir.path().join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let entry_path = runs_dir.join("1.lock");
    let identifier = "test-identifier";
    let mut child = spawn_run_entry_holder(&entry_path, identifier);
    let pid = child.id();
    std::fs::write(&entry_path, format_run_entry("mcp-serve", Some(pid))).unwrap();
    // The launcher's own lock is held by `child` above; the *entry's* flock
    // is free the instant the launcher itself is gone — simulated here by
    // never acquiring a competing lock in this test process at all, since
    // `spawn_run_entry_holder` already dropped its own local probe handle.
    child.kill().unwrap();
    child.wait().unwrap();

    let active = scan_runs(dir.path(), identifier).unwrap();
    assert_eq!(active, Vec::new());
    assert!(
        !entry_path.exists(),
        "a dead, non-identity-proven pid reads as stale and is unlinked"
    );
}

#[test]
fn stop_active_run_not_running_when_nothing_is_active() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = stop_active_run(dir.path(), "test-identifier").unwrap();
    assert!(matches!(outcome, StopOutcome::NotRunning));
}

#[test]
fn stop_active_run_pid_unknown_when_the_record_has_no_pid_yet() {
    let dir = tempfile::tempdir().unwrap();
    let runs_dir = dir.path().join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let entry_path = runs_dir.join("1.lock");
    let _held = tfsapp_core::process::try_lock_file(&entry_path)
        .unwrap()
        .unwrap();
    std::fs::write(&entry_path, format_run_entry("mcp-serve", None)).unwrap();

    let outcome = stop_active_run(dir.path(), "test-identifier").unwrap();
    assert!(matches!(outcome, StopOutcome::PidUnknown { alias } if alias == "mcp-serve"));
}

#[test]
fn stop_active_run_stops_a_matching_child_and_frees_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let runs_dir = dir.path().join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let entry_path = runs_dir.join("1.lock");
    let identifier = "test-identifier";
    let mut holder = spawn_run_entry_holder(&entry_path, identifier);
    let pid = holder.id();
    std::fs::write(&entry_path, format_run_entry("mcp-serve", Some(pid))).unwrap();
    // Started before `stop_active_run`, blocked in `wait()` on the still-alive
    // holder. Since plan 014 it is no longer what keeps `terminate` from
    // escalating — its poll stopped counting an unreaped zombie as a live
    // process — but it is still what keeps the guard honest: the identity
    // check `stop_active_run` makes reads `/proc/<pid>/environ`, which a
    // corpse no longer has, so the holder must be reaped only once the signal
    // has actually been sent, never before. It also leaves no zombie behind
    // for the rest of the suite.
    let reaper = std::thread::spawn(move || {
        let _ = holder.wait();
    });

    let outcome = stop_active_run(dir.path(), identifier).unwrap();
    reaper.join().unwrap();

    assert!(matches!(outcome, StopOutcome::Stopped { alias } if alias == "mcp-serve"));
    assert!(!tfsapp_core::process::process_exists(pid));
}

#[test]
fn stop_active_run_stops_an_active_orphan() {
    let dir = tempfile::tempdir().unwrap();
    let runs_dir = dir.path().join("runs");
    std::fs::create_dir_all(&runs_dir).unwrap();
    let entry_path = runs_dir.join("1.lock");
    let identifier = "test-identifier";
    // A real, separate process carrying `identifier` in its own environment,
    // standing in for the orphaned child — `stop_active_run`'s orphan branch
    // terminates the *recorded* pid directly, never a launcher.
    let mut orphan = std::process::Command::new("sleep")
        .arg("30")
        .env("TFS_APP_IDENTIFIER", identifier)
        .spawn()
        .unwrap();
    let pid = orphan.id();
    std::fs::write(&entry_path, format_run_entry("mcp-serve", Some(pid))).unwrap();

    let outcome = stop_active_run(dir.path(), identifier).unwrap();
    let _ = orphan.wait();

    assert!(matches!(outcome, StopOutcome::StoppedOrphan { alias } if alias == "mcp-serve"));
    assert!(!tfsapp_core::process::process_exists(pid));
    assert!(
        !entry_path.exists(),
        "the orphan's entry is removed once stopped"
    );
}

// --- `runs/` entries: format_run_entry / parse_run_entry / run_entry_file_name

#[test]
fn format_run_entry_alias_alone_or_with_a_pid() {
    assert_eq!(format_run_entry("mcp-serve", None), "mcp-serve");
    assert_eq!(format_run_entry("mcp-serve", Some(1234)), "mcp-serve\n1234");
}

#[test]
fn parse_run_entry_round_trips_through_format_run_entry() {
    let entry = parse_run_entry(&format_run_entry("mcp-serve", Some(1234))).unwrap();
    assert_eq!(
        entry,
        RunEntry {
            alias: "mcp-serve".to_string(),
            pid: Some(1234),
        }
    );
    let entry = parse_run_entry(&format_run_entry("mcp-serve", None)).unwrap();
    assert_eq!(
        entry,
        RunEntry {
            alias: "mcp-serve".to_string(),
            pid: None,
        }
    );
    assert!(parse_run_entry("").is_none());
}

#[test]
fn run_entry_file_name_is_the_launcher_pid() {
    assert_eq!(run_entry_file_name(4321), "4321.lock");
}

// --- run_entry_status --------------------------------------------------------

#[test]
fn run_entry_status_live_launcher_reads_the_record_best_effort() {
    let record = RunEntry {
        alias: "mcp-serve".to_string(),
        pid: Some(1234),
    };
    assert_eq!(
        run_entry_status(false, Some(&record), false, false),
        RunEntryStatus::LiveLauncher {
            alias: Some("mcp-serve".to_string()),
            pid: Some(1234),
        }
    );
}

#[test]
fn run_entry_status_live_launcher_with_no_record_yet() {
    assert_eq!(
        run_entry_status(false, None, false, false),
        RunEntryStatus::LiveLauncher {
            alias: None,
            pid: None,
        }
    );
}

#[test]
fn run_entry_status_active_orphan_needs_flock_free_pid_alive_and_identity_match() {
    let record = RunEntry {
        alias: "mcp-serve".to_string(),
        pid: Some(1234),
    };
    assert_eq!(
        run_entry_status(true, Some(&record), true, true),
        RunEntryStatus::ActiveOrphan {
            alias: "mcp-serve".to_string(),
            pid: 1234,
        }
    );
}

#[test]
fn run_entry_status_stale_when_pid_dead_identity_mismatched_or_no_record() {
    let record = RunEntry {
        alias: "mcp-serve".to_string(),
        pid: Some(1234),
    };
    assert_eq!(
        run_entry_status(true, Some(&record), false, true),
        RunEntryStatus::Stale
    );
    assert_eq!(
        run_entry_status(true, Some(&record), true, false),
        RunEntryStatus::Stale
    );
    assert_eq!(
        run_entry_status(true, None, false, false),
        RunEntryStatus::Stale
    );
    let no_pid = RunEntry {
        alias: "mcp-serve".to_string(),
        pid: None,
    };
    assert_eq!(
        run_entry_status(true, Some(&no_pid), false, false),
        RunEntryStatus::Stale
    );
}

// --- run_start_verdict --------------------------------------------------------

fn active_run(alias: &str, concurrent: bool) -> ActiveRun {
    ActiveRun {
        alias: alias.to_string(),
        pid: Some(1),
        concurrent,
    }
}

#[test]
fn run_start_verdict_may_start_when_nothing_is_active() {
    assert_eq!(run_start_verdict(false, &[]), RunStartVerdict::MayStart);
    assert_eq!(run_start_verdict(true, &[]), RunStartVerdict::MayStart);
}

#[test]
fn run_start_verdict_non_concurrent_newcomer_refuses_beside_anything_active() {
    let active = [active_run("mcp-serve", true)];
    assert_eq!(
        run_start_verdict(false, &active),
        RunStartVerdict::Blocked {
            blocker: active[0].clone(),
            newcomer_non_concurrent: true,
        }
    );
}

#[test]
fn run_start_verdict_concurrent_newcomer_stacks_beside_concurrent_actives() {
    let active = [active_run("mcp-serve", true), active_run("mcp-serve", true)];
    assert_eq!(run_start_verdict(true, &active), RunStartVerdict::MayStart);
}

#[test]
fn run_start_verdict_concurrent_newcomer_refused_by_a_non_concurrent_active() {
    let active = [active_run("mcp-serve", true), active_run("cleanup", false)];
    assert_eq!(
        run_start_verdict(true, &active),
        RunStartVerdict::Blocked {
            blocker: active[1].clone(),
            newcomer_non_concurrent: false,
        }
    );
}

// --- launch_verdict ------------------------------------------------------------

#[test]
fn launch_verdict_opens_beside_concurrent_commands_when_nothing_to_run() {
    let active = [active_run("mcp-serve", true)];
    assert_eq!(launch_verdict(false, &active), LaunchVerdict::MayOpen);
}

#[test]
fn launch_verdict_refuses_a_non_concurrent_active_when_nothing_to_run() {
    let active = [active_run("cleanup", false)];
    assert_eq!(
        launch_verdict(false, &active),
        LaunchVerdict::Refuse {
            blocker: active[0].clone(),
        }
    );
}

#[test]
fn launch_verdict_refuses_any_active_command_when_an_event_is_pending() {
    let active = [active_run("mcp-serve", true)];
    assert_eq!(
        launch_verdict(true, &active),
        LaunchVerdict::Refuse {
            blocker: active[0].clone(),
        }
    );
}

#[test]
fn launch_verdict_may_open_with_nothing_active_regardless_of_the_event() {
    assert_eq!(launch_verdict(false, &[]), LaunchVerdict::MayOpen);
    assert_eq!(launch_verdict(true, &[]), LaunchVerdict::MayOpen);
}

// --- format_alias_list --------------------------------------------------------

#[test]
fn format_alias_list_empty_message_when_no_aliases() {
    assert_eq!(
        format_alias_list(&BTreeMap::new()),
        "no run aliases are declared by this app"
    );
}

#[test]
fn format_alias_list_sorted_and_annotated() {
    let mut aliases = BTreeMap::new();
    aliases.insert(
        "zeta".to_string(),
        RunAlias {
            command: "app:zeta".to_string(),
            concurrent: false,
        },
    );
    aliases.insert(
        "alpha".to_string(),
        RunAlias {
            command: "app:alpha".to_string(),
            concurrent: true,
        },
    );
    let formatted = format_alias_list(&aliases);
    assert_eq!(
        formatted,
        "  alpha -> app:alpha (concurrent)\n  zeta -> app:zeta (standalone-only)"
    );
}
