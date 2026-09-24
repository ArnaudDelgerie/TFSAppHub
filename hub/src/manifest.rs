//! The app's `tfsapp.config.json`, parsed at runtime.
//!
//! The hub reads this contract-defined file at runtime. The manifest of the
//! installed snapshot is its source for everything —
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
//! - **Unknown top-level key: warn, never reject.** It lets the schema grow
//!   additively while still catching likely typos.
//! - **Known key, wrong type: reject, naming the file and the field.** A quoted
//!   `"async_worker":
//!   "true"` is not a forward-compatible extra, it is a manifest that means the
//!   opposite of what its author believes.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// The contract-defined manifest filename at the project root.
pub const MANIFEST_FILE: &str = "tfsapp.config.json";

/// Top-level keys `tfsapp.config.json` defines (CONTRACT.md §2).
///
/// `splash_bg`, `splash_text` and `splash_path` are all honoured by the hub.
/// `releases_repo` remains a known, inert legacy key so old manifests do not
/// produce a warning. Anything outside this list is far more likely a typo
/// (`"app-port"` for `"app_port"`) than a deliberate extension, which is what
/// the warning is for.
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
    "workers",
    "actions",
    "releases_repo",
    "run",
];

/// Cap on a `workers[]` declaration's `count` (the contract's
/// fall-back-and-say-so rule, CONTRACT.md §2). `DATABASE_URL` is always
/// SQLite, which serializes writers regardless of how many consumers are
/// running, so a count above this buys nothing and only adds contention.
pub const WORKER_COUNT_CAP: u8 = 4;

/// The four fields with no sane default: an app that declares none of them has
/// no identity, and every downstream key — data dir, keyring namespace, window
/// identity, lifecycle decision — hangs off one of them.
const REQUIRED_KEYS: &[&str] = &["product_name", "identifier", "project_name", "app_version"];

/// A parsed `tfsapp.config.json`.
///
/// The fields the hub needs to install, launch and serve a project.
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
    /// `semver::Version`: `install::validate` enforces the contract's canonical
    /// semver rule before a value is compared or recorded.
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
    /// Project-root-relative path to the cold-start splash page. When it is
    /// readable inside the snapshot, the hub serves it over its scoped splash
    /// scheme; otherwise it uses the bundled fallback page.
    #[serde(default)]
    pub splash_path: Option<String>,
    /// The splash's background and text colours. Honoured: the hub's own splash
    /// page reads them as CSS variables, so an app's cold start carries its own
    /// palette even though the page itself is the hub's.
    #[serde(default)]
    pub splash_bg: Option<String>,
    #[serde(default)]
    pub splash_text: Option<String>,
    /// Launch-time lifecycle commands (CONTRACT.md §2/§6).
    #[serde(default)]
    pub commands: LifecycleCommands,
    /// Named `bin/console` aliases a user runs directly with `run <id> <alias>`.
    #[serde(default)]
    pub run: BTreeMap<String, RunAlias>,
    /// Which native capability groups are reachable, over which transports.
    /// Absent means every group off — read from the manifest, as dev mode does.
    #[serde(default)]
    pub actions: ActionsConfig,
    /// Opts into a supervised Messenger worker and a `doctrine://` transport.
    /// Absent is `false`, and a non-boolean is a parse error rather than a
    /// silent `false` — see this module's header. Sugar for a single
    /// declaration consuming `async`, desugared into `workers` by [`parse`];
    /// a manifest may spell this key or `workers`, never both.
    #[serde(default)]
    pub async_worker: bool,
    /// One or more supervised consumers, each an ordered, non-empty transport
    /// list plus an optional copy count (CONTRACT.md §2, plan 045). The order
    /// of a declaration's transports is its priority: `messenger:consume`
    /// rescans from the first transport after every envelope, so a long
    /// queued run cannot starve short interactive work on the same consumer.
    /// Empty means no consumer declared, the same as `async_worker` absent —
    /// [`parse`] desugars `async_worker: true` into this field, so downstream
    /// code reads `workers` alone once a manifest has loaded.
    #[serde(default)]
    pub workers: Vec<WorkerDeclaration>,
    /// A legacy key accepted but never read (CONTRACT.md §2's "Keys this
    /// contract does not define"). `publish` uses `--repo` or the project's
    /// git remote; install and update never let a source steer its own fetch.
    #[serde(default)]
    pub releases_repo: Option<String>,
}

