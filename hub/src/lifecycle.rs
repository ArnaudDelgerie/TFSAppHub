//! The guards a launch has to pass before an app gets a window.
//!
//! Ported from the station's `lifecycle.rs`, which is where these were written
//! and measured. Three of them come across whole — the version decision against
//! `data/config.json`, the static-port conflict guard, and the sidecar liveness
//! lock with its crash-orphan reap — and one is deliberately reshaped, which is
//! the interesting part.
//!
//! **What is the same.** All of it is CONTRACT.md §6, and §6 is about the *data
//! dir*, which the two hosts share by construction: the same `identifier`
//! resolves to the same directory whether a packaged AppImage or the hub opened
//! it. A guard that read differently on one host would let the two corrupt each
//! other's data, so these are not "ported code", they are the same rules read
//! from the same file.
//!
//! **Everything runs before `tauri::Builder` exists.** Not a style choice: the
//! refusals below show a blocking native dialog through `rfd`, and once Tauri
//! has claimed GTK a raw `rfd` dialog deadlocks rather than appears. The station
//! carries the same constraint and states it the same way; the hub inherits it
//! unchanged because it inherits the reason.
//!
//! **What is reshaped, and why it is a case-1 difference.** On the station, a
//! launch that finds a *newer* binary than the data dir records **is** the update
//! event: someone dropped a newer AppImage over the same data dir, and a launch
//! is the only moment that can notice. The hub has an explicit `update <id>`
//! which owns that event — it replaces the snapshot, runs `pre-update`/
//! `post-update` and records the new version at its success point (plan 011).
//! So the same observation means something different here: an installed snapshot
//! newer than the record is not an update in progress, it is an update that
//! never completed, or a tree edited under the hub. Opening it would run the app
//! against a database its migrations never touched, which is worse than not
//! opening. It is refused, naming the command that resolves it.
//!
//! The event *itself* is not gone, it moved: the hub's `install` is the install
//! event (see `install.rs`, and friction #6 in
//! `.project/contract-amendments.md`), and it is what writes the record these
//! guards read.

use std::{
    fs,
    path::{Path, PathBuf},
};

use tfsapp_core::ports::DataConfig;

/// Which lifecycle event (CONTRACT.md §6), if any, a launch represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// No `data/config.json` yet: nothing has recorded a version in this data
    /// dir.
    Install,
    /// Recorded version < the installed app's: on the station a
    /// manually-installed upgrade, in the hub an inconsistency — see the module
    /// header.
    Update,
    /// Recorded version == the installed app's: an ordinary launch.
    None,
}

/// Why [`lifecycle_decision`] could not resolve to an event.
#[derive(Debug)]
pub enum LifecycleDecisionError {
    InvalidVersion(semver::Error),
    Downgrade {
        recorded: semver::Version,
        current: semver::Version,
    },
}

/// Decide the lifecycle event from the recorded version string
/// (`data/config.json`'s `version`, `None` when the file does not exist yet)
/// against the installed app's own `app_version`. No I/O, no dialog.
///
/// Ported unchanged from the station, down to the `Downgrade` variant being an
/// error rather than an event: running an app against data written by a newer
/// version of itself has no defined behaviour, so there is nothing for the
/// caller to do with it but refuse.
pub fn lifecycle_decision(
    recorded: Option<&str>,
    current: &semver::Version,
) -> Result<LifecycleEvent, LifecycleDecisionError> {
    let Some(recorded) = recorded else {
        return Ok(LifecycleEvent::Install);
    };
    let recorded_version =
        semver::Version::parse(recorded).map_err(LifecycleDecisionError::InvalidVersion)?;

    if recorded_version == *current {
        return Ok(LifecycleEvent::None);
    }
    if recorded_version < *current {
        return Ok(LifecycleEvent::Update);
    }
    Err(LifecycleDecisionError::Downgrade {
        recorded: recorded_version,
        current: current.clone(),
    })
}

/// `<data_subdir>/config.json` — the data dir's own record (CONTRACT.md §6),
/// written by both hosts and read by both.
pub fn data_config_path(data_subdir: &Path) -> PathBuf {
    data_subdir.join("config.json")
}

