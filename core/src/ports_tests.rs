use super::*;

// --- resolve_packaged_port ----------------------------------------------

#[test]
fn resolve_packaged_port_none_without_app_port() {
    let data_subdir = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_packaged_port(None, data_subdir.path()).unwrap(),
        None
    );
}

#[test]
fn resolve_packaged_port_uses_app_port_without_config_json() {
    let data_subdir = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_packaged_port(Some(8080), data_subdir.path()).unwrap(),
        Some(8080)
    );
}

#[test]
fn resolve_packaged_port_override_takes_precedence() {
    let data_subdir = tempfile::tempdir().unwrap();
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{"version":"1.0.0","port_override":9090}"#,
    )
    .unwrap();
    assert_eq!(
        resolve_packaged_port(Some(8080), data_subdir.path()).unwrap(),
        Some(9090)
    );
}

#[test]
fn resolve_packaged_port_override_ignored_without_app_port() {
    let data_subdir = tempfile::tempdir().unwrap();
    fs::write(
        data_subdir.path().join("config.json"),
        r#"{"version":"1.0.0","port_override":9090}"#,
    )
    .unwrap();
    assert_eq!(
        resolve_packaged_port(None, data_subdir.path()).unwrap(),
        None
    );
}

#[test]
fn resolve_packaged_port_falls_back_to_app_port_on_malformed_config_json() {
    let data_subdir = tempfile::tempdir().unwrap();
    fs::write(data_subdir.path().join("config.json"), "{ not json").unwrap();
    assert_eq!(
        resolve_packaged_port(Some(8080), data_subdir.path()).unwrap(),
        Some(8080)
    );
}

// --- check_packaged_port --------------------------------------------------
//
// Plan 031: the guard binds the port the caller already resolved instead of
// resolving one itself. The station's tests built a minimal `PackagedEnv` to
// carry that port and the data subdir; here those two are the parameters, so
// each test passes them directly (see the port note on the function itself).

#[test]
fn check_packaged_port_noop_in_dynamic_mode_even_if_port_is_taken() {
    let data_subdir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_port = listener.local_addr().unwrap().port();

    assert!(check_packaged_port(None, taken_port, data_subdir.path()).is_ok());
}

#[test]
fn check_packaged_port_ok_when_resolved_port_is_free() {
    let data_subdir = tempfile::tempdir().unwrap();
    let free_port = pick_free_local_port().unwrap();

    assert!(check_packaged_port(Some(free_port), free_port, data_subdir.path()).is_ok());
}

#[test]
fn check_packaged_port_errors_naming_port_and_config_json_when_resolved_port_is_taken() {
    let data_subdir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_port = listener.local_addr().unwrap().port();

    let error = check_packaged_port(Some(taken_port), taken_port, data_subdir.path()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains(&taken_port.to_string()));
    assert!(message.contains("port_override"));
    assert!(message.contains(&data_subdir.path().join("config.json").display().to_string()));
}
