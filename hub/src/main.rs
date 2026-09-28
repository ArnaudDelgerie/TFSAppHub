// The hub binary: parse argv, route to one command, exit with its code. The
// grammar itself — `--flags` act on the hub, bare words act on an app — and
// every form it accepts live in `cli.rs`; this file is the routing table and
// the handlers thin enough to have no module of their own. A command's real
// work belongs in that command's module, which is what let the station's
// `cli.rs` stay at ~215 lines across sixty plans.

mod app_env;
mod archive;
mod bridge;
mod cli;
mod close_guard;
mod crash;
mod desktop;
mod dev;
mod disk_space;
mod gh;
mod git;
mod hub_bin;
mod hub_rollback;
mod hub_update;
mod identity;
mod import_transaction;
mod install;
mod launch;
mod lifecycle;
mod lifecycle_gate;
mod list;
mod manifest;
mod media;
mod open;
mod open_files;
mod paths;
mod php;
mod picker;
mod platform;
mod portability;
mod prompt;
mod publish;
mod reconcile;
mod registry;
mod release;
mod remove;
mod revalidate;
mod rollback;
mod run;
mod secrets;
mod sidecar;
mod source;
mod update;
mod update_cache;
mod update_check;
mod update_refresh;
#[allow(dead_code)] // Step 2 wires the persisted protocol into update.
mod update_transaction;
mod user_dirs;
mod version;
mod window;
mod worker;

#[cfg(test)]
mod test_release;
#[cfg(test)]
mod update_transaction_tests;

use cli::{
    Command, Level, OpenChildSource, RunInvocation, EXIT_OK, EXIT_UNIMPLEMENTED, EXIT_USAGE,
};
use identity::Identity;

fn main() {
    // Every argument has to be UTF-8, and the hub says so itself rather than
    // letting `std::env::args` panic on it: a path that is not valid UTF-8
    // cannot travel through the file-request queue (or the single-instance
    // hand-off, which serialises argv as strings), and silently changing it
    // to a lossy spelling would deliver a path nobody asked for.
    let args: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(|argument| argument.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(bad) => {
            eprintln!(
                "tfsapp-hub: an argument is not valid UTF-8 ({}); paths must be valid UTF-8 \
                 to be passed on.",
                bad.to_string_lossy()
            );
            std::process::exit(cli::EXIT_USAGE);
        }
    };
    // The context is read here rather than from CARGO_PKG_* so that the
    // version the hub reports is the one baked into tauri.conf.json's package
    // info — the same source `--update` will later compare against a release
    // tag. It also keeps the Tauri codegen path exercised by an ordinary
    // `cargo build`.
    // Annotated because the branches below hand the context to no
    // `tauri::Builder`: without a Builder to pin the runtime, `Context<R>`'s
    // default `R = Wry` is not enough for inference.
    let context: tauri::Context = tauri::generate_context!();

    std::process::exit(dispatch(&args, context));
}

