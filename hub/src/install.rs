//! `tfsapp-hub install <source>` — the command that makes the hub a hub.
//!
//! The pipeline, in the order it runs and the order this module reads:
//!
//! 1. resolve the source into a directory (`source::resolve`),
//! 2. validate that directory is an app at all,
//! 3. settle the hub-local `id` and refuse a collision,
//! 4. copy the tree into `apps/<id>/`,
//! 5. run the app's dependency install and lifecycle hooks with the bundled PHP,
//! 6. register it — last, and only once everything above succeeded.
//!
//! Two rules shape every step.
//!
//! **Install is a snapshot, always.** The tree is *copied*; editing the original
//! source afterwards changes nothing until an explicit `update`. There is no
//! symlink into the working tree and no watcher — that is what makes `composer
//! install`, migrations and cache warm-up mean anything, since they ran against
//! *this* tree and it cannot move underneath them. Live editing is the
//! station's `make tauri-dev`, and the hub must not grow a second, worse
//! version of it.
//!
//! **Every step is restartable.** A failure leaves no half-installed app: the
//! copied directory is removed, and the registry is written only at the very
//! end. Which is also why registration is step 6 and not step 4 — an entry
//! pointing at a tree whose `composer install` failed would be a `ready` app
//! that cannot run.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    app_env::{self, EnvError},
    cli::{EXIT_FAILED, EXIT_OK},
    desktop, hub_bin,
    lifecycle::{self, LifecycleDecisionError, LifecycleError, LifecycleEvent},
    manifest::{self, Loaded, Manifest, ManifestError, MANIFEST_FILE},
    paths::{Paths, PathsError},
    php::{self, PhpError, Toolchain},
    platform::{self, PlatformError},
    prompt,
    registry::{self, Registry, RegistryEntry, RegistryError, State},
    source::{self, SourceError},
};

/// The whole command: install `source`, or say why not. Returns the process's
/// exit code.
#[allow(clippy::too_many_arguments)]
pub fn run(
    source: &str,
    id: Option<&str>,
    reference: Option<&str>,
    assume_yes: bool,
    no_desktop_entry: bool,
    hub_version: &str,
) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match install(
        &paths,
        source,
        id,
        reference,
        assume_yes,
        no_desktop_entry,
        hub_version,
    ) {
        Ok(Some(id)) => {
            println!("Installed {id}. `tfsapp-hub list` shows it.");
            EXIT_OK
        }
        // Declining is not a failure of the command, but the app is not
        // installed either — a script reading 0 would conclude it is.
        Ok(None) => EXIT_FAILED,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// The pipeline, in order. Returns the `id` the app was installed under.
///
/// Takes its `Paths` rather than resolving them, which is what lets the whole
/// pipeline — copy, Composer, hooks, cleanup — run against a throwaway root in
/// a test instead of the developer's real `~/.local/share/TFSApp/`.
#[allow(clippy::too_many_arguments)]
fn install(
    paths: &Paths,
    source: &str,
    id: Option<&str>,
    reference: Option<&str>,
    assume_yes: bool,
    no_desktop_entry: bool,
    hub_version: &str,
) -> Result<Option<String>, InstallError> {
    let resolved = source::resolve(&source::classify(source), reference)?;
    let loaded = validate(&resolved.root)?;
    // Before anything is written, so a typo in a key is read next to the source
    // it came from rather than after a hundred megabytes of copying.
    loaded.report_warnings();
    let manifest = &loaded.manifest;

    let id = resolve_id(id, manifest)?;
    // One read of the registry for both gates. The window between this read and
    // the write at the end is real but narrow, and the write itself takes the
    // lock — two racing installs cannot corrupt the file, at worst the second
    // one's collision check was a moment stale.
    let installed = registry::load(paths)?;
    check_id_free(&installed, paths, &id)?;
    check_identifier_free(&installed, &manifest.identifier)?;
    check_port_free(&installed, manifest.app_port)?;

    // The data directory this install would write into — `identifier`-keyed,
    // read but never created here: a gate that creates the directory it is
    // inspecting would leave a trace behind a refusal. `remove` without
    // `--purge` is what usually puts something here for a *different*,
    // no-longer-registered `id` to walk into.
    let data_dir = paths.app_data_dir(&manifest.identifier)?;
    // The third writer's own version of the guard `open` and `run` already
    // enforce: nothing may install into a data directory a live window or an
    // active `run` command still owns.
    check_data_dir_available(&id, &data_dir)?;
    // Which lifecycle event (CONTRACT.md §6) this install may run, decided
    // against whatever version record survived a `remove`.
    let recorded = lifecycle::read_data_version(&data_dir.join("data"))?;
    let event = lifecycle_event_for_install(recorded.as_deref(), &manifest.app_version, &data_dir)?;

    // Resolved before the copy — and before the question, since an install
    // nothing could finish is not worth asking about. The fingerprint comes
    // from the same interpreter that is about to run the app's PHP, which is
    // exactly what the registry has to record.
    let toolchain = php::toolchain(paths)?;
    let platform = platform::probe(&toolchain.frankenphp)?.fingerprint();

    let app_dir = paths.app_dir(&id)?;
    announce(
        &id,
        manifest,
        &resolved,
        &app_dir,
        paths,
        no_desktop_entry,
        recorded.as_deref(),
    )?;
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was installed.");
        return Ok(None);
    }
    snapshot(&resolved.root, &app_dir)?;

    // Everything from here runs the app's own PHP, so everything from here can
    // fail in ways the hub does not control. One place to undo the copy, rather
    // than an `if` after each step.
    if let Err(error) = prepare(paths, &toolchain, manifest, &app_dir, event) {
        let _ = fs::remove_dir_all(&app_dir);
        return Err(error);
    }

    // Last, and under the registry's own lock: an entry written any earlier
    // would name a `ready` app whose dependencies had not resolved yet, and
    // there is no state in the registry for "installed, but do not run it".
    let now = registry::now_timestamp();
    let entry = RegistryEntry {
        id: id.clone(),
        identifier: manifest.identifier.clone(),
        source: resolved.source,
        app_version: manifest.app_version.clone(),
        source_revision: resolved.revision,
        app_port: manifest.app_port,
        platform: platform.clone(),
        state: State::Ready,
        installed_at: now.clone(),
        updated_at: now,
        unknown: serde_json::Map::new(),
    };
    registry::update(paths, |registry| {
        registry.stamp(hub_version, platform);
        registry.upsert(entry);
    })?;

    // Last of all, and best-effort: an app whose entry could not be written
    // is still installed and still usable from the CLI, the entry is only a
    // convenience, and a failure here must not undo the install above it.
    if !no_desktop_entry {
        write_desktop_entry(paths, &id, manifest, &app_dir);
    }

    Ok(Some(id))
}

