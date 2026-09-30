//! `tfsapp-hub open <id>` — give an installed app its own window.
//!
//! **One OS process per open app.** `open <id>` resolves the app, then
//! re-executes the hub binary as a child carrying that app's identity; the child
//! is the one that mutates its runtime identity (see `identity.rs`), boots the
//! app's sidecar and opens the window. The parent's only job is to resolve, to
//! fail fast in a way **both** a terminal user and a desktop-entry launch can
//! act on (plan 015 — see [`crate::lifecycle::report_launch_failure`]), and to
//! get out of the way.
//!
//! **The child's stdio is redirected, not inherited (plan 015).** The parent
//! returns as soon as it has the pid, so a terminal that ran `open` has
//! already moved on by the time the child has anything routine to say; its
//! stdout and stderr go to `<state_root>/log/hub.log` instead (`launch`,
//! below). `dev` and `run` are the two paths where a terminal genuinely stays
//! attached for the child's whole life, and inheritance is still right there.
//!
//! It is not one process serving N apps, and that was decided rather than
//! defaulted, for three reasons, in the order they bite:
//!
//! - every isolation guarantee of CONTRACT.md §5 holds verbatim, because it is
//!   the same shape the station already has: one process, one identifier, one
//!   data dir, one cookie store;
//! - a segfault in one app cannot take the whole suite down with it;
//! - the station's lifecycle, sidecar, worker and window code keeps working per
//!   process essentially unmodified.
//!
//! The cost — each app pays its own FrankenPHP startup — is the cost today. The
//! size win comes from sharing the binary on disk, never from sharing the
//! process.
//!
//! **The parent does not hold the shell.** It spawns and returns, so `open` from
//! a terminal gives the prompt straight back and a `.desktop` entry (plan 009)
//! does not leave a launcher process hanging around the window's lifetime. The
//! child gets its own process group, which is what lets its whole descendant
//! tree — FrankenPHP, its PHP workers, a Messenger worker — be signalled as one
//! (see `core`'s `set_own_process_group`).

use std::{fmt, io, path::PathBuf, process::Command};

use tfsapp_core::sidecar::path_to_string;

use crate::{
    cli::{EXIT_FAILED, EXIT_OK, OPEN_CHILD_SUBCOMMAND},
    launch::{LaunchSpec, Source},
    lifecycle::CacheStamp,
    manifest::{self, ManifestError},
    paths::{Paths, PathsError},
    registry::{self, now_timestamp, RegistryError, State},
    update_check,
};

/// The whole command, parent side. Returns the process's exit code.
///
/// Every refusal below goes through [`lifecycle::report_launch_failure`], the
/// same reporter the child's own fatal errors use: a line on stderr always,
/// and a native dialog too when stderr is not a terminal, because a launch
/// started from a desktop entry has no terminal for the line to land on (see
/// this module's doc comment's table).
///
/// A file-bearing invocation refuses *here*, before the child is detached:
/// the receiver capability is the manifest's promise that the app can ever
/// acknowledge what it is handed, and a batch that fails validation names the
/// offending path rather than launching a window the files never reach.
pub fn run(id: &str, files: &[String]) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            crate::lifecycle::report_launch_failure(&error.to_string());
            return EXIT_FAILED;
        }
    };

    let launched = resolve(&paths, id).and_then(|resolved| {
        for warning in &resolved.warnings {
            eprintln!("tfsapp-hub: warning: {warning}");
        }
        if let Some(platform) = &resolved.pending_revalidation {
            println!(
                "Revalidating {id} behind the splash — it was installed against PHP {platform}; \
                 follow hub.log."
            );
        }
        if !files.is_empty() {
            let receiver = crate::open_files::Receiver::of(&resolved.manifest);
            if !receiver.declared {
                return Err(OpenError::NoReceiver { id: id.to_string() });
            }
            let batch = absolute_paths(files)?;
            crate::open_files::validate_batch(&batch, receiver.directories)
                .map_err(OpenError::InvalidBatch)?;
            launch(&paths, &resolved, &batch)
        } else {
            launch(&paths, &resolved, &[])
        }
    });

    match launched {
        Ok(pid) => {
            println!("Opening {id} (pid {pid}).");
            EXIT_OK
        }
        Err(error) => {
            crate::lifecycle::report_launch_failure(&error.to_string());
            EXIT_FAILED
        }
    }
}