/// One declared worker: an ordered, non-empty transport list plus how many
/// copies to run — CONTRACT.md §2, plan 045. `count` above
/// [`WORKER_COUNT_CAP`] and a `scheduler_*` transport asked for more than
/// once both fall back rather than refuse or silently clamp; see
/// [`apply_worker_fallbacks`].
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct WorkerDeclaration {
    pub transports: Vec<String>,
    #[serde(default = "default_worker_count")]
    pub count: u8,
}

fn default_worker_count() -> u8 {
    1
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
    #[serde(default)]
    pub picker: PickerActions,
    #[serde(default)]
    pub close_guard: CloseGuardActions,
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

/// The `picker` group is intentionally IPC-only. The native chooser belongs to
/// a visible webview and its owning window; no PHP process receives a route or
/// environment value through which it could open one.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct PickerActions {
    #[serde(default)]
    pub ipc: bool,
}

/// The `close_guard` group (plan 055), per transport: both are real, both are
/// off by default, and neither implies the other. `ipc` is the webview's
/// register/remove commands for a document's unsaved-work guards; `bridge` is
/// the authenticated HTTP routes for app-instance backend-work guards. The
/// two namespaces are separate by construction — see `close_guard.rs`.
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct CloseGuardActions {
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
/// install time or an installed snapshot at launch time.
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

    // Two spellings of one thing: guessing which wins is worse than a
    // refusal, so a manifest declaring both is rejected outright rather than
    // desugared one way or the other.
    if object.contains_key("async_worker") && object.contains_key("workers") {
        return Err(ManifestError::ConflictingWorkerDeclaration {
            path: path.to_path_buf(),
        });
    }

    if let Some(value) = object.get("workers") {
        validate_workers_shape(path, value)?;
    }

    // Unlike a new top-level key, `actions.picker.bridge` cannot be safely
    // ignored: this group has no bridge transport, so accepting it would make
    // a future-looking manifest appear to grant something it never can.
    if object
        .get("actions")
        .and_then(serde_json::Value::as_object)
        .and_then(|actions| actions.get("picker"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(|picker| picker.contains_key("bridge"))
    {
        return Err(ManifestError::UnsupportedActionTransport {
            path: path.to_path_buf(),
            group: "picker",
            transport: "bridge",
        });
    }

    let mut warnings: Vec<String> = object
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

    let mut manifest: Manifest =
        serde_json::from_value(value).map_err(|error| ManifestError::Invalid {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

    if let Some(transport) = duplicate_worker_transport(&manifest.workers) {
        return Err(ManifestError::DuplicateWorkerTransport {
            path: path.to_path_buf(),
            transport,
        });
    }

    // The sugar this key has always been: one declaration, one transport,
    // the same shape `workers` would spell by hand. Only reachable when
    // `workers` itself is absent — the both-keys check above already refused
    // the alternative.
    if manifest.async_worker && manifest.workers.is_empty() {
        manifest.workers = vec![WorkerDeclaration {
            transports: vec!["async".to_string()],
            count: 1,
        }];
    }

    warnings.extend(apply_worker_fallbacks(&mut manifest.workers));

    Ok(Loaded { manifest, warnings })
}

/// The type checks `serde`'s own error cannot name a field for (this
/// module's header): `workers` must be an array, each element an object,
/// `transports` a non-empty array of non-empty strings, `count` — when
/// present — a positive integer.
fn validate_workers_shape(path: &Path, value: &serde_json::Value) -> Result<(), ManifestError> {
    let invalid = |detail: String| ManifestError::WorkersInvalid {
        path: path.to_path_buf(),
        detail,
    };

    let declarations = value.as_array().ok_or_else(|| {
        invalid(format!(
            "\"workers\" must be an array, found {}",
            json_type(value)
        ))
    })?;

    for (index, declaration) in declarations.iter().enumerate() {
        let object = declaration.as_object().ok_or_else(|| {
            invalid(format!(
                "workers[{index}] must be an object, found {}",
                json_type(declaration)
            ))
        })?;

        let transports = object
            .get("transports")
            .ok_or_else(|| invalid(format!("workers[{index}].transports is required")))?;
        let transports = transports
            .as_array()
            .filter(|array| !array.is_empty())
            .ok_or_else(|| {
                invalid(format!(
                    "workers[{index}].transports must be a non-empty array of non-empty \
                     strings, found {}",
                    json_type(transports)
                ))
            })?;
        for (transport_index, transport) in transports.iter().enumerate() {
            let is_non_empty_string = transport
                .as_str()
                .is_some_and(|text| !text.trim().is_empty());
            if !is_non_empty_string {
                return Err(invalid(format!(
                    "workers[{index}].transports[{transport_index}] must be a non-empty \
                     string, found {}",
                    json_type(transport)
                )));
            }
        }

        if let Some(count) = object.get("count") {
            let is_positive_integer = count
                .as_u64()
                .is_some_and(|n| (1..=u8::MAX as u64).contains(&n));
            if !is_positive_integer {
                return Err(invalid(format!(
                    "workers[{index}].count must be a positive integer, found {}",
                    json_type(count)
                )));
            }
        }
    }

    Ok(())
}

/// The first transport spelled by more than one declaration, if any — a
/// consumer is what `count` spells, so this is refused rather than
/// fallen back on, unlike the two cases below.
fn duplicate_worker_transport(workers: &[WorkerDeclaration]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    for declaration in workers {
        for transport in &declaration.transports {
            if !seen.insert(transport.as_str()) {
                return Some(transport.clone());
            }
        }
    }
    None
}

/// Apply CONTRACT.md's fall-back-and-say-so rule to every declaration,
/// in place, and return one printable reason per fallback that fired.
///
/// A `count` above [`WORKER_COUNT_CAP`] falls back to the cap: `DATABASE_URL`
/// is always SQLite, which serializes writers regardless, so a count above it
/// only adds contention. A declaration naming a `scheduler_*` transport falls
/// back to `count: 1` whatever it asked for: a Scheduler transport consumed
/// by more than one worker at once fires every due task more than once.
fn apply_worker_fallbacks(workers: &mut [WorkerDeclaration]) -> Vec<String> {
    let mut warnings = Vec::new();
    for (index, declaration) in workers.iter_mut().enumerate() {
        if declaration.count > WORKER_COUNT_CAP {
            warnings.push(format!(
                "workers[{index}].count {} exceeds the cap of {WORKER_COUNT_CAP}; using \
                 {WORKER_COUNT_CAP} instead (CONTRACT.md §2).",
                declaration.count
            ));
            declaration.count = WORKER_COUNT_CAP;
        }
        if declaration.count > 1
            && declaration
                .transports
                .iter()
                .any(|transport| transport.starts_with("scheduler_"))
        {
            warnings.push(format!(
                "workers[{index}] consumes a scheduler transport with count {}; a Scheduler \
                 transport consumed more than once fires every due task more than once, so \
                 count is set to 1 instead (CONTRACT.md §2).",
                declaration.count
            ));
            declaration.count = 1;
        }
    }
    warnings
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
    UnsupportedActionTransport {
        path: PathBuf,
        group: &'static str,
        transport: &'static str,
    },
    ConflictingWorkerDeclaration {
        path: PathBuf,
    },
    WorkersInvalid {
        path: PathBuf,
        detail: String,
    },
    DuplicateWorkerTransport {
        path: PathBuf,
        transport: String,
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
            Self::UnsupportedActionTransport {
                path,
                group,
                transport,
            } => write!(
                formatter,
                "\"actions.{group}.{transport}\" in {} is not supported: actions.{group} has no {transport} transport (CONTRACT.md §7).",
                path.display()
            ),
            Self::ConflictingWorkerDeclaration { path } => write!(
                formatter,
                "{} declares both \"async_worker\" and \"workers\" — spell one, not both \
                 (CONTRACT.md §2).",
                path.display()
            ),
            Self::WorkersInvalid { path, detail } => write!(
                formatter,
                "\"workers\" in {} is invalid: {detail} (CONTRACT.md §2).",
                path.display()
            ),
            Self::DuplicateWorkerTransport { path, transport } => write!(
                formatter,
                "\"workers\" in {} declares transport \"{transport}\" more than once — two \
                 consumers on one transport is what \"count\" spells, not two declarations \
                 (CONTRACT.md §2).",
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