/// Refresh the stable hub copy and (re)write this app's desktop entry.
///
/// Both steps are best-effort: a failure is a warning naming the path and the
/// cause, never a reason to fail an install that has already succeeded.
fn write_desktop_entry(paths: &Paths, id: &str, manifest: &Manifest, app_dir: &Path) {
    if let Err(error) = hub_bin::ensure_current(paths) {
        eprintln!(
            "tfsapp-hub: warning: could not refresh the stable hub copy at {}: {error}",
            paths.hub_executable_path().display()
        );
    }

    let identity = manifest.identity(app_dir);
    match desktop::write(id, &identity, &paths.hub_executable_path(), paths) {
        Ok(path) => println!("Desktop entry: {}", path.display()),
        Err(error) => eprintln!("tfsapp-hub: warning: could not write the desktop entry: {error}"),
    }
}

/// Say what is about to happen, in the terms the user will have to reason about
/// afterwards: the app, where it comes from, where it lands, and where its data
/// will live.
///
/// The warning is not boilerplate and must not be softened. Installing runs the
/// app's own PHP on this machine — Composer's resolution, every script it
/// fires, the app's own install commands — and there is no sandbox anywhere in
/// this hub. The honest comparison is `composer require`, which is the same
/// trust a developer already gives daily; saying so is more useful than a
/// warning nobody believes.
fn announce(
    id: &str,
    manifest: &Manifest,
    resolved: &source::Resolved,
    app_dir: &Path,
    paths: &Paths,
    no_desktop_entry: bool,
    recorded: Option<&str>,
) -> Result<(), InstallError> {
    println!(
        "Install {} {} as \"{id}\":",
        manifest.product_name, manifest.app_version
    );
    println!("  from      {}", resolved.root.display());
    println!("  into      {}", app_dir.display());
    println!(
        "  data dir  {}",
        paths.app_data_dir(&manifest.identifier)?.display()
    );
    // Named so the confirmation below is one the user can actually answer:
    // by this point the version gate already refused a mismatch, so the only
    // record left to see here is one an equal reinstall runs no hook over.
    if let Some(recorded) = recorded {
        println!("            already holds data written by version {recorded}");
    }
    if !no_desktop_entry {
        println!(
            "  entry     {}",
            paths.desktop_entry_path(&manifest.identifier)?.display()
        );
        println!("  hub path  {}", paths.hub_executable_path().display());
    }
    println!();
    println!(
        "This runs the app's own PHP on your machine: Composer's dependency\n\
         resolution, the scripts it fires, and the app's own install commands.\n\
         There is no sandbox — it is the same trust you give `composer require`."
    );
    Ok(())
}

