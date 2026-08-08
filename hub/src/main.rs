// The hub binary: parse argv, route to one command, exit with its code. The
// grammar itself — `--flags` act on the hub, bare words act on an app — and
// every form it accepts live in `cli.rs`; this file is the routing table and
// the handlers thin enough to have no module of their own. A command's real
// work belongs in that command's module, which is what let the station's
// `cli.rs` stay at ~215 lines across sixty plans.

mod app_env;
mod cli;
mod identity;
mod install;
mod lifecycle;
mod list;
mod manifest;
mod open;
mod paths;
mod php;
mod platform;
mod prompt;
mod registry;
mod remove;
mod sidecar;
mod source;
mod window;
mod worker;

use cli::{Command, EXIT_FAILED, EXIT_OK, EXIT_UNIMPLEMENTED, EXIT_USAGE};
use identity::Identity;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        Command::Install {
            source,
            id,
            reference,
            assume_yes,
        } => install::run(
            &source,
            id.as_deref(),
            reference.as_deref(),
            assume_yes,
            // The same package info `--version` prints, threaded in rather than
            // read from `CARGO_PKG_VERSION` here: the registry records which
            // hub wrote it, and two ways of asking that question are one too
            // many.
            &context.package_info().version.to_string(),
        ),
        Command::List => list::run(),
        // Resolves and re-executes; the window itself belongs to the child this
        // returns from, which is why the parent has an exit code to give at all.
        Command::Open { id } => open::run(&id),
        Command::Remove {
            id,
            purge,
            assume_yes,
        } => remove::run(&id, purge, assume_yes),
        Command::Platform => print_platform(),
        // The app's own process, re-executed by `open <id>` above. Everything it
        // needs to become that app before GTK exists travels in argv — see
        // `open::child_args`.
        Command::OpenChild {
            id,
            identifier,
            product_name,
            icon_path,
        } => {
            open_window(
                &id,
                Identity {
                    identifier,
                    product_name,
                    icon_path: icon_path.map(std::path::PathBuf::from),
                },
                context,
            );
            EXIT_OK
        }
        // Unreachable while the table above and this match agree, which is
        // exactly what makes it worth keeping: a command marked implemented
        // with no route here is a bug in one of the two, and it should say so
        // rather than fall through to something plausible.
        other => {
            eprintln!(
                "tfsapp-hub: {} is marked implemented but has no route — this is a bug.",
                other.name()
            );
            EXIT_FAILED
        }
    }
}

