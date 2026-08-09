//! The environment an app's own processes run in — CONTRACT.md §3.
//!
//! PHP never reads `tfsapp.config.json`. Identity, port, paths and secrets
//! reach the app as environment variables, and §3 is the canonical list of
//! them. This module builds that list for the hub, so an app cannot tell which
//! host started it, and — since plan 009 — cannot tell dev from installed
//! either, beyond the handful of values §3's own dev clause names.
//!
//! Two things are worth stating about *where* the values point, because they
//! are the reason a hubbed app and a packaged one are the same app:
//!
//! - An installed app's data dir is `<OS data dir>/TFSApp/<identifier>/`,
//!   resolved from `identifier` alone (see `paths.rs`). Not from the hub's
//!   root, not from `id`. A user who had the packaged AppImage and installs the
//!   same app here opens it onto the same database, the same sessions, the
//!   same secret.
//! - `APP_PUBLIC_DIR` points into `apps/<id>/`, the installed snapshot, which is
//!   the one thing that *is* the hub's.
//!
//! A dev session's state root is its own project's `var/` instead — the caller
//! resolves *which* root applies (`open::resolve` or `dev::resolve`, plan 009
//! step 1's `LaunchSpec::state_root`) and this module only ever writes inside
//! the one it is handed.
//!
//! Built once per invocation and handed to whatever needs it: the install-time
//! lifecycle commands, and the sidecar (`sidecar::start`).

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use tfsapp_core::sidecar::path_to_string;

use crate::manifest::Manifest;

/// Which moment is assembling this environment, and — since plan 009 — which
/// of §3's dev-clause variables to use.
///
/// Everything else in the returned list is identical across all three: an app
/// must not be able to tell an install's `bin/console` from a launch's, and
/// CONTRACT.md's dev section (plan 009 step 5) is the closed list of what a
/// dev session is allowed to see differently from a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// An `install` or `update`: warm what the app's own commands are about to
    /// build, and throw nothing away.
    Install,
    /// An installed app's launch: wipe `cache/` and `build/`, and rotate the
    /// logs before anything opens them.
    Launch,
    /// A `dev <path>` launch (plan 009): `APP_ENV=dev`, `APP_DEBUG=1`, a fixed
    /// throwaway `APP_SECRET`, and — like `Launch` — a log rotation. Never
    /// wipes `cache/`/`build/`: Symfony's dev container invalidates itself on
    /// file change, which is the mechanism that makes the dev loop cheap.
    Dev,
    /// A `run <id> <alias>` command (plan 013): `prod`/`APP_DEBUG=0` and the
    /// real `APP_SECRET`, exactly like `Launch` — but neither wipes `cache/`/
    /// `build/` nor rotates the logs. A `concurrent` alias started beside a
    /// live window would otherwise be wiping the compiled container that
    /// window is serving out of, and rotating `sidecar.log` out from under a
    /// process holding it open would rotate the file's replacement instead.
    Run,
}

/// The dev session's fixed, throwaway `APP_SECRET` (CONTRACT.md's dev
/// section). Never generated, never persisted, never read from the keyring —
/// a dev session's own `dev.<identifier>` cookie store (see `dev.rs`) is what
/// the station needed a *random* one to work around, so a constant is what
/// keeps the developer logged in across a relaunch instead of costing them a
/// fresh login every time.
const DEV_APP_SECRET: &str = "tfsapp-dev-app-secret-0123456789abcdef";