/// Bring the copied tree to a state the app can run from: its dependencies,
/// then — only when `event` is [`LifecycleEvent::Install`] — its own
/// install-time lifecycle commands.
///
/// **Which hooks run here, and why these.** The hub's `install` is the
/// contract's install event (CONTRACT.md §6) only when the data directory
/// says so: `event` was decided by `lifecycle_event_for_install`, above this
/// function, against the data `remove` (without `--purge`) may have left
/// behind. An [`LifecycleEvent::Install`] runs `pre-install` then
/// `post-install`, in that order; [`LifecycleEvent::None`] — an equal
/// record, the reinstall-after-`remove` path — runs neither, exactly as
/// CONTRACT.md §6 states it. The event can never resolve to `Update` here:
/// `lifecycle_event_for_install` already turned that case into a refusal
/// before `prepare` was ever called, so `pre-update`/`post-update` stay
/// unreachable from `install`, which is not their event.
///
/// The station runs `post-install` after `/healthz` answers `200`, because over
/// there the install event happens *during a launch* and there is a sidecar up
/// by then. Here there is not: install is its own moment, with no window and no
/// server. For the ordinary contents of that hook — a cache warm, an `about` —
/// it makes no difference; for one that expects to reach its own app over HTTP
/// it does. CONTRACT.md §6 says so plainly rather than leaving it to be
/// discovered: a `post-` command may not assume its own app is reachable.
fn prepare(
    paths: &Paths,
    toolchain: &Toolchain,
    manifest: &Manifest,
    app_dir: &Path,
    event: LifecycleEvent,
) -> Result<(), InstallError> {
    // `0700` on every install, not only on the first: an app reinstalled after
    // an older host created it laxly gets tightened here rather than staying
    // that way forever (`paths::create_app_data_dir`'s own doc).
    let state_root = paths.create_app_data_dir(&manifest.identifier)?;
    let environment = app_env::resolve(
        manifest,
        app_dir,
        &manifest.identifier,
        &state_root,
        app_env::Mode::Install,
    )?;
    // Named before the first command runs, because the next thing on screen is
    // a migration writing a database into it — under `identifier`, which is
    // what makes it the same data dir a packaged install of this app uses.
    println!("Its data lives in {}", environment.data_dir.display());

    // Runs whichever event this is: dependencies are the freshly copied tree's
    // own, not a lifecycle command, and an app with none to install still
    // needs its vendor dir populated.
    toolchain.composer_install(app_dir, &environment.vars)?;

    if event == LifecycleEvent::Install {
        for command in manifest
            .commands
            .pre_install
            .iter()
            .chain(&manifest.commands.post_install)
        {
            toolchain.console(app_dir, &environment.vars, command)?;
        }
    }

    // The install event's success point (CONTRACT.md §6): the data dir records
    // which version of the app last wrote it, and it is written **only** once
    // every hook above has succeeded — a failed install leaves the dir undated,
    // so the next attempt starts the whole event over rather than believing it
    // already ran. Runs whichever event this is, same as `composer_install`
    // above: an equal record is rewritten to the same value, preserving
    // `port_override` exactly as `write_data_version` already does.
    //
    // This is the record `open`'s version guard reads, and the same file a
    // packaged AppImage of this app writes and reads: one data dir, one record,
    // whichever host wrote it.
    lifecycle::write_data_version(&environment.data_subdir, &manifest.app_version)?;

    Ok(())
}

