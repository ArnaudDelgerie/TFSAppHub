//! `tfsapp-hub dev <path>` — the live-source constructor of [`LaunchSpec`].
//!
//! The counterpart to `open::resolve`: where that one reads an installed
//! snapshot from the registry, this one reads a project directory straight
//! from the filesystem, live, never snapshotted — the developer's own
//! toolchain does the building, and the watch belongs to the build tool
//! that already has one, so the hub only serves and relaunches (plan 009).
//! Both are pure resolve functions — no sidecar, no window, no
//! guard — and both exist to let a mistyped path or a missing file fail at a
//! terminal instead of inside a half-open window.
//!
//! **The identity is `dev.<identifier>`.** `dev ./x` and `open x` (once `x` is
//! installed) resolve to the same manifest, so without a prefix they would
//! share a GTK app id, a single-instance key, a WebKit cookie store and the
//! §5 liveness lock — while pointing at two different databases. The prefix
//! is a runtime namespace only: `product_name` and `icon_path` stay the
//! project's own, unprefixed, because a user should never read "dev." in a
//! window title.

use std::{fmt, fs, path::PathBuf, process::Command};

use crate::{
    cli::{EXIT_FAILED, OPEN_CHILD_SUBCOMMAND},
    identity::Identity,
    launch::{LaunchSpec, Source},
    manifest::{self, ManifestError},
    update_check,
};

/// The runtime-identity namespace a dev session's identifier is prefixed
/// with — see the module header.
pub const DEV_IDENTIFIER_PREFIX: &str = "dev.";

