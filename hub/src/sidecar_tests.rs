use std::{
    fs,
    process::Command,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

use super::{worker_needs_termination, Sidecar};

#[test]
fn dropping_a_sidecar_reaps_its_server_and_removes_its_pid_file() {
    let data_dir = tempfile::tempdir().expect("a temp data dir");
    let pid_file = data_dir.path().join("sidecar.pid");
    let mut command = Command::new("sleep");
    command.arg("60");
    tfsapp_core::process::set_own_process_group(&mut command);
    let server = command.spawn().expect("a throwaway server");
    let server_pid = server.id();
    fs::write(&pid_file, format!("{server_pid}\n")).expect("a server pid file");

    let sidecar = Sidecar {
        server: Some(server),
        worker: Arc::new(Mutex::new(None)),
        shutting_down: Arc::new(AtomicBool::new(false)),
        pid_file: pid_file.clone(),
        lock: None,
        serving: None,
    };

    drop(sidecar);

    assert!(
        !tfsapp_core::process::process_exists(server_pid),
        "dropping Sidecar must reap its server"
    );
    assert!(
        !pid_file.exists(),
        "dropping Sidecar must remove its pid file"
    );
}

#[test]
fn only_a_live_worker_is_signalled_during_teardown() {
    let mut reaped = Command::new("true").spawn().expect("a short-lived worker");
    reaped.wait().expect("reap the short-lived worker");
    assert!(
        !worker_needs_termination(&mut reaped),
        "a worker the supervisor already reaped must not be signalled"
    );

    let mut command = Command::new("sleep");
    command.arg("60");
    tfsapp_core::process::set_own_process_group(&mut command);
    let mut live = command.spawn().expect("a live worker");
    let live_pid = live.id();
    assert!(
        worker_needs_termination(&mut live),
        "a still-running worker must be signalled"
    );
    tfsapp_core::process::terminate(live_pid);
    live.wait().expect("reap the stopped live worker");
    assert!(
        !tfsapp_core::process::process_exists(live_pid),
        "the live worker must have received the termination signal"
    );
}