/// Paths, relative to the project root, the snapshot never copies.
///
/// Two of them are the app's own runtime droppings (`var/cache` is rewritten on
/// every request, `var/log` grows without bound) and the third is the station's
/// `make build` output — a ~170 MB AppImage that has no business being
/// installed as source. Matched as whole relative paths, so a project with its
/// own `src/var/log/` keeps it.
const EXCLUDED_PATHS: &[&str] = &["var/cache", "var/log", "tfsapp_build"];

/// Directory names the snapshot never copies, at any depth.
///
/// By name rather than by path because both are legitimately nested: a `.git`
/// below the root is a vendored repository or a submodule, and `node_modules/`
/// sits wherever a package.json does. Neither is source, and both are
/// regenerable from something that is.
const EXCLUDED_NAMES: &[&str] = &[".git", "node_modules"];

/// Files an app must carry for the hub to be able to run it at all
/// (CONTRACT.md §1), plus the `composer.json` the dependency install needs.
///
/// Checked before the copy rather than discovered during it: the alternative is
/// a hundred-megabyte copy followed by "failed to start bin/console", which
/// names the symptom and not the cause.
const REQUIRED_FILES: &[(&str, &str)] = &[
    (
        "composer.json",
        "the hub installs the app's dependencies with its own PHP, and Composer \
         needs one",
    ),
    (
        "bin/console",
        "the app's lifecycle commands are `bin/console` invocations (CONTRACT.md §1)",
    ),
    (
        "public/index.php",
        "it is the front controller the app is served from (CONTRACT.md §1)",
    ),
];

/// Settle the hub-local handle this app is installed under.
///
/// `--as` wins; otherwise the manifest's `project_name`, which is already the
/// machine-friendly slug of the three name fields. Never `identifier` (a
/// reverse-DNS string is a poor thing to type) and never `product_name` (it has
/// spaces).
///
/// The `id` names a directory under the hub's root *and* is what a user types,
/// so it is held to a tighter charset than the app's `identifier` — which the
/// hub must keep accepting exactly as the station does (see `paths.rs`).
/// Refusing rather than sanitising is deliberate: a silent transformation
/// leaves the user with an app under a name they never chose and cannot guess.
pub fn resolve_id(explicit: Option<&str>, manifest: &Manifest) -> Result<String, InstallError> {
    let (id, derived) = match explicit {
        Some(id) => (id, false),
        None => (manifest.project_name.as_str(), true),
    };

    match is_usable_id(id) {
        true => Ok(id.to_string()),
        false => Err(InstallError::UnusableId {
            id: id.to_string(),
            derived,
        }),
    }
}