/// Route one parsed command, and answer with the process's exit code.
///
/// Split out of `main` so every branch has one obvious value, and so the
/// "recognised but not implemented yet" outcome is a return rather than an
/// `exit` buried in a handler.
fn dispatch(args: &[String], context: tauri::Context) -> i32 {
    let command = match cli::parse(args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            eprintln!("{}", error.hint());
            return EXIT_USAGE;
        }
    };

    // The surface table is the authority on what the hub can do today, so a
    // form it marks unavailable is answered here rather than reaching a route
    // that does not exist. Saying "not implemented yet" is the whole point: an
    // unknown-command error would send a user hunting for a typo that is not
    // there, and its own exit code says that nothing they type will fix it.
    if !command.is_implemented() {
        eprintln!(
            "tfsapp-hub: {} is recognised but not implemented yet.",
            command.name()
        );
        return EXIT_UNIMPLEMENTED;
    }

    // An early, advisory diagnostic only: it answers before any lease is
    // taken, so an interrupted command can still write its record after it
    // and before the command below runs. The authoritative check is the one
    // every lease holder runs under its lease in `lifecycle_gate` — this one
    // exists for the commands whose first act would otherwise misreport:
    // `open`'s parent and `run` resolve the app tree before any lease, and
    // an update killed with its tree set aside would show up there as a
    // missing installation instead of "run repair". A lease holder refused
    // under the gate gets the same message this prints, with no
    // `tfsapp-hub:` prefix of its own.
    if let Some(id) = interrupted_app_id(&command) {
        if let Ok(paths) = paths::Paths::resolve() {
            if let Ok(Some(refusal)) =
                lifecycle_gate::interruption(&paths, id, interrupted_operation(&command))
            {
                eprintln!("tfsapp-hub: {refusal}");
                return cli::EXIT_FAILED;
            }
        }
    }

    // Reconciliation (plan 020, step 5): a hub self-update can move PHP out
    // from under an already-installed app, and this is where that gets
    // noticed — once, on the first `Level::App` command that runs after it,
    // against the very registry that command is about to read anyway. Read
    // from `SURFACE` rather than listed here by name, same as the
    // unimplemented check above. A `Paths` failure here is swallowed rather
    // than reported: the command below either does not need the registry at
    // all, or is about to resolve `Paths` itself and will report the same
    // failure in its own words.
    if cli::spec(command.name()).is_some_and(|spec| spec.level == Level::App) {
        if let Ok(paths) = paths::Paths::resolve() {
            if let Err(error) =
                reconcile::reconcile(&paths, &context.package_info().version.to_string())
            {
                eprintln!("tfsapp-hub: warning: could not reconcile the registry: {error}");
            }
        }
    }

    match command {
        Command::Help => {
            print!("{}", cli::help_text());
            EXIT_OK
        }
        // Name and version both come from the package info Tauri assembles at
        // build time, which — with no `version` in `tauri.conf.json` to shadow
        // it — is the hub's own `Cargo.toml`. One version, one source, and the
        // one `--update` will compare against a release tag.
        Command::Version => {
            let package_info = context.package_info();
            println!("{} {}", package_info.name, package_info.version);
            EXIT_OK
        }
        // Same package info `--version` prints, passed by reference: `check`
        // (`hub_update.rs`) compares it against the release tag directly,
        // with no string round trip.
        Command::HubUpdate { assume_yes } => {
            hub_update::run(&context.package_info().version, assume_yes)
        }
        Command::HubRollback { assume_yes } => hub_rollback::run(assume_yes),
        Command::Install {
            source,
            id,
            reference,
            assume_yes,
            no_desktop_entry,
        } => install::run(
            &source,
            id.as_deref(),
            reference.as_deref(),
            assume_yes,
            no_desktop_entry,
            // The same package info `--version` prints, threaded in rather than
            // read from `CARGO_PKG_VERSION` here: the registry records which
            // hub wrote it, and two ways of asking that question are one too
            // many.
            &context.package_info().version.to_string(),
        ),
        Command::List => list::run(),
        // Resolves and re-executes; the window itself belongs to the child this
        // returns from, which is why the parent has an exit code to give at all.
        Command::Open { id, files } => open::run(&id, &files),
        // Foreground, unlike `open` — see `dev::run`.
        Command::Dev { path } => dev::run(&path),
        Command::Publish {
            path,
            repo,
            local,
            assume_yes,
        } => publish::run(&path, repo.as_deref(), local.as_deref(), assume_yes),
        Command::Update {
            id,
            archive,
            reference,
            assume_yes,
        } => update::run(
            &id,
            archive.as_deref().map(std::path::Path::new),
            reference.as_deref(),
            assume_yes,
            &context.package_info().version.to_string(),
        ),
        Command::Repair { id, assume_yes } => update::repair(&id, assume_yes),
        Command::Rollback { id, assume_yes } => rollback::run(&id, assume_yes),
        Command::Export { id, path } => portability::export(&id, &path),
        Command::Import {
            id,
            path,
            force,
            assume_yes,
        } => portability::import(&id, &path, force, assume_yes),
        Command::Remove {
            id,
            purge,
            assume_yes,
        } => remove::run(&id, purge, assume_yes),
        Command::Purge {
            identifier,
            assume_yes,
        } => remove::purge(identifier.as_deref(), assume_yes),
        // The three forms share one `run.rs`, routed here by which
        // `RunInvocation` `cli::parse_run` built — see that module's own
        // header for why the app-resolution step in front of all three is
        // the only genuinely new thing plan 013 adds.
        Command::Run(RunInvocation::List { id }) => run::list(&id),
        Command::Run(RunInvocation::ListActive { usage }) => run::list_active(usage),
        Command::Run(RunInvocation::Stop { id, alias }) => run::stop(&id, alias.as_deref()),
        Command::Run(RunInvocation::Start {
            id,
            alias,
            args,
            replace,
        }) => run::start(&id, &alias, &args, replace),
        // The app's own process, re-executed by `open <id>` and by `dev <path>`.
        // Everything it needs to become that app before GTK exists travels in
        // argv — see `open::child_args`.
        Command::OpenChild {
            source,
            identifier,
            product_name,
            icon_path,
            files,
        } => {
            open_window(
                source,
                Identity {
                    identifier,
                    product_name,
                    icon_path: icon_path.map(std::path::PathBuf::from),
                },
                context,
                files,
            );
            EXIT_OK
        }
    }
}

fn interrupted_app_id(command: &Command) -> Option<&str> {
    match command {
        Command::Open { id, .. }
        | Command::Update { id, .. }
        | Command::Rollback { id, .. }
        | Command::Remove { id, .. }
        | Command::Export { id, .. }
        | Command::Import { id, .. } => Some(id),
        Command::Run(RunInvocation::List { id })
        | Command::Run(RunInvocation::Stop { id, .. })
        | Command::Run(RunInvocation::Start { id, .. }) => Some(id),
        Command::OpenChild {
            source: OpenChildSource::Id(id),
            ..
        } => Some(id),
        _ => None,
    }
}

