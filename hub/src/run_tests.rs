use std::{collections::BTreeMap, path::Path, time::Duration};

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

// --- format_run_lock / parse_run_lock ---------------------------------------

#[test]
fn format_run_lock_alias_only_without_pid() {
    assert_eq!(format_run_lock("mcp-serve", None), "mcp-serve");
}

#[test]
fn format_run_lock_alias_and_pid() {
    assert_eq!(format_run_lock("mcp-serve", Some(1234)), "mcp-serve\n1234");
}

#[test]
fn parse_run_lock_none_for_empty_content() {
    assert!(parse_run_lock("").is_none());
}

#[test]
fn parse_run_lock_legacy_one_line_form_has_no_pid() {
    let record = parse_run_lock("mcp-serve").expect("parsed");
    assert_eq!(record.alias, "mcp-serve");
    assert_eq!(record.pid, None);
}

#[test]
fn parse_run_lock_two_line_form_has_pid() {
    let record = parse_run_lock("mcp-serve\n1234").expect("parsed");
    assert_eq!(record.alias, "mcp-serve");
    assert_eq!(record.pid, Some(1234));
}

#[test]
fn parse_run_lock_tolerates_trailing_newline() {
    let record = parse_run_lock("mcp-serve\n1234\n").expect("parsed");
    assert_eq!(record.alias, "mcp-serve");
    assert_eq!(record.pid, Some(1234));
}

#[test]
fn parse_run_lock_non_numeric_second_line_is_no_pid_not_an_error() {
    let record = parse_run_lock("mcp-serve\nnot-a-pid").expect("parsed");
    assert_eq!(record.alias, "mcp-serve");
    assert_eq!(record.pid, None);
}

// --- orphaned_run_decision / probe_orphaned_run -----------------------------

#[test]
fn orphaned_run_decision_is_active_only_for_a_live_identity_proven_pid() {
    let record = RunLockRecord {
        alias: "mcp-serve".to_string(),
        pid: Some(1234),
    };
    assert_eq!(
        orphaned_run_decision(Some(&record), true, true),
        OrphanedRun::ActiveOrphan {
            alias: "mcp-serve".to_string(),
            pid: 1234,
        }
    );
    assert_eq!(
        orphaned_run_decision(Some(&record), false, true),
        OrphanedRun::Stale
    );
    assert_eq!(
        orphaned_run_decision(Some(&record), true, false),
        OrphanedRun::Stale
    );
    assert_eq!(
        orphaned_run_decision(None, false, false),
        OrphanedRun::Stale
    );
}

#[test]
fn probe_orphaned_run_finds_a_live_identity_proven_child_then_stale_after_reaping() {
    let dir = tempfile::tempdir().unwrap();
    let run_lock_path = dir.path().join("run.lock");
    let identifier = "test-identifier";
    let mut child = spawn_run_lock_holder(&run_lock_path, identifier);
    let pid = child.id();
    std::fs::write(&run_lock_path, format_run_lock("mcp-serve", Some(pid))).unwrap();

    assert_eq!(
        probe_orphaned_run(&run_lock_path, identifier),
        OrphanedRun::ActiveOrphan {
            alias: "mcp-serve".to_string(),
            pid,
        }
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        probe_orphaned_run(&run_lock_path, identifier),
        OrphanedRun::Stale
    );
}