/// The version `data/config.json` records, or `None` when there is no file yet.
///
/// A file that exists but does not parse is an error rather than a `None`:
/// treating it as "no record" would silently re-run an install event over a data
/// dir that already holds someone's database.
pub fn read_data_version(data_subdir: &Path) -> Result<Option<String>, LifecycleError> {
    let path = data_config_path(data_subdir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(LifecycleError::Io { path, source }),
    };

    serde_json::from_str::<DataConfig>(&contents)
        .map(|config| Some(config.version))
        .map_err(|error| LifecycleError::MalformedDataConfig {
            path,
            detail: error.to_string(),
        })
}

/// Record `version` in `data/config.json`, preserving any `port_override`
/// (CONTRACT.md §6).
///
/// Same-directory temp file plus `rename`, the standard POSIX atomic write:
/// `rename(2)` is atomic within one filesystem, so a crash or power loss can
/// only ever leave the old or the new contents in full — never a truncated
/// record that the next launch would read as a corrupt data dir.
///
/// `port_override` is read back and carried over rather than dropped: it is the
/// user's own per-installation escape hatch for a static `app_port` already
/// taken on their machine, and nothing here has any business forgetting it.
pub fn write_data_version(data_subdir: &Path, version: &str) -> Result<(), LifecycleError> {
    let path = data_config_path(data_subdir);
    let port_override = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<DataConfig>(&contents).ok())
        .and_then(|config| config.port_override);

    let config = DataConfig {
        version: version.to_string(),
        port_override,
    };
    let json = serde_json::to_string_pretty(&config).map_err(|error| {
        LifecycleError::MalformedDataConfig {
            path: path.clone(),
            detail: error.to_string(),
        }
    })?;

    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| LifecycleError::Io { path, source }
    };
    let temporary = data_subdir.join("config.json.tmp");
    fs::write(&temporary, json).map_err(io_error(&temporary))?;
    fs::rename(&temporary, &path).map_err(io_error(&path))
}

/// Run every guard, and answer with the sidecar liveness lock (CONTRACT.md §6)
/// this launch is to hold for its whole lifetime.
///
/// `None` means a live sibling already holds it: this launch is the second one
/// of the same app, it ran no guard, touched nothing, and has one job left —
/// hand its argv to the running instance and go away, which is
/// `tauri-plugin-single-instance`'s from here.
///
/// The lifecycle *event* is deliberately not returned. On the station it is,
/// because `setup` runs the event's hooks off it; here the hooks belong to the
/// hub's own `install`/`update` commands, so every use of the event is made
/// inside this function — refuse, or stamp a missing record — and handing a
/// caller a value with nothing to do would only invite one to be invented.
///
/// The reap comes first, and that ordering is load-bearing rather than
/// arbitrary: a crashed launch of this same app leaves a FrankenPHP holding its
/// static port, so reaping before the port guard frees the port the guard is
/// about to test. The other way round, the guard dead-ends on a conflict this
/// app's own corpse caused.
///
/// Every refusal below exits the process through [`fatal_startup_error`] rather
/// than returning an error, because there is no caller in a position to do
/// anything else: this runs in the app's own process, launched possibly from a
/// desktop entry where stderr goes nowhere, and the only useful outcome is a
/// dialog the user can read.
pub fn prepare_launch(
    id: &str,
    data_dir: &Path,
    data_subdir: &Path,
    identifier: &str,
    app_version: &str,
    app_port: Option<u16>,
) -> Option<fs::File> {
    let lock =
        tfsapp_core::process::cleanup_previous_sidecar(&data_dir.join("sidecar.pid"), identifier);

    // A live sibling holds the lock: this process must not run the guards, must
    // not touch the data dir, and must not bind anything.
    lock.as_ref()?;

    let event = check_version(id, data_subdir, app_version);
    // A data dir with no record at all, under an app the hub installed: the
    // install event already ran, at install time, so there is nothing to run
    // here and only a record to catch up on. Stamping it is what makes the next
    // launch quiet — and what gives a *packaged* AppImage of this same app a
    // version to compare its own guard against, since the two share this file.
    if event == LifecycleEvent::Install {
        if let Err(error) = write_data_version(data_subdir, app_version) {
            fatal_startup_error(&error.to_string());
        }
    }
    check_port(app_port, data_subdir);

    lock
}