/// The installed constructor of [`LaunchSpec`]: look the app up and check it
/// can be opened at all.
///
/// The manifest is loaded from the **installed snapshot**, never from the
/// original source: what runs is what was installed, and a source tree edited
/// since changes nothing until `update <id>`. That is the same rule the
/// installer states, read from the other end.
///
/// Every refusal below names the app and the way out, because the audience is
/// someone at a terminal who typed one word and got nothing. The one state
/// that is *not* an immediate refusal is `needs-revalidation`: the app's
/// dependencies were resolved against a PHP that has since moved. The spec
/// carries that pending work into the child, where it runs behind the splash
/// after the launch locks are held, rather than making the parent wait through
/// a fresh `composer install`. `Broken`, already recorded from an earlier
/// attempt, is refused without retrying: nothing has changed since it failed,
/// and retrying it on every `open` would only repeat the same minutes of work
/// for the same answer.
pub fn resolve(paths: &Paths, id: &str) -> Result<LaunchSpec, OpenError> {
    let installed = registry::load(paths)?;
    let entry = installed
        .get(id)
        .ok_or_else(|| OpenError::NotInstalled { id: id.to_string() })?;

    if entry.state == State::Broken {
        return Err(OpenError::Broken {
            id: id.to_string(),
            platform: entry.platform.to_string(),
        });
    }
    let pending_revalidation =
        (entry.state == State::NeedsRevalidation).then(|| entry.platform.clone());

    let app_dir = paths.app_dir(id)?;
    if !app_dir.is_dir() {
        return Err(OpenError::NoSnapshot {
            id: id.to_string(),
            path: app_dir,
        });
    }

    let loaded = manifest::load(&app_dir)?;
    let manifest = loaded.manifest;

    // The registry's `identifier` and the snapshot's must agree, because they
    // name the same thing from two sides and everything downstream hangs off it:
    // the data dir, the keyring namespace, the window identity, the
    // single-instance key. A disagreement can only come from a tree edited under
    // the hub, and opening on the manifest's value would quietly point the app
    // at a different data dir than the one `list` and `remove` name.
    if entry.identifier != manifest.identifier {
        return Err(OpenError::IdentityChanged {
            id: id.to_string(),
            registered: entry.identifier.clone(),
            declared: manifest.identifier.clone(),
        });
    }

    let state_root = paths.app_data_dir(&manifest.identifier)?;
    let identity = manifest.identity(&app_dir);
    // The stamp this launch would write if it rebuilt right now (plan 024) —
    // `app_env::resolve`'s `Mode::Launch` compares the recorded one against
    // this rather than deciding on its own, which has neither a `Platform`
    // nor any notion of the installed snapshot's identity beyond `app_dir`.
    let expected_cache = CacheStamp {
        app_version: manifest.app_version.clone(),
        snapshot_path: path_to_string(&app_dir),
        // Provisional when `pending_revalidation` is `Some`: the serve thread
        // replaces it with the freshly probed platform before comparing or
        // writing this stamp.
        platform: entry.platform.clone(),
    };

    Ok(LaunchSpec {
        source: Source::Installed { id: id.to_string() },
        app_dir,
        identity,
        manifest,
        state_root,
        label: id.to_string(),
        warnings: loaded.warnings,
        update: update_check::Context::Installed {
            source: entry.source.clone(),
            app_version: entry.app_version.clone(),
            cache_path: paths.update_cache_path(),
        },
        expected_cache: Some(expected_cache),
        pending_revalidation,
    })
}

/// The child's argv, after the binary's own name.
///
/// The identity travels as arguments rather than being re-derived by the child,
/// and that is worth a line. Applying it has to happen before anything touches
/// GTK — before the registry is read, before a manifest is parsed, before any
/// error could want a dialog — so the value it needs must be reachable with no
/// I/O at all. `--id` comes along so the child can resolve the rest of the app
/// once it is safely past that point.
///
/// A file-bearing invocation appends its batch after a `--` separator: the
/// paths are already absolute and validated, and they travel as distinct
/// arguments so spaces, Unicode, quotes and option-looking names survive
/// verbatim — no shell, no quoting, no interpolation.
///
/// Only ever called on a [`Source::Installed`] spec — `resolve` above is this
/// module's only constructor, and it never builds a `Live` one.
pub fn child_args(spec: &LaunchSpec, files: &[String]) -> Vec<String> {
    let mut args = vec![
        OPEN_CHILD_SUBCOMMAND.to_string(),
        "--id".to_string(),
        spec.installed_id()
            .expect("open's own LaunchSpec is always Source::Installed")
            .to_string(),
        "--identity".to_string(),
        spec.identity.identifier.clone(),
        "--name".to_string(),
        spec.identity.product_name.clone(),
    ];
    if let Some(icon) = &spec.identity.icon_path {
        args.push("--icon".to_string());
        args.push(icon.display().to_string());
    }
    if !files.is_empty() {
        args.push("--".to_string());
        args.extend(files.iter().cloned());
    }
    args
}