#[test]
fn probe_orphaned_run_ignores_a_live_child_with_the_wrong_identifier() {
    let dir = tempfile::tempdir().unwrap();
    let run_lock_path = dir.path().join("run.lock");
    let mut child = spawn_run_lock_holder(&run_lock_path, "another-app");
    std::fs::write(
        &run_lock_path,
        format_run_lock("mcp-serve", Some(child.id())),
    )
    .unwrap();

    assert_eq!(
        probe_orphaned_run(&run_lock_path, "test-identifier"),
        OrphanedRun::Stale
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

// --- run_start_guard_decision -----------------------------------------------

#[test]
fn run_start_guard_decision_refuses_a_live_orphan_after_acquiring_the_flock() {
    assert_eq!(
        run_start_guard_decision(
            true,
            OrphanedRun::ActiveOrphan {
                alias: "mcp-serve".to_string(),
                pid: 1234,
            },
        ),
        RunStartGuard::ActiveOrphan {
            alias: "mcp-serve".to_string(),
            pid: 1234,
        }
    );
}

#[test]
fn run_start_guard_decision_allows_stale_records_and_refuses_held_flocks() {
    assert_eq!(
        run_start_guard_decision(true, OrphanedRun::Stale),
        RunStartGuard::MayStart
    );
    assert_eq!(
        run_start_guard_decision(false, OrphanedRun::Stale),
        RunStartGuard::ActiveLauncher
    );
}

// --- stop_outcome_message / stop_outcome_succeeded --------------------------

#[test]
fn stop_outcome_message_not_running() {
    let path = Path::new("/data/run.lock");
    assert_eq!(
        stop_outcome_message(&StopOutcome::NotRunning, path),
        "no run command is currently active"
    );
}

#[test]
fn stop_outcome_message_stopped_names_the_alias() {
    let path = Path::new("/data/run.lock");
    let outcome = StopOutcome::Stopped {
        alias: "mcp-serve".to_string(),
    };
    assert_eq!(
        stop_outcome_message(&outcome, path),
        "stopped the active run command \"mcp-serve\""
    );
}

#[test]
fn stop_outcome_message_stopped_orphan_names_the_alias_and_gone_launcher() {
    let path = Path::new("/data/run.lock");
    let outcome = StopOutcome::StoppedOrphan {
        alias: "mcp-serve".to_string(),
    };
    let message = stop_outcome_message(&outcome, path);
    assert!(message.contains("mcp-serve"));
    assert!(message.contains("launcher had already gone"));
}

#[test]
fn stop_outcome_message_pid_unknown_names_the_alias_and_suggests_a_retry() {
    let path = Path::new("/data/run.lock");
    let outcome = StopOutcome::PidUnknown {
        alias: "mcp-serve".to_string(),
    };
    let message = stop_outcome_message(&outcome, path);
    assert!(message.contains("mcp-serve"));
    assert!(message.contains("retry"));
}

#[test]
fn stop_outcome_message_lock_held_names_the_alias_pid_and_lock_path() {
    let path = Path::new("/data/run.lock");
    let outcome = StopOutcome::LockHeld {
        alias: "mcp-serve".to_string(),
        pid: 4321,
    };
    let message = stop_outcome_message(&outcome, path);
    assert!(message.contains("mcp-serve"));
    assert!(message.contains("4321"));
    assert!(message.contains("/data/run.lock"));
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
        pid: 4321
    }));
    assert!(!stop_outcome_succeeded(&StopOutcome::OrphanStillRunning {
        alias: "mcp-serve".to_string(),
        pid: 4321
    }));
}

// --- stop_active_run ---------------------------------------------------------

/// Spawn a controlled child that holds `run_lock_path`'s exclusive flock — a
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
fn spawn_run_lock_holder(run_lock_path: &Path, identifier: &str) -> std::process::Child {
    let child = std::process::Command::new("sh")
        .args([
            "-c",
            r#"exec 9>"$1"; flock -n 9 || exit 1; exec sleep 30"#,
            "sh",
        ])
        .arg(run_lock_path)
        .env("TFS_APP_IDENTIFIER", identifier)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        if tfsapp_core::process::try_lock_file(run_lock_path)
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
fn stop_active_run_not_running_when_the_lock_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = stop_active_run(dir.path(), "test-identifier").unwrap();
    assert!(matches!(outcome, StopOutcome::NotRunning));
}

#[test]
fn stop_active_run_pid_unknown_when_the_record_has_no_pid_yet() {
    let dir = tempfile::tempdir().unwrap();
    let run_lock_path = dir.path().join("run.lock");
    let _held = tfsapp_core::process::try_lock_file(&run_lock_path)
        .unwrap()
        .unwrap();
    std::fs::write(&run_lock_path, format_run_lock("mcp-serve", None)).unwrap();

    let outcome = stop_active_run(dir.path(), "test-identifier").unwrap();
    assert!(matches!(outcome, StopOutcome::PidUnknown { alias } if alias == "mcp-serve"));
}

#[test]
fn stop_active_run_stops_a_matching_child_and_frees_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let run_lock_path = dir.path().join("run.lock");
    let identifier = "test-identifier";
    let mut holder = spawn_run_lock_holder(&run_lock_path, identifier);
    let pid = holder.id();
    std::fs::write(&run_lock_path, format_run_lock("mcp-serve", Some(pid))).unwrap();
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
