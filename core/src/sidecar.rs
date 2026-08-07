use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

/// The FrankenPHP sidecar: `make sidecar`'s download in dev, a bundled Tauri
/// resource in packaged mode; a system-wide install is the fallback either way.
pub fn resolve_frankenphp_binary(bundled: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if is_executable(bundled) {
        return Ok(bundled.to_path_buf());
    }
    let usr_bin = PathBuf::from("/usr/bin/frankenphp");
    if usr_bin.is_file() {
        return Ok(usr_bin);
    }
    Err(format!(
        "FrankenPHP sidecar not found. Run `make sidecar` (tried {} and {}).",
        bundled.display(),
        usr_bin.display()
    )
    .into())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn command_with_env(binary: &Path, envs: &[(&str, String)]) -> Command {
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
