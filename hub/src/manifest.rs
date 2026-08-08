//! The app's `tfsapp.config.json`, parsed at runtime.
//!
//! One schema, two hosts (station CONTRACT.md §2). The station reads this file
//! in **dev** mode and bakes most of it into `tauri.conf.json` at build time
//! for **packaged** mode; the hub has no `build-app.sh` and never will, so the
//! manifest of the installed snapshot is its only source for everything —
//! identity, port, icon, lifecycle commands, `run` aliases and the runtime
//! toggles alike.
//!
//! That last group is the one worth stating out loud. `async_worker` and
//! `actions` are read here exactly as the station's *dev* mode reads them, and
//! unlike its packaged mode, which reads them back from
//! `config.rs::baked_async_worker` / `baked_actions`. Had the hub simply not
//! looked, nothing would have failed: the unknown-key rule below would have
//! warned about a key it did not know, and the app would then have run with
//! `sync://` and `TFS_ASYNC_WORKER=0` while its station AppImage runs a real
//! worker off the same manifest. Same manifest, two behaviours, is precisely
//! what the shared contract forbids.
//!
//! Two rules govern how strict this parser is, and they are not the same rule:
//!
//! - **Unknown top-level key: warn, never reject.** It is what lets the schema
//!   grow additively — the hub may read a key the station ignores and the other
//!   way round, without either host failing on the other's file. Its corollary
//!   binds just as hard: the hub must never *require* a key the station does
//!   not know.
//! - **Known key, wrong type: reject, naming the file and the field.** This is
//!   where `build-app.sh`'s build-time JSON validation lands for the hub, since
//!   there is no build step to catch it earlier. A quoted `"async_worker":
//!   "true"` is not a forward-compatible extra, it is a manifest that means the
//!   opposite of what its author believes.

// Consumed by the installer (plan 006) and by `open` (007); until those land,
// this module's own tests are its only callers. Remove the allow with the
// first real consumer rather than letting it linger.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// The manifest's filename at the project root — the station's own constant
/// spelled out, since a mismatch would silently split the two hosts' idea of
/// what an app is.
pub const MANIFEST_FILE: &str = "tfsapp.config.json";

/// Top-level keys `tfsapp.config.json` defines (CONTRACT.md §2).
///
/// Kept in sync with the station's `config::KNOWN_PROJECT_CONFIG_KEYS`, and
/// deliberately *wider* than the fields [`Manifest`] parses: `splash_bg`,
/// `splash_text` and `releases_repo` are meaningful to the station and not yet
/// to the hub, and warning about them would turn "the hub does not use this
/// yet" into "your manifest looks wrong". Anything outside this list is far
/// more likely a typo (`"app-port"` for `"app_port"`) than a deliberate
/// extension, which is what the warning is for.
pub const KNOWN_KEYS: &[&str] = &[
    "product_name",
    "identifier",
    "project_name",
    "app_version",
    "app_port",
    "icon_path",
    "splash_path",
    "splash_bg",
    "splash_text",
    "commands",
    "async_worker",
    "actions",
    "releases_repo",
    "run",
];

/// The four fields with no sane default: an app that declares none of them has
/// no identity, and every downstream key — data dir, keyring namespace, window
/// identity, lifecycle decision — hangs off one of them.
const REQUIRED_KEYS: &[&str] = &["product_name", "identifier", "project_name", "app_version"];

