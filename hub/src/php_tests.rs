use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use super::write_shim;

/// A stand-in for FrankenPHP that prints the arguments it was handed, so a test
/// can read what the shim forwarded without a real interpreter.
fn fake_frankenphp(path: &std::path::Path) {
    fs::write(
        path,
        "#!/bin/sh\nfor arg in \"$@\"; do echo \"$arg\"; done\n",
    )
    .expect("a fake interpreter");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("+x");
}

fn shim_output(arguments: &[&str], interpreter_name: &str) -> String {
    let root = tempfile::tempdir().expect("a temp dir");
    let interpreter = root.path().join(interpreter_name);
    fake_frankenphp(&interpreter);
    let shim = root.path().join("bin/php");
    write_shim(&shim, &interpreter).expect("a written shim");

    let output = Command::new(&shim)
        .args(arguments)
        .output()
        .expect("the shim runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_shim_re_enters_the_bundled_interpreter() {
    // The whole point: anything an app's Composer or console spawns has to land
    // on the interpreter the hub bundles, never on a host PHP.
    let output = shim_output(&["bin/console", "about"], "frankenphp");

    assert_eq!(output, "php-cli\nbin/console\nabout\n");
}

#[test]
fn the_shim_drops_the_ini_options_frankenphp_cannot_take() {
    // Measured 2026-08-07 on the pinned sidecar: `frankenphp php-cli` accepts a
    // script or `-r` and no PHP CLI options at all — it reads `-d` as a
    // filename and dies with "Failed opening required '-d'". Composer appends
    // three of them to every `@php` it spawns, so a shim that forwarded them
    // would fail on the very first script an app declares.
    let output = shim_output(
        &[
            "-d",
            "allow_url_fopen=1",
            "-d",
            "memory_limit=-1",
            "-dopcache.enable=0",
            "bin/console",
            "cache:warmup",
        ],
        "frankenphp",
    );

    assert_eq!(output, "php-cli\nbin/console\ncache:warmup\n");
}

#[test]
fn a_flag_the_app_meant_for_its_own_command_is_forwarded() {
    // The dropping stops at the first word that is not one of ours: everything
    // after the script belongs to the app, `-d`-looking or not.
    let output = shim_output(&["bin/console", "app:run", "-d", "x"], "frankenphp");

    assert_eq!(output, "php-cli\nbin/console\napp:run\n-d\nx\n");
}

#[test]
fn an_interpreter_path_with_a_space_still_runs() {
    // It lives under the user's home directory, which is not the hub's to
    // assume anything about.
    let output = shim_output(&["-r", "echo 1;"], "franken php");

    assert_eq!(output, "php-cli\n-r\necho 1;\n");
}