/// The batch as the child must see it: every path absolute. A relative path
/// is read against the *originating caller's* working directory — here, in
/// the parent, before the child is detached into a context where that cwd no
/// longer names the person who typed the command. Made absolute, never
/// canonicalized: the request names what the caller named, symlink and `..`
/// included.
fn absolute_paths(files: &[String]) -> Result<Vec<String>, OpenError> {
    let cwd = std::env::current_dir().map_err(OpenError::NoWorkingDirectory)?;
    Ok(files
        .iter()
        .map(|file| {
            if std::path::Path::new(file).is_absolute() {
                file.clone()
            } else {
                cwd.join(file).display().to_string()
            }
        })
        .collect())
}

/// Re-execute this binary as the app's own process, and answer with its pid.
///
/// stdio is **not** inherited: the parent returns as soon as it has the pid
/// (see this module's doc comment's table), so a terminal that ran `open` has
/// already been handed back by the time the child has anything to say —
/// inheriting would write the child's routine lines to a prompt nobody is
/// reading any more. The child's stdout and stderr are redirected to
/// `<state_root>/log/hub.log` instead, appended across launches with a dated
/// header marking where this one starts; stdin is nulled. `dev` is the path
/// where inheritance is still right, because its parent stays in the
/// foreground and waits for the child.
fn launch(paths: &Paths, spec: &LaunchSpec, files: &[String]) -> Result<u32, OpenError> {
    let executable = std::env::current_exe().map_err(OpenError::NoExecutable)?;
    let mut command = Command::new(&executable);
    command.args(child_args(spec, files));
    tfsapp_core::process::set_own_process_group(&mut command);

    let hub_log = prepare_hub_log(paths, &spec.identity.identifier, &mut command);

    let pid = command
        .spawn()
        .map(|child| child.id())
        .map_err(|source| OpenError::Unstartable { executable, source })?;

    if let Some(hub_log) = hub_log {
        tfsapp_core::log::append_log(
            &hub_log,
            &launch_header(&now_timestamp(), "open", &spec.label, pid),
        );
    }

    Ok(pid)
}

/// Points `command`'s stdio at `<state_root>/log/hub.log`, creating the
/// directory and, when this launch will serve, rotating the file first.
/// Returns the log path on success, so the caller can write the launch header
/// to it once the child's pid is known; `None` means every line below already
/// explained itself on stderr, or a live sibling will receive this launch as a
/// hand-off, and `command` was left with its default (inherited) stdio.
///
/// **Best-effort, on purpose.** A launch is not refused, and output is not
/// dropped, just because its log file could not be opened — inherited stdio
/// that nobody is watching is still a better outcome than `/dev/null` (see
/// this module's doc comment's table for who normally reads it instead).
///
/// The directory is created through [`Paths::create_app_data_dir`] rather
/// than a bare `create_dir_all`, so its `0700` (CONTRACT.md §5) is applied by
/// whoever gets there first — this parent, or the child a moment later —
/// with no window in which the app's data directory exists at the umask's
/// permissions. Before rotating, the parent probes the serving lock. A held
/// lock means this child exists only to hand its argv to the live instance, so
/// it must not rename that instance's open `hub.log` fd or add a launch header
/// of its own. When the lock is free, rotation happens here, before the file is
/// opened for the child: `app_env::resolve`'s own `rotate_logs` runs in the
/// child after this fd is already open, and renaming a file out from under an
/// open `O_APPEND` handle would silently send the whole session's lines into
/// `hub.log.1` instead of `hub.log`.
fn prepare_hub_log(paths: &Paths, identifier: &str, command: &mut Command) -> Option<PathBuf> {
    let warn = |context: String, error: &dyn fmt::Display| {
        eprintln!(
            "tfsapp-hub: warning: {context}: {error}; the app's output will go to this \
             terminal instead."
        );
    };

    let data_dir = match paths.create_app_data_dir(identifier) {
        Ok(dir) => dir,
        Err(error) => {
            warn(
                "cannot prepare the app's data directory".to_string(),
                &error,
            );
            return None;
        }
    };
    let log_dir = data_dir.join("log");
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        warn(format!("cannot create {}", log_dir.display()), &error);
        return None;
    }

    match tfsapp_core::process::try_lock_file(&crate::lifecycle::serving_lock_path(&data_dir)) {
        Ok(None) => return None,
        Ok(Some(_)) => {}
        Err(error) => {
            warn(
                "cannot tell whether the app is already serving".to_string(),
                &error,
            );
            return None;
        }
    }

    let hub_log = log_dir.join("hub.log");
    tfsapp_core::log::rotate_log(&hub_log);

    match tfsapp_core::log::append_stdio(&hub_log) {
        Ok((stdout, stderr)) => {
            command
                .stdout(stdout)
                .stderr(stderr)
                .stdin(std::process::Stdio::null());
            Some(hub_log)
        }
        Err(error) => {
            warn(format!("cannot open {}", hub_log.display()), &error);
            None
        }
    }
}