/// The maintenance operation the dispatched command will ask the gate for,
/// or `None` for the activity commands (`open`, `run`) — the same name
/// `refuse_if_interrupted` matches its exemptions against, so a `repair`
/// that may pass a journal and a `rollback` that may pass a marker are
/// recognised here exactly as they are under the lease.
fn interrupted_operation(command: &Command) -> Option<&'static str> {
    match command {
        Command::Update { .. } => Some("update"),
        Command::Rollback { .. } => Some("rollback"),
        Command::Remove { .. } => Some("remove"),
        Command::Export { .. } => Some("export"),
        Command::Import { .. } => Some("import"),
        // Activity holders run no named operation; the gate's activity
        // check passes `None` for exactly these.
        _ => None,
    }
}

/// Re-resolve the app in its own process and run every launch guard against it.
///
/// The parent already resolved all of this before spawning us, and this is not
/// that work repeated for its own sake: the parent proved the app *can* be
/// opened, cheaply and at the terminal, so a mistyped id or path never costs a
/// window. What happens here is the part that has to happen in the process
/// that will actually hold the app — the liveness lock is held by *this* pid,
/// and the port is bound to prove it is free to *this* process.
///
/// A failure exits through `lifecycle::fatal_startup_error`, which is the only
/// thing that reaches a user who launched from a desktop entry.
///
/// Rebuilds the matching `LaunchSpec` from `source` — `open::resolve` for
/// `--id`, `dev::resolve` for `--project` — which is `OpenChildSource`'s whole
/// reason to exist: one child, two constructors, resolved here rather than at
/// the parent so a mistyped id and a mistyped path fail the same way.
fn prepare(source: &OpenChildSource, identity: &Identity) -> Launching {
    let paths = match paths::Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => lifecycle::fatal_startup_error(&error.to_string()),
    };
    let resolved = match source {
        OpenChildSource::Id(id) => open::resolve(&paths, id).map_err(|error| error.to_string()),
        OpenChildSource::Project(path) => dev::resolve(path).map_err(|error| error.to_string()),
    };
    let spec = match resolved {
        Ok(spec) => spec,
        Err(message) => lifecycle::fatal_startup_error(&message),
    };

    // `spec.state_root`, created the way its own source needs: `0700` on every
    // launch for an installed app, because the directory holds the app's
    // database and its `APP_SECRET` and an installation created by an older
    // host or recreated by hand gets tightened on its next use; a plain
    // `var/` for a dev session, which needs no such tightening — it is the
    // developer's own project directory (CONTRACT.md's dev section).
    let activity = match &spec.source {
        launch::Source::Installed { .. } => {
            match lifecycle_gate::acquire_activity(&paths, &identity.identifier) {
                Ok(lease) => Some(lease),
                Err(error) => lifecycle::fatal_startup_error(&error.to_string()),
            }
        }
        launch::Source::Live => None,
    };
    let data_dir = match &spec.source {
        launch::Source::Installed { .. } => match paths.create_app_data_dir(&identity.identifier) {
            Ok(data_dir) => data_dir,
            Err(error) => lifecycle::fatal_startup_error(&error.to_string()),
        },
        launch::Source::Live => match std::fs::create_dir_all(&spec.state_root) {
            Ok(()) => spec.state_root.clone(),
            Err(error) => {
                lifecycle::fatal_startup_error(&format!("{}: {error}", spec.state_root.display()))
            }
        },
    };
    let data_subdir = data_dir.join("data");
    if let Err(error) = std::fs::create_dir_all(&data_subdir) {
        lifecycle::fatal_startup_error(&format!("{}: {error}", data_subdir.display()));
    }

    // `spec.label` rather than a hub-local id: a dev session has none to give,
    // and the label is the same thing a message about this launch would call
    // it either way (plan 009 step 1). The two sources part ways here too: an
    // installed launch runs every guard, a dev one skips the version guard and
    // writes no `data/config.json` — see `lifecycle::prepare_dev_launch`.
    let locks = match &spec.source {
        launch::Source::Installed { .. } => lifecycle::prepare_launch(
            &spec.label,
            &data_dir,
            &data_subdir,
            &identity.identifier,
            &spec.manifest.app_version,
            spec.manifest.app_port,
            &spec.manifest.run,
        ),
        launch::Source::Live => lifecycle::prepare_dev_launch(
            &data_dir,
            &data_subdir,
            &identity.identifier,
            spec.manifest.app_port,
        ),
    };

    // The launch's one microphone value (plan 070): the manifest's
    // declaration ANDed with this installation's `data/config.json`
    // revocation, read in both launch sources — a dev launch's
    // `data/config.json` normally does not exist, which reads as "not
    // revoked". Everything downstream (the splash's grant, every later
    // window's grant, `serve`'s wait for the grant report) reads this
    // instead of re-deriving the declaration, so a revocation and a
    // missing declaration reach the platform and `hub.log` with the same
    // single value they will be answered with.
    let microphone_access = media::MicrophoneAccess::of(
        spec.manifest.actions.media.microphone,
        lifecycle::read_microphone_revoked(&data_subdir),
    );
    if microphone_access == media::MicrophoneAccess::Revoked {
        eprintln!(
            "tfsapp-hub: the microphone is declared but revoked in {}; \
             TFS_MEDIA_MICROPHONE=0",
            lifecycle::data_config_path(&data_subdir).display()
        );
    }

    Launching {
        paths,
        spec,
        locks,
        activity,
        microphone_access,
    }
}