/// A parsed `tfsapp.config.json`.
///
/// Field-for-field the station's `config::ProjectConfig`, plus the keys that
/// struct deliberately omits because *dev mode* has no use for them
/// (`commands`, `run`, `icon_path`, `splash_path`) — the hub does, since it is
/// the one running the packaged-mode responsibilities without a packaging step.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Human-readable name. Feeds the window title *and* the generated
    /// `.desktop` `Name=` from this one field — the identity spike watched
    /// those two diverge on the station, where the window title reaches the
    /// binary only through a build-time bake with a silent fallback.
    pub product_name: String,
    /// Reverse-domain identity. The data dir, the keyring namespace, the GTK
    /// app id, the D-Bus name, the single-instance key and `WM_CLASS` all
    /// derive from it — see `identity.rs`.
    pub identifier: String,
    /// Machine-friendly slug. Not an identity key: the hub's own CLI handle is
    /// the registry's `id`, which the installer derives but does not have to
    /// equal this.
    pub project_name: String,
    /// The app's own release version, and the lifecycle authority for
    /// install/update/downgrade (CONTRACT.md §6) — never the git ref, which is
    /// only a source selector. Kept as a string here rather than a
    /// `semver::Version`: the contract requires canonical semver but only
    /// `build-app.sh` enforces it, so a hub that rejected a non-semver value
    /// would refuse an app the station happily runs in dev mode. The installer
    /// parses it where a comparison is actually needed.
    pub app_version: String,
    /// Pins `APP_PORT` instead of a fresh dynamic loopback port each launch.
    /// Recorded in the registry too, so the installer can flag a collision
    /// between two apps that both pinned the same port.
    #[serde(default)]
    pub app_port: Option<u16>,
    /// Project-root-relative path to the app's square PNG. The hub resolves it
    /// inside the installed snapshot and decodes it at launch, because it has
    /// no per-app build step to bake an icon into.
    #[serde(default)]
    pub icon_path: Option<String>,
    /// Project-root-relative path to the cold-start splash page. Parsed, and —
    /// so far — not honoured: the hub shows its own splash page and says so at
    /// launch. See `window::splash_style` for why, and for what *is* honoured.
    #[serde(default)]
    pub splash_path: Option<String>,
    /// The splash's background and text colours. Honoured: the hub's own splash
    /// page reads them as CSS variables, so an app's cold start carries its own
    /// palette even though the page itself is the hub's.
    #[serde(default)]
    pub splash_bg: Option<String>,
    #[serde(default)]
    pub splash_text: Option<String>,
    /// Launch-time lifecycle commands (CONTRACT.md §2/§6). The station's dev
    /// mode ignores this key — there is no versioned data dir to install or
    /// update — while the hub is exactly the host that has one.
    #[serde(default)]
    pub commands: LifecycleCommands,
    /// Named `bin/console` aliases a user runs directly. Packaged-only on the
    /// station, hub-relevant from the moment `run <id> <alias>` exists.
    #[serde(default)]
    pub run: BTreeMap<String, RunAlias>,
    /// Which native capability groups are reachable, over which transports.
    /// Absent means every group off — read from the manifest, as dev mode does.
    #[serde(default)]
    pub actions: ActionsConfig,
    /// Opts into a supervised Messenger worker and a `doctrine://` transport.
    /// Absent is `false`, and a non-boolean is a parse error rather than a
    /// silent `false` — see this module's header.
    #[serde(default)]
    pub async_worker: bool,
}

/// The four launch-time lifecycle command lists (CONTRACT.md §2/§6):
/// `bin/console` argument strings, split on whitespace by the runner, no shell
/// interpretation.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct LifecycleCommands {
    #[serde(rename = "pre-install", default)]
    pub pre_install: Vec<String>,
    #[serde(rename = "post-install", default)]
    pub post_install: Vec<String>,
    #[serde(rename = "pre-update", default)]
    pub pre_update: Vec<String>,
    #[serde(rename = "post-update", default)]
    pub post_update: Vec<String>,
}

/// One declared `run` alias: a `bin/console` argument string plus a
/// `concurrent` permission bit deciding whether it may start while a window of
/// the same app is already live.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct RunAlias {
    pub command: String,
    #[serde(default)]
    pub concurrent: bool,
}

/// `actions` (CONTRACT.md §2): one entry per native capability group, every
/// level defaulted so an absent `actions`, an absent group and an absent
/// transport all mean off.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct ActionsConfig {
    #[serde(default)]
    pub secrets: SecretsActions,
    #[serde(default)]
    pub update: UpdateActions,
}

/// The `secrets` group, per transport. `keys` is a manifest — typo-catching,
/// enumerability, a spam cap — never a security boundary on its own.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct SecretsActions {
    #[serde(default)]
    pub ipc: bool,
    #[serde(default)]
    pub bridge: bool,
    #[serde(default)]
    pub keys: Vec<String>,
}

/// The `update` group, per transport. No `keys`: the update check has one
/// outcome shape, not a set of named resources.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct UpdateActions {
    #[serde(default)]
    pub ipc: bool,
    #[serde(default)]
    pub bridge: bool,
}

/// A manifest and whatever the parse wanted to say about it.
///
/// The warnings are returned rather than printed so they can be asserted in a
/// test and routed by the caller (a `--quiet` install, a GUI later). Printing
/// them is [`Loaded::report_warnings`], one line each.
#[derive(Debug)]
pub struct Loaded {
    pub manifest: Manifest,
    pub warnings: Vec<String>,
}

impl Loaded {
    /// Print every warning to stderr, so an install's own output stays clean
    /// on stdout.
    pub fn report_warnings(&self) {
        for warning in &self.warnings {
            eprintln!("tfsapp-hub: warning: {warning}");
        }
    }
}

