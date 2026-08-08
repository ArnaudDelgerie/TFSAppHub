//! The environment an app's own processes run in — station CONTRACT.md §3.
//!
//! PHP never reads `tfsapp.config.json`. Identity, port, paths and secrets
//! reach the app as environment variables, and §3 is the canonical list of
//! them. This module builds that list for the hub, from the installed
//! snapshot's manifest, so an app cannot tell which host started it — which is
//! the whole contract.
//!
//! Two things are worth stating about *where* the values point, because they
//! are the reason a hubbed app and a packaged one are the same app:
//!
//! - The data dir is `<OS data dir>/TFSApp/<identifier>/`, resolved from
//!   `identifier` alone (see `paths.rs`). Not from the hub's root, not from
//!   `id`. A user who had the packaged AppImage and installs the same app here
//!   opens it onto the same database, the same sessions, the same secret.
//! - `APP_PUBLIC_DIR` points into `apps/<id>/`, the installed snapshot, which is
//!   the one thing that *is* the hub's.
//!
//! Built once per invocation and handed to whatever needs it: the install-time
//! lifecycle commands today, the sidecar in plan 007. The two differ only in
//! what plan 007 adds to it (a live bridge, a real keyring-backed store), never
//! in the shape.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use tfsapp_core::sidecar::path_to_string;

use crate::{
    manifest::Manifest,
    paths::{Paths, PathsError},
};

/// Which moment is assembling this environment.
///
/// The variables are identical either way — that is the point of §3, and an app
/// must not be able to tell an install's `bin/console` from a launch's. What
/// differs is what the assembly *does to the data dir* on its way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// An `install` or `update`: warm what the app's own commands are about to
    /// build, and throw nothing away.
    Install,
    /// A launch: wipe `cache/` and `build/`, and rotate the logs before anything
    /// opens them.
    Launch,
}

/// The §3 variables, plus the paths a caller needs in their own right.
pub struct AppEnvironment {
    /// Ready for `core`'s `command_with_env`.
    pub vars: Vec<(&'static str, String)>,
    /// `<OS data dir>/TFSApp/<identifier>/` — where the app's own data lands,
    /// which is what an install has to be able to name to the user.
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

/// Assemble CONTRACT.md §3 for `manifest`'s app, installed at `app_dir`.
///
/// Creates the data dir and its four subdirectories. What else it does depends
/// on [`Mode`], and only one of the two differences is interesting.
///
/// **The launch-time cache wipe is the station's workaround, kept deliberately.**
/// Over there `cache/` and `build/` are emptied on every launch because a random
/// `/tmp/.mount_*` AppImage path bakes itself into the compiled Symfony
/// container, and reusing it across an upgrade is how a stale container survives.
/// The hub does not have that cause: it runs apps from a stable real path (see
/// `paths.rs`). So a persistent warm cache is *possible* here — which is not the
/// same as proven, and claiming it needs its own measurement. Until then the hub
/// pays the same cost the station pays, because the failure mode of getting this
/// wrong is a user running last version's compiled container against this
/// version's code. Plan 007 keeps the wipe on purpose and says so; dropping it is
/// its own plan.
///
/// An install is not a launch: wiping there would throw away the very cache the
/// install's own `cache:warmup` just built.
pub fn resolve(
    paths: &Paths,
    manifest: &Manifest,
    app_dir: &Path,
    mode: Mode,
) -> Result<AppEnvironment, EnvError> {
    let data_dir = paths.create_app_data_dir(&manifest.identifier)?;
    let data_subdir = data_dir.join("data");
    let cache_dir = data_dir.join("cache");
    let build_dir = data_dir.join("build");
    let log_dir = data_dir.join("log");
    let sessions_dir = data_dir.join("sessions");
    if mode == Mode::Launch {
        // Best-effort: a directory that cannot be removed is recreated below and
        // the launch carries on, rather than refusing to open the app over a
        // cache it could not clear.
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
    if mode == Mode::Launch {
        // Once per launch, and here rather than anywhere later: this is the one
        // point both `commands.log`'s first write and `sidecar.log`'s fd open
        // are still ahead of, which is what a size-based rotation needs to be
        // true to rotate the file rather than the file's replacement.
        tfsapp_core::log::rotate_logs(&log_dir);
    }

    // The same resolution packaged mode makes, `data/config.json`'s
    // `port_override` included: a user who pinned a different port for their
    // installation must not have it ignored because the app moved hosts.
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
    // launch. Both write `APP_SECRET`, and having one of them write a plaintext
    // file while the other used the keyring would leave a secret on disk that
    // nothing needed. CONTRACT.md §6's resolution order does the rest: an
    // existing keyring entry always wins, and a file from an older installation
    // is migrated into the keyring rather than competing with it.
    let secret_store = crate::secrets::new_store(&manifest.identifier, &data_subdir);
    let app_secret = crate::secrets::resolve_app_secret(&secret_store, &data_subdir)
        .map_err(|error| EnvError::Secret(error.to_string()))?;
    // Purely internal, never persisted: the same loopback process signs and
    // validates these, so a fresh one per invocation is strictly better than a
    // stored one.
    let mercure_secret = tfsapp_core::app_secret::random_secret_hex()
        .map_err(|error| EnvError::Secret(error.to_string()))?;

    let vars = vec![
        ("APP_ENV", "prod".to_string()),
        ("APP_DEBUG", "0".to_string()),
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
        ("TFS_APP_IDENTIFIER", manifest.identifier.clone()),
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
    Paths(PathsError),
    Io { path: PathBuf, source: io::Error },
    Port(String),
    Secret(String),
}

impl fmt::Display for EnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Port(detail) => write!(formatter, "cannot settle the app's port: {detail}"),
            Self::Secret(detail) => write!(formatter, "cannot resolve APP_SECRET: {detail}"),
        }
    }
}

impl std::error::Error for EnvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<PathsError> for EnvError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

#[cfg(test)]
#[path = "app_env_tests.rs"]
mod tests;