/// The version guard: decide the event, and refuse the three outcomes an app
/// cannot be opened under.
fn check_version(id: &str, data_subdir: &Path, app_version: &str) -> LifecycleEvent {
    let config_file = data_config_path(data_subdir);

    let recorded = match read_data_version(data_subdir) {
        Ok(recorded) => recorded,
        Err(error) => fatal_startup_error(&error.to_string()),
    };
    let current = match semver::Version::parse(app_version) {
        Ok(current) => current,
        // The installer refuses a non-semver `app_version` (CONTRACT.md §2), so
        // reaching this means the installed snapshot's manifest was edited since.
        Err(error) => fatal_startup_error(&format!(
            "This app declares version {app_version:?}, which is not canonical semver \
             ({error}). It is what decides whether this data dir is up to date, so it \
             cannot be compared — reinstall the app."
        )),
    };

    // Bound rather than matched inline: the arms below move `recorded`, and the
    // borrow `as_deref()` takes would otherwise outlive the call it was made for.
    let decision = lifecycle_decision(recorded.as_deref(), &current);
    match decision {
        // The hub's own `update <id>` owns the update event and records its
        // version at the event's success point, so an installed snapshot newer
        // than the record is never an update in progress — it is one that never
        // finished, or a tree edited under the hub. See the module header for
        // why this is a refusal here and an event on the station.
        Ok(LifecycleEvent::Update) => fatal_startup_error(&format!(
            "This app's installed source is version {current}, but its data dir was last \
             written by version {}. Its update never completed, so its migrations may not \
             have run — opening it could corrupt data. Run `tfsapp-hub update {id}` to \
             finish it.",
            recorded.unwrap_or_default()
        )),
        Ok(event) => event,
        Err(LifecycleDecisionError::InvalidVersion(error)) => fatal_startup_error(&format!(
            "Invalid version in {}: {error}",
            config_file.display()
        )),
        Err(LifecycleDecisionError::Downgrade { recorded, current }) => {
            fatal_startup_error(&format!(
                "This installation's data was written by app version {recorded}, but the \
                 installed app is version {current}. Downgrading is not automated — edit {} \
                 to resolve.",
                config_file.display()
            ))
        }
    }
}

/// The static-port guard (CONTRACT.md §6).
///
/// A no-op for an app that pins no port, which is the default and the common
/// case: a dynamic port is picked free at launch and can neither claim nor lose
/// a number. For one that does pin, the number is bound and released here, before
/// FrankenPHP is asked to use it, so the failure is a message naming
/// `port_override` rather than a sidecar dying with a bind error nobody sees.
fn check_port(app_port: Option<u16>, data_subdir: &Path) {
    let port = match tfsapp_core::ports::resolve_packaged_port(app_port, data_subdir) {
        Ok(Some(port)) => port,
        Ok(None) => return,
        Err(error) => fatal_startup_error(&format!("Cannot settle the app's port: {error}")),
    };

    if let Err(error) = tfsapp_core::ports::check_packaged_port(app_port, port, data_subdir) {
        fatal_startup_error(&error.to_string());
    }
}

/// Every fatal pre-`Builder` startup error's single exit: a blocking native
/// dialog, then the same text on stderr, then exit 1.
///
/// Both, because there are two audiences and they never overlap. A user who
/// double-clicked a desktop entry has no stderr — without the dialog the app
/// just fails to open, silently, which is the worst outcome this whole module
/// exists to avoid. A developer at a terminal wants the line where they typed
/// the command, not a modal to dismiss.
///
/// Safe here, and only here: `rfd` drives GTK itself, so this is sound exactly
/// while `tauri::Builder` has not claimed it yet. Anything fatal after that
/// point needs the app's own dialog plugin, off the main thread.
pub fn fatal_startup_error(message: &str) -> ! {
    rfd::MessageDialog::new()
        .set_title("TFSApp Hub: startup error")
        .set_description(message)
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    eprintln!("tfsapp-hub: {message}");
    std::process::exit(1);
}

#[derive(Debug)]
pub enum LifecycleError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    MalformedDataConfig {
        path: PathBuf,
        detail: String,
    },
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::MalformedDataConfig { path, detail } => write!(
                formatter,
                "cannot read {}: {detail}. It records which version of the app wrote this \
                 data dir (CONTRACT.md §6); the hub will not run an app against data it \
                 cannot date.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