/// The §3 variables, plus the paths a caller needs in their own right.
pub struct AppEnvironment {
    /// Ready for `core`'s `command_with_env`.
    pub vars: Vec<(&'static str, String)>,
    /// This launch's `state_root`, as handed in: `<OS data dir>/TFSApp/<identifier>/`
    /// for an installed app, `<project>/var/` for a dev session — either way,
    /// what an install or a `dev` launch has to be able to name to the user.
    pub data_dir: PathBuf,
    /// `<data_dir>/data/` — the app's own writable subdirectory: its SQLite
    /// database, its `APP_SECRET`, and `config.json`, the record of which
    /// version last wrote all of it (CONTRACT.md §6). Carried out because the
    /// caller that finishes an install event has to stamp that record, and
    /// rebuilding the path at each call site is how two of them end up
    /// disagreeing.
    pub data_subdir: PathBuf,
    /// `<data_dir>/log/` — where `commands.log` and `sidecar.log` land. The one
    /// place a launch failure can be read from afterwards, which is why every
    /// message that mentions a failure names a file in here.
    pub log_dir: PathBuf,
    /// The port `APP_PORT`/`APP_ORIGIN` were built from, for the caller that has
    /// to poll `/healthz` on it and point a window at it. Reading it back out of
    /// `vars` would work and would be one string parse away from a launch that
    /// silently polls the wrong port.
    pub port: u16,
    /// The store `APP_SECRET` was resolved against, carried out so the launch
    /// can hand the very same instance to the IPC commands and the bridge — one
    /// keyring probe per launch, and no way for two of them to disagree about
    /// which backend is in use.
    pub secret_store: crate::secrets::SecretStore,
}

/// Assemble CONTRACT.md §3 for `manifest`'s app, served from `app_dir`, with
/// its state rooted at `state_root`.
///
/// `identifier` is the *runtime* identifier — `dev.<identifier>` for a dev
/// session, `manifest.identifier` unchanged for an install or a launch — used
/// everywhere §3 ties a variable to "the app's own identity": `TFS_APP_IDENTIFIER`,
/// and the `actions.secrets` store's service name. It is not always
/// `manifest.identifier` for the same reason `state_root` is not always the
/// installed app's OS data dir: the caller (`open::resolve` or `dev::resolve`)
/// already decided which of the two this launch is.
///
/// `state_root` is created if it does not already exist, but is otherwise the
/// caller's to have prepared — an installed app's `0700` tightening is
/// `paths::create_app_data_dir`'s job, done before this is ever called, and a
/// dev session's `var/` needs no such tightening at all. This function only
/// ever creates what hangs *under* `state_root`: its five subdirectories.
///
/// **The launch-time cache wipe is the station's workaround, kept deliberately.**
/// Over there `cache/` and `build/` are emptied on every launch because a random
/// `/tmp/.mount_*` AppImage path bakes itself into the compiled Symfony
/// container, and reusing it across an upgrade is how a stale container survives.
/// The hub does not have that cause: it runs installed apps from a stable real
/// path (see `paths.rs`). So a persistent warm cache is *possible* here — which
/// is not the same as proven, and claiming it needs its own measurement. Until
/// then the hub pays the same cost the station pays for an installed launch,
/// because the failure mode of getting this wrong is a user running last
/// version's compiled container against this version's code.
///
/// An install is not a launch: wiping there would throw away the very cache the
/// install's own `cache:warmup` just built. Neither is dev, but for a different
/// reason — Symfony's dev container invalidates itself on file change, which is
/// the mechanism the whole dev loop is measured on, and a wipe on every relaunch
/// would cost a full rebuild for nothing it buys back.
pub fn resolve(
    manifest: &Manifest,
    app_dir: &Path,
    identifier: &str,
    state_root: &Path,
    mode: Mode,
) -> Result<AppEnvironment, EnvError> {
    let data_dir = state_root.to_path_buf();
    let data_subdir = data_dir.join("data");
    let cache_dir = data_dir.join("cache");
    let build_dir = data_dir.join("build");
    let log_dir = data_dir.join("log");
    let sessions_dir = data_dir.join("sessions");
    if mode == Mode::Launch {
        // Best-effort: a directory that cannot be removed is recreated below and
        // the launch carries on, rather than refusing to open the app over a
        // cache it could not clear. Dev never reaches this branch — see the
        // doc comment above.
        let _ = fs::remove_dir_all(&cache_dir);
        let _ = fs::remove_dir_all(&build_dir);
    }
    for directory in [
        &data_subdir,
        &cache_dir,
        &build_dir,
        &log_dir,
        &sessions_dir,
    ] {
        fs::create_dir_all(directory).map_err(|source| EnvError::Io {
            path: directory.clone(),
            source,
        })?;
    }
    if matches!(mode, Mode::Launch | Mode::Dev) {
        // Once per launch — installed or dev — and here rather than anywhere
        // later: this is the one point both `commands.log`'s first write and
        // `sidecar.log`'s fd open are still ahead of, which is what a
        // size-based rotation needs to be true to rotate the file rather than
        // the file's replacement. `Run` is deliberately excluded: rotating
        // `sidecar.log` out from under a process already holding it open (a
        // `concurrent` alias beside a live window) would rotate the file's
        // replacement instead of the file itself — see `Mode::Run`'s own doc
        // comment.
        tfsapp_core::log::rotate_logs(&log_dir);
    }

    // The same resolution an installed launch makes, `data/config.json`'s
    // `port_override` included: a user who pinned a different port for their
    // installation must not have it ignored because the app moved hosts. A dev
    // `var/data/config.json` never exists (plan 009 step 4 — the version guard
    // does not run in dev), so this reads the same absence it would for a
    // fresh install and simply picks a free port.
    //
    // Nothing binds this port during an install — there is no sidecar yet. It
    // is assembled anyway because an app is free to build a URL out of
    // `APP_ORIGIN` at container-compile time, and a variable that is merely
    // absent fails in a way that names no cause.
    let port = match tfsapp_core::ports::resolve_packaged_port(manifest.app_port, &data_subdir)
        .map_err(|error| EnvError::Port(error.to_string()))?
    {
        Some(port) => port,
        None => tfsapp_core::ports::pick_free_local_port()
            .map_err(|error| EnvError::Port(error.to_string()))?,
    };
    let origin = format!("http://127.0.0.1:{port}");
    let mercure_url = format!("{origin}/.well-known/mercure");

    // The real store, probed once here — for an install exactly as for a
    // launch or a dev session — under `identifier`, which is `dev.<identifier>`
    // in dev and keeps a dev session's declared secrets from ever resolving to
    // the same keyring entry as the installed app's own (CONTRACT.md's dev
    // section; the trap the station's own `dev_secrets_service` already solved).
    let secret_store = crate::secrets::new_store(identifier, &data_subdir);

    // `APP_SECRET` is the one value dev does not run through the store above:
    // a fixed, throwaway constant, neither generated, persisted nor read from
    // the keyring — see [`DEV_APP_SECRET`]. Everything else about the store
    // (`actions.secrets`, `TFS_KEYRING_AVAILABLE` below) stays real.
    let app_secret = match mode {
        Mode::Dev => DEV_APP_SECRET.to_string(),
        Mode::Install | Mode::Launch | Mode::Run => {
            crate::secrets::resolve_app_secret(&secret_store, &data_subdir)
                .map_err(|error| EnvError::Secret(error.to_string()))?
        }
    };
    // Purely internal, never persisted: the same loopback process signs and
    // validates these, so a fresh one per invocation is strictly better than a
    // stored one.
    let mercure_secret = tfsapp_core::app_secret::random_secret_hex()
        .map_err(|error| EnvError::Secret(error.to_string()))?;

    let (app_env, app_debug) = match mode {
        Mode::Dev => ("dev", "1"),
        Mode::Install | Mode::Launch | Mode::Run => ("prod", "0"),
    };

    let vars = vec![
        ("APP_ENV", app_env.to_string()),
        ("APP_DEBUG", app_debug.to_string()),
        ("APP_SECRET", app_secret),
        ("APP_PORT", port.to_string()),
        ("APP_ORIGIN", origin),
        ("APP_PUBLIC_DIR", path_to_string(&app_dir.join("public"))),
        ("APP_CACHE_DIR", path_to_string(&cache_dir)),
        ("APP_BUILD_DIR", path_to_string(&build_dir)),
        ("APP_LOG_DIR", path_to_string(&log_dir)),
        ("APP_SESSION_DIR", path_to_string(&sessions_dir)),
        (
            "DATABASE_URL",
            format!("sqlite:///{}", data_subdir.join("app.db").display()),
        ),
        (
            "MESSENGER_TRANSPORT_DSN",
            match manifest.async_worker {
                true => "doctrine://default?queue_name=async",
                false => "sync://",
            }
            .to_string(),
        ),
        ("MERCURE_URL", mercure_url.clone()),
        ("MERCURE_PUBLIC_URL", mercure_url),
        ("MERCURE_JWT_SECRET", mercure_secret),
        (
            "TFS_ASYNC_WORKER",
            match manifest.async_worker {
                true => "1",
                false => "0",
            }
            .to_string(),
        ),
        // What the probe above actually picked, not whether a keyring is
        // installed: a present-but-locked one has already fallen back to the
        // file, and an app is entitled to warn its user on that basis.
        (
            "TFS_KEYRING_AVAILABLE",
            crate::secrets::keyring_env_value(&secret_store).to_string(),
        ),
        ("TFS_APP_IDENTIFIER", identifier.to_string()),
        ("TFS_APP_VERSION", manifest.app_version.clone()),
    ];

    // `TFS_BRIDGE_URL`/`TFS_BRIDGE_TOKEN` are deliberately absent, whatever the
    // manifest's `actions` declare: §3 makes them conditional on a bridge
    // *running*, and an install starts none. Present-but-dead would be worse
    // than absent — an app would open a connection to nothing.

    Ok(AppEnvironment {
        vars,
        data_dir,
        data_subdir,
        log_dir,
        port,
        secret_store,
    })
}

#[derive(Debug)]
pub enum EnvError {
    Io { path: PathBuf, source: io::Error },
    Port(String),
    Secret(String),
}

impl fmt::Display for EnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Port(detail) => write!(formatter, "cannot settle the app's port: {detail}"),
            Self::Secret(detail) => write!(formatter, "cannot resolve APP_SECRET: {detail}"),
        }
    }
}

impl std::error::Error for EnvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "app_env_tests.rs"]
mod tests;