/// Print what the bundled FrankenPHP says it is, and the fingerprint derived
/// from it. Returns the process exit code.
///
/// The extension list is printed in full alongside the hash: the point of this
/// command is to be checkable against `frankenphp php-cli -m` by eye, and a
/// 64-character hash on its own is checkable against nothing.
fn print_platform() -> i32 {
    let binary = match platform::hub_frankenphp() {
        Ok(binary) => binary,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };

    match platform::probe(&binary) {
        Ok(probe) => {
            let fingerprint = probe.fingerprint();
            println!("interpreter: {}", binary.display());
            println!("php:         {}", probe.php_version);
            println!(
                "extensions:  {} — {}",
                probe.extensions.len(),
                probe.extensions.join(", ")
            );
            println!("hash:        {}", fingerprint.extensions_hash);
            println!("platform:    {fingerprint}");
            EXIT_OK
        }
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// Re-resolve the app in its own process and run every launch guard against it.
///
/// The parent already resolved all of this before spawning us, and this is not
/// that work repeated for its own sake: the parent proved the app *can* be
/// opened, cheaply and at the terminal, so a mistyped id never costs a window.
/// What happens here is the part that has to happen in the process that will
/// actually hold the app — the liveness lock is held by *this* pid, and the port
/// is bound to prove it is free to *this* process.
///
/// A failure exits through `lifecycle::fatal_startup_error`, which is the only
/// thing that reaches a user who launched from a desktop entry.
fn prepare(id: &str, identity: &Identity) -> Launching {
    let paths = match paths::Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => lifecycle::fatal_startup_error(&error.to_string()),
    };
    let resolved = match open::resolve(&paths, id) {
        Ok(resolved) => resolved,
        Err(error) => lifecycle::fatal_startup_error(&error.to_string()),
    };

    // `0700` on every launch, not only on creation: the directory holds the
    // app's database and its `APP_SECRET`, so an installation created by an
    // older host or recreated by hand gets tightened on its next use.
    let data_dir = match paths.create_app_data_dir(&identity.identifier) {
        Ok(data_dir) => data_dir,
        Err(error) => lifecycle::fatal_startup_error(&error.to_string()),
    };
    let data_subdir = data_dir.join("data");
    if let Err(error) = std::fs::create_dir_all(&data_subdir) {
        lifecycle::fatal_startup_error(&format!("{}: {error}", data_subdir.display()));
    }

    let lock = lifecycle::prepare_launch(
        id,
        &data_dir,
        &data_subdir,
        &identity.identifier,
        &resolved.manifest.app_version,
        resolved.manifest.app_port,
    );

    Launching {
        paths,
        resolved,
        lock,
    }
}

/// What the guards leave for the launch itself: where the app is, what it
/// declares, and the liveness lock proving this process is its live instance.
struct Launching {
    paths: paths::Paths,
    resolved: open::Resolved,
    lock: Option<std::fs::File>,
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
fn open_window(id: &str, identity: Identity, mut context: tauri::Context) {
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
        resolved,
        lock,
    } = prepare(id, &identity);

    // One slot per process, shared by every window this process builds: the
    // splash's navigation policy needs to read the backend's origin, and the
    // splash is built long before that origin exists.
    let app_origin = window::new_app_origin_slot();
    let relaunch_origin = app_origin.clone();

    tauri::Builder::default()
        // Registered before every other plugin, per the plugin's own guidance,
        // and after the identity mutation above — which is what makes its key
        // this app's identifier rather than the hub's. Two different apps of
        // the one binary therefore never collide on it, and a second launch of
        // *this* app reaches the closure below instead of booting a second
        // process against the same data dir.
        .plugin(tauri_plugin_single_instance::init(
            move |app, _args, _cwd| {
                use tauri::Manager;

                // A second `open` of an app already running is not an error and
                // not a no-op: it opens another window on the backend already
                // up. Before the backend is resolved there is no `Launch` state
                // yet and this deliberately does nothing — the window on its way
                // is the one the user is waiting for.
                let Some(launch) = app.try_state::<sidecar::Launch>() else {
                    println!("tfsapp-hub: still starting — the window is on its way.");
                    return;
                };
                if let Err(error) = window::create_app_window(
                    app,
                    &launch.url,
                    &launch.product_name,
                    &relaunch_origin,
                ) {
                    eprintln!("tfsapp-hub: cannot open another window: {error}");
                }
            },
        ))
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
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

            let splash = window::create_splash_window(
                app,
                &identity.product_name,
                &window::splash_style(
                    &identity.product_name,
                    resolved.manifest.splash_bg.as_deref(),
                    resolved.manifest.splash_text.as_deref(),
                ),
                &app_origin,
            )?;
            if let Some(path) = &identity.icon_path {
                if let Some(icon) = identity::load_icon(path) {
                    splash.set_icon(icon)?;
                }
            }
            let splash_label = splash.label().to_string();

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                serve(
                    handle,
                    paths,
                    resolved,
                    identity,
                    lock,
                    app_origin,
                    splash_label,
                );
            });

            Ok(())
        })
        .on_window_event(lifecycle::on_window_event)
        .run(context)
        .expect("failed to run the app window");
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
    resolved: open::Resolved,
    identity: Identity,
    lock: Option<std::fs::File>,
    app_origin: window::AppOriginSlot,
    splash_label: String,
) {
    use tauri::Manager;

    let manifest = &resolved.manifest;
    if manifest.splash_path.is_some() {
        // Said, not swallowed: an app author who declared a splash page has to
        // learn it is not the one on screen. See `window::splash_style`.
        eprintln!(
            "tfsapp-hub: warning: this app declares \"splash_path\", which the hub cannot \
             serve — it has no per-app build step to bundle the page with. Showing the hub's \
             own splash in its colours instead."
        );
    }

    let environment =
        match app_env::resolve(&paths, manifest, &resolved.app_dir, app_env::Mode::Launch) {
            Ok(environment) => environment,
            Err(error) => return lifecycle::fatal_post_setup_error(app, error.to_string()),
        };
    let toolchain = match php::toolchain(&paths) {
        Ok(toolchain) => toolchain,
        Err(error) => return lifecycle::fatal_post_setup_error(app, error.to_string()),
    };

    let (sidecar, url) = match sidecar::start(
        &toolchain,
        &resolved.app_dir,
        &environment,
        manifest.async_worker,
        lock,
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
    if let Err(error) = window.navigate(url.parse().expect("a valid local backend URL")) {
        return lifecycle::fatal_post_setup_error(app, error.to_string());
    }

    app.manage(sidecar::Launch {
        url,
        product_name: identity.product_name,
    });
}