/// The one line `hub.log` gets per launch: an RFC 3339 `timestamp`, the
/// `command`, the `app` and the child's `pid`. `hub.log` spans every launch of
/// that app and the lines that follow this one carry no timestamp of their
/// own — this is the marker that makes them readable without touching a
/// single `println!`. Pure, with the timestamp passed in rather than read
/// from the clock here, so it is testable on a fixed value.
fn launch_header(timestamp: &str, command: &str, app: &str, pid: u32) -> String {
    format!("=== {timestamp} {command} {app} (pid {pid}) ===")
}

/// Everything that can stop an `open` before the app's own process exists.
#[derive(Debug)]
pub enum OpenError {
    Registry(RegistryError),
    Manifest(ManifestError),
    Paths(PathsError),
    NotInstalled {
        id: String,
    },
    Broken {
        id: String,
        platform: String,
    },
    NoSnapshot {
        id: String,
        path: PathBuf,
    },
    IdentityChanged {
        id: String,
        registered: String,
        declared: String,
    },
    /// A file-bearing `open` of an app whose manifest declares no
    /// `actions.open_files` receiver — the files cannot be delivered, so
    /// nothing is launched at all.
    NoReceiver {
        id: String,
    },
    /// The originating caller's working directory could not be read, so a
    /// relative path cannot be made absolute without inventing where it
    /// came from.
    NoWorkingDirectory(io::Error),
    /// A batch that failed whole-batch validation, carrying the diagnostic
    /// that names the first offending path.
    InvalidBatch(String),
    NoExecutable(io::Error),
    Unstartable {
        executable: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::NotInstalled { id } => write!(
                formatter,
                "no app is installed under {id}. `tfsapp-hub list` shows what is, \
                 and `tfsapp-hub install <source>` adds one."
            ),
            Self::Broken { id, platform } => write!(
                formatter,
                "{id} is marked broken: its dependencies were resolved against PHP \
                 {platform} and could not be re-resolved against the PHP this hub runs. \
                 Update the app (`tfsapp-hub update {id}`), or roll the hub back \
                 (`tfsapp-hub --rollback`). Opening it would only fail later, deeper."
            ),
            Self::NoSnapshot { id, path } => write!(
                formatter,
                "{id} is registered but {} does not exist. Reinstall it, or drop the \
                 entry with `tfsapp-hub remove {id}`.",
                path.display()
            ),
            Self::IdentityChanged {
                id,
                registered,
                declared,
            } => write!(
                formatter,
                "{id} was installed as {registered} and its installed source now declares \
                 {declared}. The identifier names the app's data dir, so the hub will not \
                 guess which one you meant: reinstall the app under the identifier you want."
            ),
            Self::NoReceiver { id } => write!(
                formatter,
                "{id} declares no open_files receiver (actions.open_files.ipc in its \
                 manifest), so the hub cannot hand it files. Open it without `-- <file>...`, \
                 or have its manifest declare the receiver and reinstall it."
            ),
            Self::NoWorkingDirectory(source) => write!(
                formatter,
                "cannot read the working directory to resolve a relative path against: {source}"
            ),
            Self::InvalidBatch(diagnostic) => write!(formatter, "{diagnostic}"),
            Self::NoExecutable(source) => write!(
                formatter,
                "cannot find the hub's own binary to open the app with: {source}"
            ),
            Self::Unstartable { executable, source } => {
                write!(formatter, "cannot start {}: {source}", executable.display())
            }
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Paths(error) => Some(error),
            Self::NoWorkingDirectory(source) => Some(source),
            Self::NoExecutable(source) => Some(source),
            Self::Unstartable { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<RegistryError> for OpenError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<ManifestError> for OpenError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<PathsError> for OpenError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

#[cfg(test)]
#[path = "open_tests.rs"]
mod tests;
