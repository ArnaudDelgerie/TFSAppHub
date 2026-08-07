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

/// The §3 variables, plus the one path a caller needs in its own right.
///
/// The resolved port is deliberately *not* a field: nothing in an install binds
/// it, and `APP_PORT` already carries it. Plan 007, which does bind it, is the
/// one that should decide how it wants to hold the value.
pub struct AppEnvironment {
    /// Ready for `core`'s `command_with_env`.
    pub vars: Vec<(&'static str, String)>,
    /// `<OS data dir>/TFSApp/<identifier>/` — where the app's own data lands,
    /// which is what an install has to be able to name to the user.
    pub data_dir: PathBuf,
}

/// Assemble CONTRACT.md §3 for `manifest`'s app, installed at `app_dir`.
///
/// Creates the data dir and its four subdirectories, and nothing else. In
/// particular it does **not** wipe `cache/` and `build/` the way the station
/// does on every launch: over there the wipe is a remedy for a random
/// `/tmp/.mount_*` path baking itself into the compiled container, a cause the
/// hub does not have (it runs apps from a stable real path). Whether to wipe at
/// *launch* is plan 007's decision to make and measure; an install is not a
/// launch, and wiping here would only throw away the cache the install just
/// warmed.
pub fn resolve(
    paths: &Paths,
    manifest: &Manifest,
    app_dir: &Path,
) -> Result<AppEnvironment, EnvError> {
    let data_dir = paths.create_app_data_dir(&manifest.identifier)?;
    let data_subdir = data_dir.join("data");
    let cache_dir = data_dir.join("cache");
    let build_dir = data_dir.join("build");
    let log_dir = data_dir.join("log");
    let sessions_dir = data_dir.join("sessions");
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

    // The file-backed half of CONTRACT.md §6's `APP_SECRET`, deliberately: the
    // keyring-backed store lands in plan 007 with the rest of `secrets`, and
    // the contract's own resolution order makes this forward-compatible — an
    // existing keyring entry always wins, and a file left here is *migrated*
    // into the keyring rather than competing with it. Stable across the
    // install's commands and whatever runs later, which is what anything
    // Symfony signs needs.
    let app_secret = tfsapp_core::app_secret::load_or_create_app_secret(&data_subdir)
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
        // Zero, and true while it is written: no secret store is opened during
        // an install, so the store backing `APP_SECRET` here is the file
        // fallback and an app is right to warn its user about degraded secret
        // storage. Plan 007 opens the real store and this value follows it.
        ("TFS_KEYRING_AVAILABLE", "0".to_string()),
        ("TFS_APP_IDENTIFIER", manifest.identifier.clone()),
        ("TFS_APP_VERSION", manifest.app_version.clone()),
    ];

    // `TFS_BRIDGE_URL`/`TFS_BRIDGE_TOKEN` are deliberately absent, whatever the
    // manifest's `actions` declare: §3 makes them conditional on a bridge
    // *running*, and an install starts none. Present-but-dead would be worse
    // than absent — an app would open a connection to nothing.

    Ok(AppEnvironment { vars, data_dir })
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
