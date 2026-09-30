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
//!
//! One of the five directories this module creates is not like the other
//! four: `uploads/` (`APP_UPLOAD_DIR`, decision 006) is never emptied by the
//! host, at any launch, under any circumstance. `cache/` and `build/` are
//! wiped on a cache-stamp mismatch and `log/`/`sessions/` are the app's own
//! churn; `uploads/` is where a durable file goes precisely because nothing
//! here ever clears it.

use std::{
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use tfsapp_core::sidecar::path_to_string;

use crate::{
    lifecycle::{read_cache_stamp, CacheStamp, CacheStatus},
    manifest::Manifest,
};

/// Which moment is assembling this environment, and — since plan 009 — which
/// of §3's dev-clause variables to use.
///
/// Everything else in the returned list is identical across all three: an app
/// must not be able to tell an install's `bin/console` from a launch's, and
/// CONTRACT.md's dev section (plan 009 step 5) is the closed list of what a
/// dev session is allowed to see differently from a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// An `install` or `update`: warm what the app's own commands are about to
    /// build, and throw nothing away.
    Install,
    /// An installed app's launch: reuse `cache/`/`build/` when the cache stamp
    /// (plan 024) matches, wipe and rebuild when it does not, and rotate the
    /// logs before anything opens them. The stamp this launch would write if
    /// it rebuilt right now — the caller's, since `resolve` has neither a
    /// `Platform` nor any notion of the installed snapshot's identity beyond
    /// `app_dir` — travels with the variant rather than as a side parameter,
    /// so a `Launch` can never reach the wipe decision without one.
    Launch(CacheStamp),
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
    /// Ready for `core`'s `command_with_env`. The values are raw bytes
    /// (`OsString`), so a user directory outside UTF-8 reaches the app as
    /// itself rather than as a lossy look-alike.
    pub vars: Vec<(&'static str, OsString)>,
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
    /// `<data_dir>/log/` — where `commands.log`, `sidecar.log` and, since plan
    /// 046, each worker slot's own `worker-<n>.log` land. The one place a
    /// launch failure can be read from afterwards, which is why every message
    /// that mentions a failure names a file in here.
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

/// `MESSENGER_TRANSPORT_DSN`, from the manifest's post-fallback `workers`
/// (plan 045). The legacy spelling keeps its DSN byte for byte — an
/// already-installed app must not have rows already queued under
/// `queue_name='async'` stranded by a DSN change under it — while a manifest
/// that spells `workers` directly gets the bare DSN, so each transport's own
/// `queue_name` reaches Doctrine instead of being overridden by
/// `Connection::buildConfiguration()`'s left-hand array merge (this plan's
/// design decision, plan 045).
fn messenger_transport_dsn(manifest: &Manifest) -> String {
    if manifest.workers.is_empty() {
        "sync://".to_string()
    } else if manifest.async_worker {
        "doctrine://default?queue_name=async".to_string()
    } else {
        "doctrine://default".to_string()
    }
}

/// `TFS_WORKER_TRANSPORTS`: every transport the hub set out to run, after
/// fallbacks, in declaration order, deduplicated, comma-separated, empty when
/// none. Computed once at launch from the manifest — never a live report; a
/// slot's later fate is `worker-<n>.log`'s to tell. The union across workers
/// rather than a per-worker grouping — §3's
/// question is whether *this* transport is consumed, not which process
/// consumes it (parse-time refuses a transport repeated across declarations,
/// so the dedup here only ever guards the invariant, never masks a
/// collision).
fn worker_transports(manifest: &Manifest) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut ordered = Vec::new();
    for declaration in &manifest.workers {
        for transport in &declaration.transports {
            if seen.insert(transport.as_str()) {
                ordered.push(transport.as_str());
            }
        }
    }
    ordered.join(",")
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
/// ever creates what hangs *under* `state_root`: its six subdirectories.
///
/// **The launch-time cache wipe used to be unconditional; now it is a
/// comparison (plan 024).** The station empties `cache/` and `build/` on every
/// launch because a random `/tmp/.mount_*` AppImage path bakes itself into the
/// compiled Symfony container, and reusing it across an upgrade is how a stale
/// container survives. The hub does not have that cause: it runs installed
/// apps from a stable real path (see `paths.rs`), which is what makes a
/// persistent warm cache possible here at all — plan 024 step 1 measured the
/// wipe at roughly 1.7s of every launch, cost enough to be worth spending the
/// stamp on.
///
/// Three things can invalidate a compiled container, and only three: the
/// app's own `app_version`, the snapshot path it was compiled from, and the
/// `Platform` the PHP that compiled it ran under (`lifecycle::CacheStamp`). A
/// `Mode::Launch` carries the stamp it would write if it rebuilt right now —
/// see the variant's own doc comment for why `resolve` cannot derive that
/// itself — and wipes only when [`crate::lifecycle::read_cache_stamp`] finds a
/// reason to: a mismatched dimension, no stamp at all, or a `cache/` that
/// turned out empty despite one. Every wipe logs why, to stdout — which is
/// `hub.log` by the time this runs (`open::prepare_hub_log` redirects it
/// before the child that reaches here is ever spawned) — so a launch that
/// rebuilds is diagnosable rather than merely slow.
///
/// An install is not a launch: wiping there would throw away the very cache the
/// install's own `cache:warmup` just built. Neither is dev, but for a different
/// reason — Symfony's dev container invalidates itself on file change, which is
/// the mechanism the whole dev loop is measured on, and a wipe on every relaunch
/// would cost a full rebuild for nothing it buys back.
///
/// `microphone_granted` is the caller's own report of whether this launch's
/// webview really had the microphone grant applied — the
/// `webkit2gtk` setting written and the permission handler connected
/// (`media::install_permission_handler`, CONTRACT.md §7/§8, decision 007)
/// — `false` for every mode that builds no window at all (`Install`, `Run`),
/// and whatever `main::serve` learned from the splash install's report,
/// within `media::MICROPHONE_GRANT_DEADLINE`, for `Launch`/`Dev`.
/// `TFS_MEDIA_MICROPHONE` below is the AND of this and the manifest's own
/// declaration: the variable mirrors what actually happened on this
/// machine, never what was merely asked for.
pub fn resolve(
    manifest: &Manifest,
    app_dir: &Path,
    identifier: &str,
    state_root: &Path,
    mode: Mode,
    microphone_granted: bool,
) -> Result<AppEnvironment, EnvError> {
    resolve_with_store(
        manifest,
        app_dir,
        identifier,
        state_root,
        mode,
        microphone_granted,
        crate::secrets::new_store,
        glib::user_special_dir,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_with_store<F, G>(
    manifest: &Manifest,
    app_dir: &Path,
    identifier: &str,
    state_root: &Path,
    mode: Mode,
    microphone_granted: bool,
    new_store: F,
    resolve_user_dir: G,
) -> Result<AppEnvironment, EnvError>
where
    F: FnOnce(&str, &Path) -> crate::secrets::SecretStore,
    G: Fn(glib::UserDirectory) -> Option<PathBuf>,
{
    let data_dir = state_root.to_path_buf();
    let data_subdir = data_dir.join("data");
    let cache_dir = data_dir.join("cache");
    let build_dir = data_dir.join("build");
    let log_dir = data_dir.join("log");
    let sessions_dir = data_dir.join("sessions");
    let uploads_dir = data_dir.join("uploads");
    if let Mode::Launch(expected) = &mode {
        let wipe_reason = match read_cache_stamp(&data_subdir, &cache_dir, expected) {
            CacheStatus::Matches => None,
            CacheStatus::Absent => Some("no cache stamp recorded yet".to_string()),
            CacheStatus::Mismatch { reason } => Some(reason),
        };
        if let Some(reason) = wipe_reason {
            println!("cache/build not reused, rebuilding: {reason}");
            // Best-effort: a directory that cannot be removed is recreated below
            // and the launch carries on, rather than refusing to open the app
            // over a cache it could not clear. Dev never reaches this branch —
            // see the doc comment above. These two calls name `cache` and
            // `build` and will never grow a third: `uploads/` is never emptied
            // by the host, at any launch, under any circumstance (decision 006).
            let _ = fs::remove_dir_all(&cache_dir);
            let _ = fs::remove_dir_all(&build_dir);
        }
    }
    for directory in [
        &data_subdir,
        &cache_dir,
        &build_dir,
        &log_dir,
        &sessions_dir,
        &uploads_dir,
    ] {
        fs::create_dir_all(directory).map_err(|source| EnvError::Io {
            path: directory.clone(),
            source,
        })?;
    }
    if matches!(&mode, Mode::Launch(_) | Mode::Dev) {
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
    let secret_store = new_store(identifier, &data_subdir);

    // `APP_SECRET` is the one value dev does not run through the store above:
    // a fixed, throwaway constant, neither generated, persisted nor read from
    // the keyring — see [`DEV_APP_SECRET`]. Everything else about the store
    // (`actions.secrets`, `TFS_KEYRING_AVAILABLE` below) stays real.
    let app_secret = match &mode {
        Mode::Dev => DEV_APP_SECRET.to_string(),
        Mode::Install | Mode::Launch(_) | Mode::Run => {
            crate::secrets::resolve_app_secret(&secret_store, &data_subdir)
                .map_err(|error| EnvError::Secret(error.to_string()))?
        }
    };
    // Purely internal, never persisted: the same loopback process signs and
    // validates these, so a fresh one per invocation is strictly better than a
    // stored one.
    let mercure_secret = tfsapp_core::app_secret::random_secret_hex()
        .map_err(|error| EnvError::Secret(error.to_string()))?;

    let (app_env, app_debug) = match &mode {
        Mode::Dev => ("dev", "1"),
        Mode::Install | Mode::Launch(_) | Mode::Run => ("prod", "0"),
    };

    let mut vars = vec![
        ("APP_ENV", app_env.into()),
        ("APP_DEBUG", app_debug.into()),
        ("APP_SECRET", app_secret.into()),
        ("APP_PORT", port.to_string().into()),
        ("APP_ORIGIN", origin.into()),
        (
            "APP_PUBLIC_DIR",
            path_to_string(&app_dir.join("public")).into(),
        ),
        ("APP_CACHE_DIR", path_to_string(&cache_dir).into()),
        ("APP_BUILD_DIR", path_to_string(&build_dir).into()),
        ("APP_LOG_DIR", path_to_string(&log_dir).into()),
        ("APP_SESSION_DIR", path_to_string(&sessions_dir).into()),
        ("APP_UPLOAD_DIR", path_to_string(&uploads_dir).into()),
        (
            "DATABASE_URL",
            format!("sqlite:///{}", data_subdir.join("app.db").display()).into(),
        ),
        (
            "MESSENGER_TRANSPORT_DSN",
            messenger_transport_dsn(manifest).into(),
        ),
        ("MERCURE_URL", mercure_url.clone().into()),
        ("MERCURE_PUBLIC_URL", mercure_url.into()),
        ("MERCURE_JWT_SECRET", mercure_secret.into()),
        (
            "TFS_ASYNC_WORKER",
            if manifest.workers.is_empty() {
                "0"
            } else {
                "1"
            }
            .into(),
        ),
        ("TFS_WORKER_TRANSPORTS", worker_transports(manifest).into()),
        // What the probe above actually picked, not whether a keyring is
        // installed: a present-but-locked one has already fallen back to the
        // file, and an app is entitled to warn its user on that basis.
        (
            "TFS_KEYRING_AVAILABLE",
            crate::secrets::keyring_env_value(&secret_store)
                .to_string()
                .into(),
        ),
        ("TFS_APP_IDENTIFIER", identifier.into()),
        ("TFS_APP_VERSION", manifest.app_version.clone().into()),
        (
            "TFS_MEDIA_MICROPHONE",
            if manifest.actions.media.microphone && microphone_granted {
                "1"
            } else {
                "0"
            }
            .into(),
        ),
    ];

    // `TFS_BRIDGE_URL`/`TFS_BRIDGE_TOKEN` are deliberately absent, whatever the
    // manifest's `actions` declare: §3 makes them conditional on a bridge
    // *running*, and an install starts none. Present-but-dead would be worse
    // than absent — an app would open a connection to nothing.

    // Appended after the canonical list above (decision 008): one
    // `TFS_USER_<NAME>_DIR` per declared `actions.paths` member GLib actually
    // resolves, entirely absent otherwise — never an empty string, never a
    // fallback guess.
    vars.extend(crate::user_dirs::resolve(
        &manifest.actions.paths,
        resolve_user_dir,
    ));

    Ok(AppEnvironment {
        vars,
        data_dir,
        data_subdir,
        log_dir,
        port,
        secret_store,
    })
}

/// Test-only store injection for environment resolution paths that need to
/// model a working keyring without probing the host Secret Service.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn resolve_with_secret_store_for_test(
    manifest: &Manifest,
    app_dir: &Path,
    identifier: &str,
    state_root: &Path,
    mode: Mode,
    microphone_granted: bool,
    secret_store: crate::secrets::SecretStore,
) -> Result<AppEnvironment, EnvError> {
    resolve_with_store(
        manifest,
        app_dir,
        identifier,
        state_root,
        mode,
        microphone_granted,
        move |_, _| secret_store,
        glib::user_special_dir,
    )
}

/// Test-only `actions.paths` resolver injection, so a test can prove
/// `TFS_USER_<NAME>_DIR` wiring without depending on the host account's real
/// `~/.config/user-dirs.dirs` — see `user_dirs.rs`'s own header.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn resolve_with_user_dirs_resolver_for_test(
    manifest: &Manifest,
    app_dir: &Path,
    identifier: &str,
    state_root: &Path,
    mode: Mode,
    microphone_granted: bool,
    resolve_user_dir: impl Fn(glib::UserDirectory) -> Option<PathBuf>,
) -> Result<AppEnvironment, EnvError> {
    resolve_with_store(
        manifest,
        app_dir,
        identifier,
        state_root,
        mode,
        microphone_granted,
        crate::secrets::new_store,
        resolve_user_dir,
    )
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
