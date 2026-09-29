use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

/// The FrankenPHP sidecar: `make sidecar`'s download in dev, a bundled Tauri
/// resource in packaged mode; a system-wide install is the fallback either
/// way. `candidates` is tried in order — the caller's job is to put the
/// packaged path first and the dev path last — and the error names every path
/// that was tried, including the final system-wide one.
pub fn resolve_frankenphp_binary(
    candidates: &[PathBuf],
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    for candidate in candidates {
        if is_executable(candidate) {
            return Ok(candidate.clone());
        }
    }
    let usr_bin = PathBuf::from("/usr/bin/frankenphp");
    if usr_bin.is_file() {
        return Ok(usr_bin);
    }
    let mut tried: Vec<String> = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    tried.push(usr_bin.display().to_string());
    Err(format!(
        "FrankenPHP sidecar not found. Run `make sidecar` (tried {}).",
        tried.join(", ")
    )
    .into())
}

/// True when `path` is a real, non-empty file — the presence check every
/// bundled resource (packaged, dev, or a `build.rs` stub) has to pass before
/// it is trusted. A stub written only so a fresh clone compiles before
/// `make resources` has ever run must never be mistaken for the real
/// download.
pub fn is_present(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    is_present(path)
        && fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    is_present(path)
}

pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Environment pairs for the processes the hub starts. The value is raw
/// bytes (`OsString`), not a lossy string: a path that is not valid UTF-8
/// reaches the child as itself, never as a different path wearing the
/// replacement character (decision 008 — omit rather than a wrong path; the
/// hub instead passes the right bytes).
pub fn command_with_env(binary: &Path, envs: &[(&str, OsString)]) -> Command {
    let mut command = Command::new(binary);
    for (key, value) in envs {
        command.env(key, value);
    }
    command
}

/// `chmod 0755` on a just-downloaded AppImage (`update`'s `--update` and
/// `rollback`'s `--rollback` flows): a `browser_download_url` stream arrives
/// without the executable bit, and the swapped-in file must be launchable
/// exactly like the one it replaces.
pub fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(test)]
#[path = "sidecar_tests.rs"]
mod tests;
