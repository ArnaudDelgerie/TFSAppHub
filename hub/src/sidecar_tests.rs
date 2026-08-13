use std::{
    fs,
    process::Command,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

use super::Sidecar;

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