/// Whether `id` can be both a CLI word and a directory name.
fn is_usable_id(id: &str) -> bool {
    let mut characters = id.chars();
    let starts_well = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    starts_well
        && id.len() <= 64
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// Refuse `id` if anything already answers to it.
///
/// Both halves are checked, not just the registry: a directory left behind by
/// an install that was interrupted before it could register is exactly the case
/// where silently copying over it would destroy something. The two get
/// different messages because they need different answers from the user.
pub fn check_id_free(registry: &Registry, paths: &Paths, id: &str) -> Result<(), InstallError> {
    if let Some(entry) = registry.get(id) {
        return Err(InstallError::IdTaken {
            id: id.to_string(),
            location: entry.source.location.clone(),
        });
    }

    let app_dir = paths.app_dir(id)?;
    if app_dir.exists() {
        return Err(InstallError::DirectoryInTheWay { path: app_dir });
    }

    Ok(())
}

/// Refuse an install whose manifest's `identifier` already belongs to another
/// registered `id`.
///
/// `identifier`, not `id`, is what the data directory, the keyring namespace,
/// the WebKit data directory and the `.desktop` entry are all keyed on. Two
/// registry entries sharing one `identifier` would share all four, and the
/// second install would silently overwrite the first's `.desktop` entry
/// (`paths::desktop_entry_path`). Checked on the same already-loaded registry
/// [`check_id_free`] reads, and called right after it: a collision here
/// explains the data directory better than any version mismatch the
/// lifecycle gate below would find, so it is reported first.
pub fn check_identifier_free(registry: &Registry, identifier: &str) -> Result<(), InstallError> {
    if let Some(entry) = registry
        .apps
        .iter()
        .find(|entry| entry.identifier == identifier)
    {
        return Err(InstallError::IdentifierTaken {
            identifier: identifier.to_string(),
            id: entry.id.clone(),
        });
    }

    Ok(())
}

/// Who [`check_data_dir_available`] found already using the data directory.
#[derive(Debug)]
pub enum DataDirHolder {
    /// A live app window holds the sidecar liveness lock (CONTRACT.md §6).
    Window,
    /// An active `run` command holds `run.lock` (rule 3). `alias` is
    /// whatever [`lifecycle::probe_run_lock`] could read from the record —
    /// `None` in the narrow window between `run`'s own lock acquisition and
    /// its first write.
    RunCommand { alias: Option<String> },
}

/// Refuse an install into a data directory something is already using —
/// `install` is the data directory's third writer, and observes the same two
/// locks `open`'s launch guard and `run`'s own rule 3 already read
/// ([`lifecycle::probe_run_lock`], reused rather than reimplemented, and
/// [`tfsapp_core::process::is_owner_live`], the same read-only probe
/// `remove --purge` makes at `remove.rs`). This is the gap plain `remove`
/// leaves open: it does not check whether the app is running, so a live
/// window or an active `run` command can survive a `remove` and be exactly
/// what a following `install` would otherwise run Composer and lifecycle
/// commands against.
///
/// A stale `sidecar.pid` whose owning process is gone must **not** refuse:
/// [`tfsapp_core::process::is_owner_live`] already answers that correctly (a
/// dead process holds no lock), so a crashed instance never locks an app out
/// of being reinstalled.
pub fn check_data_dir_available(id: &str, data_dir: &Path) -> Result<(), InstallError> {
    // The ordinary case, by far: a brand new `identifier` has no data
    // directory at all yet. Neither probe below may create one — both open
    // their lock file with `create(true)`, which would fail on a missing
    // *parent* directory rather than silently succeed, so this has to be
    // checked first rather than left to surface as an I/O error.
    if !data_dir.is_dir() {
        return Ok(());
    }

    let pid_file = data_dir.join("sidecar.pid");
    if tfsapp_core::process::is_owner_live(&pid_file).unwrap_or(false) {
        return Err(InstallError::DataDirInUse {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            holder: DataDirHolder::Window,
        });
    }

    match lifecycle::probe_run_lock(data_dir) {
        Ok(lifecycle::RunLockHeld::Free) => Ok(()),
        Ok(lifecycle::RunLockHeld::Held { alias }) => Err(InstallError::DataDirInUse {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            holder: DataDirHolder::RunCommand { alias },
        }),
        Err(source) => Err(InstallError::Io {
            path: data_dir.join("run.lock"),
            source,
        }),
    }
}

/// Refuse an install whose manifest pins a port another installed app has
/// already claimed.
///
/// This is `build-app.sh`'s build-time port validation becoming a runtime
/// concern, which is the whole difference the hub makes: over there one build
/// produced one app and the only question was whether the number was a plausible
/// port. Here N apps share one machine and one registry, so the question becomes
/// *whose* port it is — and it can only be asked at install, because that is the
/// first moment both apps exist in one place.
///
/// Only static-versus-static collides. A dynamic port (the default, and what
/// every app that declares no `app_port` gets) is picked free at launch, so it
/// can neither claim nor lose a number here. That leaves one real race the hub
/// does not chase: a dynamic app could pick, at launch, the number a *closed*
/// static app has reserved. It is a launch-time conflict with a launch-time
/// answer, not something an install can prevent.
pub fn check_port_free(registry: &Registry, app_port: Option<u16>) -> Result<(), InstallError> {
    let Some(port) = app_port else {
        return Ok(());
    };

    // `app_port` is typed `u16`, so the manifest parse already refused anything
    // above 65535 or negative; zero is the one value left that parses and
    // cannot be bound — it means "any free port" to the OS, which is the
    // opposite of what pinning one is for.
    if port == 0 {
        return Err(InstallError::UnusablePort { port });
    }

    match registry
        .apps
        .iter()
        .find(|entry| entry.app_port == Some(port))
    {
        Some(entry) => Err(InstallError::PortTaken {
            port,
            id: entry.id.clone(),
        }),
        None => Ok(()),
    }
}

/// Decide which lifecycle event (CONTRACT.md §6) this install may run under,
/// from the data directory's own record — the four-row table from the
/// Overview, and nothing else. No I/O: `recorded` is `read_data_version`'s own
/// result, already read by the caller; `data_dir` is named only in the
/// refusals' messages, as the directory a user would delete or wait out.
///
/// `app_version` is taken as already-validated semver — `validate` runs
/// before this is ever called and refuses anything else (CONTRACT.md §2) —
/// so the parse below cannot fail in practice.
///
/// An equal record answers `Ok(LifecycleEvent::None)`: the reinstall-after-
/// `remove` path, and the event under which `prepare` (below) runs no
/// lifecycle command at all.
fn lifecycle_event_for_install(
    recorded: Option<&str>,
    app_version: &str,
    data_dir: &Path,
) -> Result<LifecycleEvent, InstallError> {
    let current = semver::Version::parse(app_version)
        .expect("validate() already refused a non-canonical app_version");

    match lifecycle::lifecycle_decision(recorded, &current) {
        // The hub's `install` does not own the update event (CONTRACT.md §6):
        // it would need a pre-update database snapshot this repo has not
        // ported (see the module header of `lifecycle.rs`), so it refuses
        // rather than adopting `pre-update`/`post-update`.
        Ok(LifecycleEvent::Update) => Err(InstallError::DataOlderThanSource {
            recorded: recorded
                .expect("an Update decision is only reached when a record exists")
                .to_string(),
            current: current.to_string(),
            data_dir: data_dir.to_path_buf(),
        }),
        Ok(event) => Ok(event),
        Err(LifecycleDecisionError::Downgrade { recorded, current }) => {
            Err(InstallError::DataNewerThanSource {
                recorded: recorded.to_string(),
                current: current.to_string(),
                data_dir: data_dir.to_path_buf(),
            })
        }
        // The data dir's `version` field parses as JSON but not as semver —
        // the same "cannot be trusted" class `read_data_version`'s own
        // `MalformedDataConfig` already covers for a file that does not even
        // parse as JSON, reused here rather than inventing a second flavour
        // of "the record is corrupt".
        Err(LifecycleDecisionError::InvalidVersion(error)) => Err(InstallError::Lifecycle(
            LifecycleError::MalformedDataConfig {
                path: lifecycle::data_config_path(&data_dir.join("data")),
                detail: error.to_string(),
            },
        )),
    }
}

/// Read `root`'s manifest and check the tree is an app the hub can install.
///
/// The `app_version` semver check is the hub's half of the station's
/// build-time validation (CONTRACT.md §2 now states the rule on its own,
/// naming no enforcer). Install is the hub's equivalent
/// moment: the value is what `update` will later compare against to choose
/// install / update / downgrade, and a value nothing can compare is a broken
/// app whose first symptom would appear months later, at the update that needed
/// it.
pub fn validate(root: &Path) -> Result<Loaded, InstallError> {
    let loaded = manifest::load(root)?;

    if let Err(error) = semver::Version::parse(&loaded.manifest.app_version) {
        return Err(InstallError::UnusableVersion {
            path: root.join(MANIFEST_FILE),
            version: loaded.manifest.app_version.clone(),
            detail: error.to_string(),
        });
    }

    for (relative, why) in REQUIRED_FILES {
        if !root.join(relative).is_file() {
            return Err(InstallError::MissingFile {
                path: root.join(relative),
                why,
            });
        }
    }

    Ok(loaded)
}

/// Copy `from` into `to`, minus what must not be installed.
///
/// `to` must not exist: an install never merges into a tree it did not write,
/// because a leftover file from a previous version — a migration, a compiled
/// container, a route — is indistinguishable from one this version meant to
/// ship. On any failure the partial copy is removed, so the caller's next
/// attempt meets a clean root rather than half of the last one.
pub fn snapshot(from: &Path, to: &Path) -> Result<(), InstallError> {
    if to.exists() {
        return Err(InstallError::DirectoryInTheWay {
            path: to.to_path_buf(),
        });
    }

    let copied = copy_tree(from, to, Path::new(""));
    if copied.is_err() {
        // Best-effort, and deliberately not reported: the caller is already
        // being told why the install failed, and "…and the cleanup failed too"
        // would bury it.
        let _ = fs::remove_dir_all(to);
    }
    copied
}

/// Whether `relative` — a path below the project root — is left out of the
/// snapshot.
fn is_excluded(relative: &Path) -> bool {
    EXCLUDED_PATHS
        .iter()
        .any(|excluded| relative == Path::new(excluded))
        || relative
            .file_name()
            .is_some_and(|name| EXCLUDED_NAMES.iter().any(|excluded| name == *excluded))
}

fn copy_tree(from: &Path, to: &Path, relative: &Path) -> Result<(), InstallError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| InstallError::Io { path, source }
    };

    fs::create_dir_all(to).map_err(io_error(to))?;
    // The source's own mode, so a `0700` directory does not become `0755`
    // because the hub's umask happened to be looser.
    let mode = fs::metadata(from).map_err(io_error(from))?.permissions();
    fs::set_permissions(to, mode).map_err(io_error(to))?;

    for entry in fs::read_dir(from).map_err(io_error(from))? {
        let entry = entry.map_err(io_error(from))?;
        let name = entry.file_name();
        let relative = relative.join(&name);
        if is_excluded(&relative) {
            continue;
        }

        let source_path = from.join(&name);
        let target_path = to.join(&name);
        let metadata = fs::symlink_metadata(&source_path).map_err(io_error(&source_path))?;

        if metadata.is_symlink() {
            // Recreated, never followed: a link is part of the tree, and
            // resolving it here would either duplicate what it points at or
            // pull in something outside the project entirely.
            let target = fs::read_link(&source_path).map_err(io_error(&source_path))?;
            std::os::unix::fs::symlink(target, &target_path).map_err(io_error(&target_path))?;
        } else if metadata.is_dir() {
            copy_tree(&source_path, &target_path, &relative)?;
        } else {
            // `fs::copy` carries the permission bits over, which is what keeps
            // `bin/console` executable on the other side.
            fs::copy(&source_path, &target_path).map_err(io_error(&target_path))?;
        }
    }

    Ok(())
}

