use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// Size cap that triggers rotation (plan 038). Checked once per launch,
/// never mid-run — see `rotate_logs`.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// How many older `.N` generations survive a rotation (plan 038) before the
/// oldest is dropped.
pub const LOG_GENERATIONS: u32 = 3;

/// Best-effort append to `log_file`, creating its parent dir and the file
/// itself as needed. Never fails the caller — losing a log line is not
/// worth aborting a lifecycle command or a worker restart over, unlike the
/// event it's recording. Shared by the lifecycle command runner
/// (`commands.log`) and the worker supervisor (`sidecar.log`, plan 032).
pub fn append_log(log_file: &Path, content: &str) {
    if content.is_empty() {
        return;
    }
    if let Some(parent) = log_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_file) {
        let _ = file.write_all(content.as_bytes());
        if !content.ends_with('\n') {
            let _ = file.write_all(b"\n");
        }
    }
}

/// Two independent `Stdio` handles onto `path` — one for stdout, one for
/// stderr — from the same `File`, opened for append and created if missing.
/// Both still append to the same underlying file (`try_clone` shares the OS
/// file description, and therefore the `O_APPEND` file offset, not just the
/// fd number), so a caller can hand one to each without fighting over a
/// single owned value. Shared by the sidecar (`sidecar_log_stdio`, below) and
/// by `open`'s own detached child (plan 015), whose routine output has no
/// terminal to land on once the parent has returned.
pub fn append_stdio(path: &Path) -> std::io::Result<(std::process::Stdio, std::process::Stdio)> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let stdout = std::process::Stdio::from(file.try_clone()?);
    let stderr = std::process::Stdio::from(file);
    Ok((stdout, stderr))
}

/// Packaged mode only (plan 031): `<data dir>/log/sidecar.log`, appended to,
/// never truncated — the same shape as `commands.log`, for the same reason
/// (multiple launches, and the server plus a recycled Messenger worker, must
/// stay comparable in one file rather than each clobbering the last). Dev
/// mode never calls this — it keeps inheriting the developer's own terminal
/// stdio, same as before this plan.
pub fn sidecar_log_stdio(
    log_dir: &Path,
) -> std::io::Result<(std::process::Stdio, std::process::Stdio)> {
    append_stdio(&log_dir.join("sidecar.log"))
}

/// `path.{generation}` — the sibling rotation uses for `path`'s older
/// generations (`sidecar.log.1`, `sidecar.log.2`, …).
fn generation_path(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

/// Best-effort, size-based rotation of a single log file (plan 038). A
/// no-op when `path` is missing or under `MAX_LOG_BYTES`. Otherwise shifts
/// `path.{N-1}` to `path.{N}` down to `LOG_GENERATIONS` — dropping whatever
/// already sat at the oldest generation — and renames the live file to
/// `path.1`, freeing `path` for the caller's next `append_log`/
/// `sidecar_log_stdio` open to start fresh. Never panics: an unwritable
/// `log/` directory just leaves the oversized file in place, the same
/// "losing a log line is not worth aborting a launch over" philosophy as
/// `append_log`.
pub fn rotate_log(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() < MAX_LOG_BYTES {
        return;
    }
    for generation in (1..LOG_GENERATIONS).rev() {
        let from = generation_path(path, generation);
        if from.is_file() {
            let _ = fs::rename(&from, generation_path(path, generation + 1));
        }
    }
    let _ = fs::rename(path, generation_path(path, 1));
}

/// Rotate both of `log_dir`'s logs (`commands.log`, `sidecar.log`) once,
/// called early in both launch paths — before `commands.log` is written and
/// before `sidecar_log_stdio` opens the sidecar's fd (plan 038) — so a
/// long-lived install's `log/` stays bounded instead of growing forever.
///
/// **`hub.log` is deliberately not among them (plan 015).** This runs in the
/// *child*, after the *parent* has already opened `hub.log`'s fd for the
/// child's own stdio; renaming it out from under that open `O_APPEND` handle
/// here would silently send the whole session's lines into `hub.log.1`
/// instead of `hub.log`. The parent rotates `hub.log` itself, with a plain
/// `rotate_log` call, before it opens that fd — see `open::prepare_hub_log`.
pub fn rotate_logs(log_dir: &Path) {
    rotate_log(&log_dir.join("commands.log"));
    rotate_log(&log_dir.join("sidecar.log"));
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