/// What the guards leave for the launch itself: where the app is, what it
/// declares, and the locks proving this process is its live instance
/// ([`lifecycle::LaunchLocks`]). `microphone_access` is the one value the
/// whole launch's microphone story reads (plan 070): computed once here, it
/// drives the splash's grant, every later window's grant, and `serve`'s wait.
struct Launching {
    paths: paths::Paths,
    spec: launch::LaunchSpec,
    locks: Option<lifecycle::LaunchLocks>,
    activity: Option<lifecycle_gate::ActivityLease>,
    microphone_access: media::MicrophoneAccess,
}

/// Open the app installed as `id`, under `identity`.
///
/// The shape is the station's, and its order is the part worth reading. The
/// splash window is built **first**, on the main thread, before the sidecar
/// exists — and `.setup()` returns immediately after spawning the thread that
/// does everything else. That early return is what lets the GTK event loop
/// advance far enough to actually paint the splash. Everything slow then happens
/// off the main thread and ends by navigating that same window to the backend,
/// so nothing visibly jumps and no second window appears.
fn open_window(
    source: OpenChildSource,
    identity: Identity,
    mut context: tauri::Context,
    files: Vec<String>,
) {
    // Before the `Builder` exists, and so before anything has initialised GTK
    // — the whole point of the module. Everything identity-derived downstream
    // (app id, bus name, single-instance key, WM_CLASS, cookie store) reads
    // what this call leaves behind. First of all, so that even a guard's own
    // refusal dialog below carries this app's identity rather than the hub's.
    identity::apply(&identity, &mut context);

    // Everything from here to `Builder` is CONTRACT.md §6's launch guards, and
    // all of it has to happen in this window: they refuse through a blocking
    // native dialog, which deadlocks rather than appears once Tauri has claimed
    // GTK. See `lifecycle`'s module header.
    let Launching {
        paths,
        spec,
        locks,
        activity,
        microphone_access,
    } = prepare(&source, &identity);

    // The app's `actions` groups, granted at runtime, before `Builder` — the
    // only window in which they can be: `add_capability` lives on `Context` and
    // has no equivalent once `Builder::run` has taken over. The static
    // `capabilities/default.json` grants nothing at all, so an `invoke` for a
    // group this app did not declare is refused by Tauri's own ACL before any
    // handler runs. Registering the commands is not the boundary; this is.
    for grant in window::declared_action_ipc_grants(&spec.manifest.actions) {
        context
            .runtime_authority_mut()
            .add_capability(window::action_capability(grant))
            .expect("a valid action capability");
    }

    // One slot per process, shared by every window this process builds: the
    // splash's navigation policy needs to read the backend's origin, and the
    // splash is built long before that origin exists.
    let app_origin = window::new_app_origin_slot();
    let relaunch_origin = app_origin.clone();
    let relaunch_icon_path = identity.icon_path.clone();

    // One guard state per launch, created before the `Builder` so no window —
    // splash, relaunch or main — and no request thread can exist ahead of it.
    // It is managed into Tauri in `setup` (before the splash is built, the
    // earliest a webview could call a command) and handed to `serve` for the
    // bridge's request threads. Both transports share the one state, while
    // keeping their own namespaces inside it.
    let close_guards = std::sync::Arc::new(close_guard::CloseGuardState::new());
    let relaunch_close_guards = close_guards.clone();

    // One pending-request queue per launch, likewise created before the
    // `Builder` and managed before the splash: it has to exist before any
    // arrival — a file-bearing launch's own arguments (plan 056 step 3) or a
    // second-instance handoff — and before any window could read it. Managed
    // unconditionally, like the close guards: the state is inert until an
    // enqueue happens, and the enqueueing boundary is what refuses an app
    // that declared no receiver capability.
    let open_files = std::sync::Arc::new(open_files::OpenFilesState::new());
    let serve_open_files = open_files.clone();
    // The arrival's other fact, read here before `spec` moves into `setup`:
    // the second-instance callback runs in whatever process is live when the
    // arrival lands, and it is *that* instance's declaration that decides
    // whether a file-bearing argv can be delivered at all — and whether its
    // directories opted in, which the batch validation needs the same way.
    let relaunch_receiver = open_files::Receiver::of(&spec.manifest);
    // Read here for the same reason: a further window on an already-running
    // backend goes through `create_app_window`, never through `serve`, so its
    // own media grant has to be decided from this instance's access value
    // before `spec` moves into `setup`.
    let relaunch_microphone_access = microphone_access;

    // A file-bearing launch enqueues its own batch before the `Builder` exists,
    // so it is waiting in the pre-launch pool before any window — and the
    // window that becomes the app's first document claims it in `serve`'s
    // hand-off. Only the instance that holds the launch locks: a hand-off
    // child's queue dies with it at the single-instance `exit(0)`, and its
    // argv — the same paths, the same order — is delivered by the live
    // instance's own callback instead, which is the path that owns a queue
    // with a future.
    if !files.is_empty() && locks.is_some() {
        if relaunch_receiver.declared {
            match open_files::validate_batch(&files, relaunch_receiver.directories) {
                Ok(()) => {
                    if let Err(error) = open_files.enqueue(files, None) {
                        eprintln!(
                            "tfsapp-hub: warning: the file request was not queued: {error}; \
                             opening without the files."
                        );
                    }
                }
                Err(diagnostic) => {
                    eprintln!("tfsapp-hub: warning: {diagnostic}; opening without the files.");
                }
            }
        } else {
            eprintln!(
                "tfsapp-hub: warning: this app declares no open_files receiver; opening \
                 without the files."
            );
        }
    }

    window::register_splash_scheme(tauri::Builder::default(), &spec.app_dir)
        // Registered before every other plugin, per the plugin's own guidance,
        // and after the identity mutation above — which is what makes its key
        // this app's identifier rather than the hub's. Two different apps of
        // the one binary therefore never collide on it, and a second launch of
        // *this* app reaches the closure below instead of booting a second
        // process against the same data dir.
        .plugin(tauri_plugin_single_instance::init(
            move |app, args, _cwd| {
                use tauri::Manager;

                // A second `open` of an app already running is not an error
                // and not a no-op: it opens another window on the backend
                // already up — unless a final shutdown has committed, in
                // which case no window may be opened on this process again
                // (plan 055: an arrival after commitment cannot revive the
                // closing instance; before commitment it changes the
                // topology and every close decision honours it).
                //
                // A file-bearing second `open` (plan 056 step 3) is neither:
                // its argv carries files after `--`, and that arrival is a
                // delivery to an existing window — during the splash, to the
                // pre-launch pool the hand-off empties — never a
                // second window. The pinned plugin hands this callback the
                // second process's whole argv, program name included, so the
                // same parser that built it reads it back.
                //
                // This callback runs on the plugin's D-Bus service thread,
                // serialized with nothing — so it decides nothing here. The
                // whole admission (launch presence, shutdown re-check,
                // window creation or file delivery) is one unit scheduled on
                // the event loop, where every close decision and commitment
                // also runs: an arrival suspended across a committed shutdown
                // cannot open a window on the closing instance (audit 017,
                // finding 2). A scheduling failure means the loop is gone —
                // the process is exiting, which is the shutting-down
                // outcome already.
                let app = app.clone();
                let origin = relaunch_origin.clone();
                let guards = relaunch_close_guards.clone();
                let icon_path = relaunch_icon_path.clone();
                let receiver = relaunch_receiver;
                let microphone_access = relaunch_microphone_access;
                let arrival = cli::second_instance_files(&args);
                let scheduler = app.clone();
                let scheduled = move || {
                    let launch_present = app.try_state::<sidecar::Launch>().is_some();
                    if !arrival.is_empty() {
                        // The file branch needs no launch snapshot: where the
                        // arrival goes is decided with the queue itself
                        // (audit 019, finding 1), so a hand-off racing this
                        // closure can neither strand the request in a pool
                        // nothing will drain nor have it refused while the
                        // first document's navigation is still settling.
                        match open_files::deliver_arrival(&app, &guards, receiver, &arrival) {
                            open_files::ArrivalOutcome::Delivered { window: None } => {
                                println!(
                                    "tfsapp-hub: still starting — the paths will be delivered \
                                     when the window is ready."
                                );
                            }
                            open_files::ArrivalOutcome::Delivered { window: Some(_) } => {
                                println!(
                                    "tfsapp-hub: delivering the file request to the app's \
                                     window."
                                );
                            }
                            open_files::ArrivalOutcome::Refused(diagnostic) => {
                                eprintln!("tfsapp-hub: {diagnostic}");
                            }
                        }
                        return;
                    }
                    let outcome =
                        window::admit_second_instance_window(&guards, launch_present, || {
                            let launch = app.state::<sidecar::Launch>();
                            window::create_app_window(
                                &app,
                                &launch.url,
                                &launch.product_name,
                                &origin,
                                &guards,
                            )
                            .and_then(|window| {
                                if let Some(path) = &icon_path {
                                    if let Some(icon) = identity::load_icon(path) {
                                        window.set_icon(icon)?;
                                    }
                                }
                                // Best-effort, like the splash's own install:
                                // a further window on an already-running
                                // backend gets the same grant an undeclared
                                // app already denies explicitly, and this
                                // window's own outcome feeds nothing —
                                // `TFS_MEDIA_MICROPHONE` was already resolved
                                // for the sidecar this backend is serving.
                                let _ = media::install_permission_handler(
                                    &window,
                                    microphone_access,
                                    origin.clone(),
                                );
                                let _ = crash::install_crash_recovery_handler(
                                    &window,
                                    guards.clone(),
                                    launch.product_name.clone(),
                                    launch.splash_bg.clone(),
                                    launch.splash_text.clone(),
                                    launch.url.clone(),
                                );
                                Ok(())
                            })
                            .map_err(|error| error.to_string())
                        });
                    match outcome {
                        window::SecondInstanceOutcome::StillStarting => {
                            println!("tfsapp-hub: still starting — the window is on its way.");
                        }
                        window::SecondInstanceOutcome::ShuttingDown => {
                            println!(
                                "tfsapp-hub: this instance is shutting down; the new launch \
                                 was not given a window."
                            );
                        }
                        window::SecondInstanceOutcome::WindowOpened => {}
                        window::SecondInstanceOutcome::WindowFailed(error) => {
                            eprintln!("tfsapp-hub: cannot open another window: {error}");
                        }
                    }
                };
                let _ = scheduler.run_on_main_thread(scheduled);
            },
        ))
        .plugin(tauri_plugin_dialog::init())
        // Registered unconditionally in both cases — registration is not the
        // boundary, the ACL grant above is. With a group's `ipc` off, nothing
        // grants its permission and `invoke()` is refused before a handler runs.
        .invoke_handler(tauri::generate_handler![
            secrets::secret_has,
            secrets::secret_get,
            secrets::secret_set,
            secrets::secret_delete,
            secrets::secret_list,
            update_check::update_check,
            picker::pick_path,
            picker::save_path,
            close_guard::close_guard_context,
            close_guard::close_guard_register,
            close_guard::close_guard_remove,
            open_files::open_files_pending,
            open_files::open_files_ack,
        ])
        .setup(move |app| {
            // `manage` below is the one call here that needs the trait, and
            // importing it at the top of `open_window` would read as if the
            // whole function dealt in managed state.
            use tauri::Manager;

            // Greyscale rather than subpixel text antialiasing, for every
            // WebView this process creates. It has to run after Tauri has
            // initialised GTK and before any window exists, because WebKit reads
            // its font options when its web process starts. A `None` here means
            // GTK is not initialised, which is a situation to leave alone rather
            // than panic on.
            if let Some(settings) = gtk::Settings::default() {
                use gtk::prelude::GtkSettingsExt;
                settings.set_gtk_xft_rgba(Some("none"));
            }

            // Before either window exists, so it covers the whole life of the
            // sidecar the thread below is about to spawn.
            lifecycle::install_shutdown_on_signal(app.handle());

            // Before the splash is built: the earliest a webview could call a
            // close-guard command is the splash's own first script run, and
            // every command resolves this state from the calling window.
            app.manage(close_guards.clone());

            // The open-files queue the same way — before any window exists,
            // so a startup arrival already has somewhere to land.
            app.manage(open_files.clone());

            let splash_source =
                window::resolve_splash_source(&spec.app_dir, spec.manifest.splash_path.as_deref());
            let splash = window::create_splash_window(
                app,
                &identity.product_name,
                &window::splash_style(
                    &identity.product_name,
                    spec.manifest.splash_bg.as_deref(),
                    spec.manifest.splash_text.as_deref(),
                ),
                &app_origin,
                &splash_source,
                &close_guards,
            )?;
            if let Some(path) = &identity.icon_path {
                if let Some(icon) = identity::load_icon(path) {
                    splash.set_icon(icon)?;
                }
            }
            let splash_label = splash.label().to_string();

            // Scheduled on the splash's own webview — the same one `serve`
            // navigates to the backend, never rebuilt for the hand-over
            // (`architecture/09`) — so this one install covers the window for
            // the app's whole first life. The receiver is kept here and
            // **not** waited on: this closure runs on the main thread, which
            // is the thread the `with_webview` closure itself needs to run
            // on, so blocking here could only deadlock. `serve` waits, off
            // the main thread, and `TFS_MEDIA_MICROPHONE` reports what that
            // wait learned, never what the manifest asked for (§3).
            let media_grant_report =
                media::install_permission_handler(&splash, microphone_access, app_origin.clone())
                    .ok();
            let _ = crash::install_crash_recovery_handler(
                &splash,
                close_guards.clone(),
                identity.product_name.clone(),
                spec.manifest.splash_bg.clone(),
                spec.manifest.splash_text.clone(),
                // The URL this window was created showing, so a web process
                // killed before any page committed still gets a Reload
                // target (`crash::reload_target`).
                splash_source.initial_url(),
            );

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                serve(
                    handle,
                    paths,
                    spec,
                    identity,
                    locks,
                    activity,
                    app_origin,
                    splash_label,
                    close_guards,
                    serve_open_files,
                    microphone_access,
                    media_grant_report,
                );
            });

            Ok(())
        })
        .on_window_event(lifecycle::on_window_event)
        // `build` + `run` rather than `Builder::run(context)`, which is the
        // same thing with an empty callback: the hub needs the callback. See
        // `lifecycle::veto_exit` — teardown destroys this process's windows
        // before signalling the backend, and an event loop that finds itself
        // with no windows left would otherwise end the process on the spot,
        // orphaning the FrankenPHP that has not been signalled yet.
        .build(context)
        .expect("failed to build the app window")
        .run(lifecycle::on_run_event);
}