/// Everything that can stop an install, in one type so the command has one
/// place to print from.
#[derive(Debug)]
pub enum InstallError {
    Source(SourceError),
    Manifest(ManifestError),
    Paths(PathsError),
    Registry(RegistryError),
    Env(EnvError),
    Php(PhpError),
    Platform(PlatformError),
    /// The install ran and the data dir's version record could not be written —
    /// which would leave the app installed but undated, and so re-running its
    /// whole install event on the next one.
    Lifecycle(LifecycleError),
    /// The derived or given `id` cannot name a directory or be typed as one
    /// word.
    UnusableId {
        id: String,
        derived: bool,
    },
    /// Another installed app already answers to this `id`.
    IdTaken {
        id: String,
        location: String,
    },
    /// Another registered `id` already carries this `identifier`.
    IdentifierTaken {
        identifier: String,
        id: String,
    },
    /// Something is already using the data directory this install would
    /// write into.
    DataDirInUse {
        id: String,
        data_dir: PathBuf,
        holder: DataDirHolder,
    },
    /// The data directory records a version *newer* than the source being
    /// installed — a downgrade, refused rather than guessed at (CONTRACT.md
    /// §6): running old code against data a newer version wrote is how a
    /// database gets corrupted quietly.
    DataNewerThanSource {
        recorded: String,
        current: String,
        data_dir: PathBuf,
    },
    /// The data directory records a version *older* than the source being
    /// installed — the update event (CONTRACT.md §6), which `install` does
    /// not own.
    DataOlderThanSource {
        recorded: String,
        current: String,
        data_dir: PathBuf,
    },
    /// `apps/<id>/` exists with no registry entry to explain it.
    DirectoryInTheWay {
        path: PathBuf,
    },
    /// Another installed app pinned this port first.
    PortTaken {
        port: u16,
        id: String,
    },
    /// A pinned port that no process could ever bind.
    UnusablePort {
        port: u16,
    },
    MissingFile {
        path: PathBuf,
        why: &'static str,
    },
    UnusableVersion {
        path: PathBuf,
        version: String,
        detail: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(formatter, "{error}"),
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Env(error) => write!(formatter, "{error}"),
            Self::Php(error) => write!(formatter, "{error}"),
            Self::Platform(error) => write!(formatter, "{error}"),
            Self::Lifecycle(error) => write!(formatter, "{error}"),
            Self::UnusableId { id, derived } => {
                let source = match derived {
                    true => format!(
                        "\"project_name\" is {id:?} in the app's {MANIFEST_FILE}, and the hub \
                         installs under that name by default"
                    ),
                    false => format!("--as {id:?}"),
                };
                write!(
                    formatter,
                    "{source} — an app id has to start with a letter or a digit and hold \
                     nothing but letters, digits, \"-\", \"_\" and \".\": it names a \
                     directory and is typed as one word. Pass --as <id> to pick another."
                )
            }
            Self::IdTaken { id, location } => write!(
                formatter,
                "{id} is already installed, from {location}. Pick another handle with \
                 --as <id>, or remove that one first."
            ),
            Self::IdentifierTaken { identifier, id } => write!(
                formatter,
                "{identifier} is already installed as {id} — the two would share its data \
                 directory and its .desktop entry. --as gives this install a different \
                 hub-local handle, not a different app: remove {id} first, or check this is \
                 genuinely a different app before installing it."
            ),
            Self::DataDirInUse {
                id,
                data_dir,
                holder,
            } => match holder {
                DataDirHolder::Window => write!(
                    formatter,
                    "{} is in use — {id} has a window open right now. Installing over it would \
                     run Composer and the app's own commands against the database that window \
                     has open; close {id} first.",
                    data_dir.display()
                ),
                DataDirHolder::RunCommand { alias: Some(alias) } => write!(
                    formatter,
                    "{} is in use — {id}'s \"{alias}\" run command is still active. Stop it \
                     first with `tfsapp-hub run --stop {id}`.",
                    data_dir.display()
                ),
                DataDirHolder::RunCommand { alias: None } => write!(
                    formatter,
                    "{} is in use — a run command is still active for {id}. Stop it first with \
                     `tfsapp-hub run --stop {id}`.",
                    data_dir.display()
                ),
            },
            Self::DataNewerThanSource {
                recorded,
                current,
                data_dir,
            } => write!(
                formatter,
                "{} was last written by app version {recorded}, but {current} is being \
                 installed — running old code against data a newer version wrote is how a \
                 database gets corrupted quietly (CONTRACT.md §6). Delete that directory by \
                 hand if you mean to start over: `remove --purge` cannot reach it, since no \
                 app is registered under this identifier to purge.",
                data_dir.display()
            ),
            Self::DataOlderThanSource {
                recorded,
                current,
                data_dir,
            } => write!(
                formatter,
                "{} was last written by app version {recorded}, and {current} is newer — that \
                 moment is the update event (CONTRACT.md §6), which `install` does not run. \
                 Per-app update is a separate, not-yet-landed command; installing here would \
                 run the wrong hooks — or none — over data already in place.",
                data_dir.display()
            ),
            Self::DirectoryInTheWay { path } => write!(
                formatter,
                "{} already exists but nothing is registered under it — an earlier install \
                 was probably interrupted. Remove that directory by hand and try again; \
                 the hub will not copy over a tree it cannot account for.",
                path.display()
            ),
            Self::PortTaken { port, id } => write!(
                formatter,
                "this app pins port {port} in its {MANIFEST_FILE}, and {id} already \
                 claimed it. Two apps cannot pin one port: drop \"app_port\" from one \
                 of them to give it a fresh port on every launch, or change the number."
            ),
            Self::UnusablePort { port } => write!(
                formatter,
                "\"app_port\" is {port} in the app's {MANIFEST_FILE} — a pinned port has \
                 to be one a process can bind, between 1 and 65535 (CONTRACT.md §2)."
            ),
            Self::MissingFile { path, why } => {
                write!(formatter, "{} is missing — {why}.", path.display())
            }
            Self::UnusableVersion {
                path,
                version,
                detail,
            } => write!(
                formatter,
                "\"app_version\" is {version:?} in {}, which is not canonical semver \
                 ({detail}). It is what decides install / update / downgrade later on, so \
                 it has to be comparable (CONTRACT.md §2).",
                path.display()
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Paths(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Env(error) => Some(error),
            Self::Php(error) => Some(error),
            Self::Platform(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<SourceError> for InstallError {
    fn from(error: SourceError) -> Self {
        Self::Source(error)
    }
}

impl From<ManifestError> for InstallError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<PathsError> for InstallError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<RegistryError> for InstallError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<EnvError> for InstallError {
    fn from(error: EnvError) -> Self {
        Self::Env(error)
    }
}

impl From<PhpError> for InstallError {
    fn from(error: PhpError) -> Self {
        Self::Php(error)
    }
}

impl From<PlatformError> for InstallError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl From<LifecycleError> for InstallError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