impl Manifest {
    /// The runtime identity this manifest describes, with `icon_path` resolved
    /// inside `app_dir` — the installed snapshot, whose path only the caller
    /// knows.
    ///
    /// One field feeds one surface, by construction: the window title and the
    /// generated `.desktop` `Name=` both read `product_name` from here, with no
    /// bake and no fallback in between, so the divergence the identity spike
    /// saw on the station cannot reproduce.
    pub fn identity(&self, app_dir: &Path) -> crate::identity::Identity {
        crate::identity::Identity {
            identifier: self.identifier.clone(),
            product_name: self.product_name.clone(),
            icon_path: self
                .icon_path
                .as_ref()
                .map(|relative| app_dir.join(relative)),
        }
    }
}

/// Read and parse `<project_path>/tfsapp.config.json`.
///
/// `project_path` is the directory holding the manifest — a source tree at
/// install time, an installed snapshot at launch time — matching the station's
/// `load_project_config` so the two hosts take the same argument.
pub fn load(project_path: &Path) -> Result<Loaded, ManifestError> {
    let path = project_path.join(MANIFEST_FILE);
    let contents = fs::read_to_string(&path).map_err(|source| ManifestError::Unreadable {
        path: path.clone(),
        source,
    })?;
    parse(&path, &contents)
}

/// The pure half of [`load`]: everything but the read, so the whole decision
/// table is testable from a string.
pub fn parse(path: &Path, contents: &str) -> Result<Loaded, ManifestError> {
    let value: serde_json::Value =
        serde_json::from_str(contents).map_err(|error| ManifestError::Malformed {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

    let object = value.as_object().ok_or_else(|| ManifestError::Malformed {
        path: path.to_path_buf(),
        detail: format!(
            "the file must contain a JSON object, found {}",
            json_type(&value)
        ),
    })?;

    for key in REQUIRED_KEYS {
        match object.get(*key) {
            None => {
                return Err(ManifestError::MissingField {
                    path: path.to_path_buf(),
                    field: key,
                })
            }
            Some(serde_json::Value::String(text)) if text.trim().is_empty() => {
                return Err(ManifestError::MissingField {
                    path: path.to_path_buf(),
                    field: key,
                })
            }
            Some(serde_json::Value::String(_)) => {}
            Some(other) => {
                return Err(ManifestError::WrongType {
                    path: path.to_path_buf(),
                    field: key,
                    expected: "a string",
                    found: json_type(other),
                })
            }
        }
    }

    // The one type check that cannot be left to serde: it would report
    // `invalid type: string "true", expected a boolean` without ever naming
    // the field, on a key whose whole failure mode is being silently wrong.
    if let Some(value) = object.get("async_worker") {
        if !value.is_boolean() {
            return Err(ManifestError::WrongType {
                path: path.to_path_buf(),
                field: "async_worker",
                expected: "a boolean",
                found: json_type(value),
            });
        }
    }

    let warnings = object
        .keys()
        .filter(|key| !KNOWN_KEYS.contains(&key.as_str()))
        .map(|key| {
            format!(
                "unknown key \"{key}\" in {} — check for a typo (CONTRACT.md §2). \
                 It is ignored, not rejected.",
                path.display()
            )
        })
        .collect();

    let manifest = serde_json::from_value(value).map_err(|error| ManifestError::Invalid {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;

    Ok(Loaded { manifest, warnings })
}

/// What a JSON value is, in words an error message can use.
fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Every way a manifest can fail to be one. Each variant carries the file, so
/// no caller has to remember to add it: with N apps installed, "invalid
/// manifest" without a path is not an error message.
#[derive(Debug)]
pub enum ManifestError {
    Unreadable {
        path: PathBuf,
        source: io::Error,
    },
    Malformed {
        path: PathBuf,
        detail: String,
    },
    MissingField {
        path: PathBuf,
        field: &'static str,
    },
    WrongType {
        path: PathBuf,
        field: &'static str,
        expected: &'static str,
        found: &'static str,
    },
    Invalid {
        path: PathBuf,
        detail: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, source } => write!(
                formatter,
                "cannot read {}: {source}. Every app must carry a {MANIFEST_FILE} \
                 (CONTRACT.md §2).",
                path.display()
            ),
            Self::Malformed { path, detail } => {
                write!(formatter, "invalid JSON in {}: {detail}", path.display())
            }
            Self::MissingField { path, field } => write!(
                formatter,
                "{} declares no \"{field}\" — it is required and must be a non-empty \
                 string (CONTRACT.md §2).",
                path.display()
            ),
            Self::WrongType {
                path,
                field,
                expected,
                found,
            } => write!(
                formatter,
                "\"{field}\" in {} must be {expected}, found {found} (CONTRACT.md §2).",
                path.display()
            ),
            Self::Invalid { path, detail } => {
                write!(formatter, "invalid {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