/// Everything between the splash appearing and the app answering, off the main
/// thread.
///
/// Every failure hands off to `lifecycle::fatal_post_setup_error`, which stops
/// whatever is already running, shows a dialog over the still-visible splash and
/// exits. Already off the main thread here by construction, which is what that
/// function requires.
#[allow(clippy::too_many_arguments)]
fn serve(
    app: tauri::AppHandle,
    paths: paths::Paths,
    spec: launch::LaunchSpec,
    identity: Identity,
    locks: Option<lifecycle::LaunchLocks>,
    activity: Option<lifecycle_gate::ActivityLease>,
    app_origin: window::AppOriginSlot,
    splash_label: String,
    close_guards: close_guard::SharedCloseGuards,
    open_files: open_files::SharedOpenFiles,
    microphone_access: media::MicrophoneAccess,
    media_grant_report: Option<std::sync::mpsc::Receiver<bool>>,
) {
    use tauri::Manager;

    // `prepare_launch` returns no locks for a hand-off to an existing app.
    // That child must not touch Composer: another launch already owns the
    // app, and this process has nothing left to serve.
    let holds_launch_locks = locks.is_some();
    if !holds_launch_locks {
        drop(activity);
        return;
    }

    // Retain the shared activity lease for the complete window lifetime, next
    // to the liveness and serving locks it protects. Tauri owns managed state
    // until this process exits, including paths where `serve` itself returns
    // after reporting an error.
    if let Some(activity) = activity {
        app.manage(activity);
    }

    let (liveness_lock, serving_lock) = match locks {
        Some(locks) => (Some(locks.liveness), Some(locks.serving)),
        None => (None, None),
    };

    let manifest = &spec.manifest;
    let splash_source =
        window::resolve_splash_source(&spec.app_dir, manifest.splash_path.as_deref());
    if manifest.splash_path.is_some() && splash_source == window::SplashSource::Fallback {
        // Said, not swallowed: an app author who declared a splash page has to
        // learn it is not the one on screen. See `window::splash_style`.
        eprintln!(
            "tfsapp-hub: warning: this app declares \"splash_path\", but the hub could not \
             show it — the file is missing, unreadable, or escapes the installed snapshot. \
             Showing the hub's own splash in its colours instead."
        );
    }

    let env_mode = match &spec.source {
        launch::Source::Installed { .. } => {
            let mut expected_cache = spec.expected_cache.clone().expect(
                "an installed launch's spec always carries its expected cache stamp — \
                 open::resolve is its only constructor",
            );
            if should_revalidate(holds_launch_locks, spec.pending_revalidation.is_some()) {
                let last_known_platform = spec
                    .pending_revalidation
                    .as_ref()
                    .expect("the pending revalidation was just checked");
                let id = spec
                    .installed_id()
                    .expect("only installed specs carry a pending revalidation");
                println!(
                    "Revalidating {id} behind the splash — it was installed against PHP \
                     {last_known_platform}. Re-resolving its dependencies…"
                );
                match revalidate::revalidate(&paths, id, &spec.app_dir, manifest) {
                    Ok(revalidate::Outcome::Ready(platform)) => {
                        println!("{id} is ready.");
                        // The revalidation just probed this platform. Comparing
                        // against the registry value that made the work pending
                        // would incorrectly preserve a cache built for old PHP.
                        expected_cache.platform = platform;
                    }
                    Ok(revalidate::Outcome::Broken) => {
                        return lifecycle::fatal_post_setup_error(
                            app,
                            open::OpenError::Broken {
                                id: id.to_string(),
                                platform: last_known_platform.to_string(),
                            }
                            .to_string(),
                        );
                    }
                    Err(error) => return lifecycle::fatal_post_setup_error(app, error.to_string()),
                }
            }
            app_env::Mode::Launch(expected_cache)
        }
        launch::Source::Live => app_env::Mode::Dev,
    };
    // `serve` runs off the main thread, so it may block — and this is the
    // one place that does: the grant's report is waited for only when this
    // launch's microphone is `Allowed` (plan 070 — an undeclared or revoked
    // app has nothing being granted, so it waits for nothing and its
    // report, if the closure ever sends one, is dropped unread), never
    // longer than the deadline, and never from the main thread (the `setup`
    // closure and the second-instance callback share the thread the
    // `with_webview` closure itself needs).
    let microphone_granted = if microphone_access == media::MicrophoneAccess::Allowed {
        media::await_grant(media_grant_report, media::MICROPHONE_GRANT_DEADLINE)
    } else {
        false
    };
    let environment = match app_env::resolve(
        manifest,
        &spec.app_dir,
        &identity.identifier,
        &spec.state_root,
        env_mode,
        microphone_granted,
    ) {
        Ok(environment) => environment,
        Err(error) => return lifecycle::fatal_post_setup_error(app, error.to_string()),
    };
    let toolchain = match php::toolchain(&paths) {
        Ok(toolchain) => toolchain,
        Err(error) => return lifecycle::fatal_post_setup_error(app, error.to_string()),
    };

    // Managed before the sidecar starts, so the IPC commands can never meet a
    // window without a store behind it: `secrets.rs` resolves both from the
    // calling window and has nothing else to fall back on. `spec.update` the
    // same way — `update_check::update_check` reads it from the calling
    // window, exactly as the secret commands read their own state.
    app.manage(environment.secret_store.clone());
    app.manage(manifest.actions.secrets.clone());
    app.manage(spec.update.clone());

    let (sidecar, url) = match sidecar::start(
        &toolchain,
        &spec.app_dir,
        &environment,
        manifest,
        &spec.update,
        &close_guards,
        liveness_lock,
        serving_lock,
        &app,
    ) {
        Ok(started) => started,
        Err(error) => return lifecycle::fatal_post_setup_error(app, error.to_string()),
    };
    app.manage(std::sync::Mutex::new(sidecar));

    let sidecar_log = environment.log_dir.join("sidecar.log");
    let healthy = {
        let state = app.state::<std::sync::Mutex<sidecar::Sidecar>>();
        let mut guard = state.lock().expect("the sidecar mutex is not poisoned");
        let server = guard
            .server
            .as_mut()
            .expect("the server is always Some right after start");
        tfsapp_core::health::wait_for_healthz(&url, server, Some(&sidecar_log))
    };
    if let Err(error) = healthy {
        return lifecycle::fatal_post_setup_error(app, error.to_string());
    }

    // Hand the *same* window over to the backend rather than opening a second
    // one. The origin is published first: this programmatic navigation goes
    // through the very policy that would otherwise cancel it, leaving the app
    // stuck on its splash for ever.
    let Some(window) = app.get_webview_window(&splash_label) else {
        // The user closed the splash before the backend was ready; the close
        // handler has already torn the sidecar down. Nothing left to navigate.
        return;
    };
    window::publish_app_origin(&app_origin, &url);
    // The open-files hand-off, before the navigation it designates this
    // window for: the pool of startup arrivals drains into it and — the part
    // audit 019's finding 1 turns into an invariant — the pool closes, so an
    // arrival that races this transition lands on the window through the
    // queue's own in-lock fallback instead of waiting for a drain that
    // already happened. Notified even though the receiver cannot have
    // subscribed yet: the notification may well be lost, and the receiver's
    // own startup read is what actually delivers.
    let claimed = open_files.handoff_to_first_document(&splash_label);
    if claimed > 0 {
        println!("tfsapp-hub: delivering {claimed} file request(s) that arrived during startup.");
        open_files::notify(&app, &splash_label);
    }
    // `window.navigate` pushes a history entry; the bundled splash on
    // `tauri://` stays navigable forever (`window.rs`'s
    // `is_bundled_asset_origin`), so Back would return to a cold-start page
    // that then waits for a hand-over that already happened. `location.replace`
    // replaces the splash's entry instead of stacking on it, and the
    // navigation still goes through `classify_navigation` like any other.
    if let Err(error) = window.eval(format!("location.replace({url:?})")) {
        return lifecycle::fatal_post_setup_error(app, error.to_string());
    }

    app.manage(sidecar::Launch {
        url,
        product_name: identity.product_name,
        splash_bg: manifest.splash_bg.clone(),
        splash_text: manifest.splash_text.clone(),
    });

    // Last of all, and only after the app is already running: a slow or
    // offline forge must never delay a launch reaching its window. A no-op
    // for anything but a release install that declares `actions.update` —
    // see `update_refresh::spawn`'s own guard.
    update_refresh::spawn(
        paths,
        &spec.update,
        manifest.actions.update.ipc || manifest.actions.update.bridge,
        Some(environment.log_dir.join("hub.log")),
    );
}

/// Revalidation is work for the instance that owns the launch locks. A
/// hand-off reaches this point with neither locks nor a right to change the
/// installed tree, even if the parent observed a pending revalidation.
fn should_revalidate(holds_launch_locks: bool, pending_revalidation: bool) -> bool {
    holds_launch_locks && pending_revalidation
}

#[cfg(test)]
mod tests {
    use super::should_revalidate;

    #[test]
    fn a_hand_off_short_circuits_before_pending_revalidation() {
        assert!(!should_revalidate(false, true));
    }

    #[test]
    fn the_lock_holding_child_runs_pending_revalidation_once() {
        assert!(should_revalidate(true, true));
        assert!(!should_revalidate(true, false));
    }
}