/// The whole command, parent side. Returns the process's exit code.
///
/// **Foreground, unlike `open::run`.** `open` is a launcher: it resolves,
/// spawns, and hands the prompt back. `dev` is a loop the developer is
/// watching, and `Ctrl-C` has to stop it — so this parent stays up, forwards
/// `SIGINT`/`SIGTERM` to the child, waits on it, and exits with its status
/// (plan 009, step 4).
///
/// The child cannot use `spawn_signal_forwarder` as it stands: that helper
/// gates on `TFS_APP_IDENTIFIER` in the target's `/proc/<pid>/environ`, which
/// only the *sidecar* carries by default — the hub's own re-executed child
/// does not. Rather than teach the forwarder a second rule, this sets the
/// same marker on the child directly, so the one rule
/// `core::process::terminate_if_identifier_matches` already checks
/// everywhere else holds here too.
///
/// **Nor can it use `spawn_signal_forwarder`'s own `terminate` relay**, for a
/// second, unrelated reason found live in step 6's own verification: the
/// child already owns a bounded, escalating teardown of its own
/// (`lifecycle::install_shutdown_on_signal` stops the worker, then the
/// server, each with up to 3s of SIGTERM-then-SIGKILL — up to ~6s together).
/// Relaying through `terminate` would impose a second, shorter 3s budget on
/// top of that one, and the outer budget would win the race: it SIGKILLs the
/// child before the child's own teardown reaches the server, orphaning it.
/// `core::process::signal_terminate_once` sends the one signal and returns;
/// the wait below is `child.wait()`, which blocks on the child's *own* budget
/// instead of imposing a second one.
pub fn run(path: &str) -> i32 {
    let spec = match resolve(path) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    for warning in &spec.warnings {
        eprintln!("tfsapp-hub: warning: {warning}");
    }

    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!(
                "tfsapp-hub: cannot find the hub's own binary to run the project with: {error}"
            );
            return EXIT_FAILED;
        }
    };

    let mut command = Command::new(&executable);
    command.args(child_args(&spec));
    command.env("TFS_APP_IDENTIFIER", &spec.identity.identifier);
    tfsapp_core::process::set_own_process_group(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("tfsapp-hub: cannot start {}: {error}", executable.display());
            return EXIT_FAILED;
        }
    };

    match tfsapp_core::process::install_signal_forwarding() {
        Ok(read_fd) => {
            let child_pid = child.id();
            let identifier = spec.identity.identifier.clone();
            tfsapp_core::process::spawn_on_signal(read_fd, move || {
                if tfsapp_core::process::process_environ_has_identifier(child_pid, &identifier) {
                    tfsapp_core::process::signal_terminate_once(child_pid);
                }
            });
        }
        Err(error) => {
            // Not fatal, and degrades the same way
            // `lifecycle::install_shutdown_on_signal` does: the session runs
            // exactly as it would have, just without Ctrl-C forwarding an
            // orderly shutdown to the child.
            eprintln!("tfsapp-hub: cannot install signal handling: {error}");
        }
    }

    match child.wait() {
        Ok(status) => status.code().unwrap_or(EXIT_FAILED),
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The live constructor of [`LaunchSpec`]: check the project looks like a
/// TFSApp, load its manifest, and build the spec dev's own launch pipeline
/// (steps 3–4) will run against.
///
/// Every refusal below names the file it looked for and the path it looked
/// under — the same principle `open::resolve` follows, for the same reason:
/// the audience is someone at a terminal who typed one path and got nothing.
/// Order matters only in that it is the order CONTRACT.md §1 lists an app's
/// required layout, so the first refusal to fire is the first thing a reader
/// of that section would check too.
pub fn resolve(project_path: &str) -> Result<LaunchSpec, DevError> {
    let project_path = PathBuf::from(project_path);
    if !project_path.is_dir() {
        return Err(DevError::NotADirectory { path: project_path });
    }
    // Canonicalize once, here: left verbatim, a relative `dev ../Foo/app`
    // builds a relative `state_root`, and `sidecar::start` passes that
    // relative Caddyfile path to FrankenPHP while also setting
    // `current_dir(app_dir)` — the server resolves it a second time against
    // the project dir and dies looking for `../Foo/app/var/Caddyfile`. Doing
    // it once here, rather than at every path built below, fixes `app_dir`,
    // `state_root`, `icon_path` and the child's own `--project` together.
    // `is_dir` above already proved the path exists, so a failure here is a
    // TOCTOU race, not the ordinary "no such project" case — folded into the
    // same refusal rather than given its own untestable variant.
    let project_path = fs::canonicalize(&project_path)
        .map_err(|_| DevError::NotADirectory { path: project_path })?;

    for (what, relative) in [
        ("bin/console", "bin/console"),
        ("public/index.php", "public/index.php"),
    ] {
        let path = project_path.join(relative);
        if !path.is_file() {
            return Err(DevError::MissingEntryPoint { what, path });
        }
    }

    let loaded = manifest::load(&project_path)?;
    let manifest = loaded.manifest;

    let identity = Identity {
        identifier: format!("{DEV_IDENTIFIER_PREFIX}{}", manifest.identifier),
        product_name: manifest.product_name.clone(),
        icon_path: manifest
            .icon_path
            .as_ref()
            .map(|relative| project_path.join(relative)),
    };

    let label = project_path.display().to_string();
    let state_root = project_path.join("var");

    Ok(LaunchSpec {
        source: Source::Live,
        app_dir: project_path,
        identity,
        manifest,
        state_root,
        label,
        warnings: loaded.warnings,
        update: update_check::Context::Dev,
        // A dev session has no registry entry to build a stamp from, and
        // `Mode::Dev` never wipes `cache/`/`build/` — nothing would ever read
        // this.
        expected_cache: None,
        pending_revalidation: None,
    })
}

/// The child's argv, after the binary's own name — the dev counterpart of
/// `open::child_args`, `--project <path>` in place of `--id <id>`. See that
/// function for why the identity travels as arguments rather than being
/// re-derived by the child.
///
/// Only ever called on a [`Source::Live`] spec — `resolve` above is this
/// module's only constructor, and it never builds an `Installed` one.
pub fn child_args(spec: &LaunchSpec) -> Vec<String> {
    let mut args = vec![
        OPEN_CHILD_SUBCOMMAND.to_string(),
        "--project".to_string(),
        spec.app_dir.display().to_string(),
        "--identity".to_string(),
        spec.identity.identifier.clone(),
        "--name".to_string(),
        spec.identity.product_name.clone(),
    ];
    if let Some(icon) = &spec.identity.icon_path {
        args.push("--icon".to_string());
        args.push(icon.display().to_string());
    }
    args
}

/// Every way `dev <path>` can be refused before anything is spawned.
#[derive(Debug)]
pub enum DevError {
    NotADirectory { path: PathBuf },
    MissingEntryPoint { what: &'static str, path: PathBuf },
    Manifest(ManifestError),
}

impl fmt::Display for DevError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotADirectory { path } => write!(
                formatter,
                "{} is not a directory. `tfsapp-hub dev` runs a project's live source in \
                 place — point it at the project's own root.",
                path.display()
            ),
            Self::MissingEntryPoint { what, path } => write!(
                formatter,
                "{} does not exist. A TFSApp project must have a {what} (CONTRACT.md §1).",
                path.display()
            ),
            Self::Manifest(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DevError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ManifestError> for DevError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

#[cfg(test)]
#[path = "dev_tests.rs"]
mod tests;
