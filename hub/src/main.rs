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
mod list;
mod manifest;
mod paths;
mod php;
mod platform;
mod prompt;
mod registry;
mod remove;
mod source;

use cli::{Command, EXIT_FAILED, EXIT_OK, EXIT_UNIMPLEMENTED, EXIT_USAGE};
use identity::Identity;
use tauri::{WebviewUrl, WebviewWindowBuilder};

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
        Command::Remove {
            id,
            purge,
            assume_yes,
        } => remove::run(&id, purge, assume_yes),
        Command::Platform => print_platform(),
        Command::OpenIdentity {
            identifier,
            product_name,
            icon_path,
        } => {
            open(
                Identity {
                    // Defaulting to the identifier rather than to some
                    // prettified form of it: the window title is the only place
                    // plan 003's manual checks can read which identity a given
                    // window carries, so a lossless default is worth more here
                    // than a nice one. Plan 007 replaces it with the manifest's
                    // own field.
                    product_name: product_name.unwrap_or_else(|| identifier.clone()),
                    icon_path: icon_path.map(std::path::PathBuf::from),
                    identifier,
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

/// Open one window under `identity`. The window lands on the checked-in
/// placeholder page: serving a real Symfony app behind a FrankenPHP sidecar is
/// plan 007's job, and nothing here should pretend otherwise.
fn open(identity: Identity, mut context: tauri::Context) {
    // Before the `Builder` exists, and so before anything has initialised GTK
    // — the whole point of the module. Everything identity-derived downstream
    // (app id, bus name, single-instance key, WM_CLASS, cookie store) reads
    // what this call leaves behind.
    identity::apply(&identity, &mut context);

    let relaunched = identity.identifier.clone();
    tauri::Builder::default()
        // Registered before every other plugin, per the plugin's own guidance,
        // and after the identity mutation above — which is what makes its key
        // this app's identifier rather than the hub's. Two different apps of
        // the one binary therefore never collide on it, and a second launch of
        // *this* app reaches the closure below instead of booting a second
        // process against the same data dir.
        //
        // Logging is all it does for now. The real behaviour — opening another
        // window on the sidecar already running, the station's plan 007
        // semantics — needs a sidecar, and there is none until plan 007.
        .plugin(tauri_plugin_single_instance::init(
            move |_app, args, cwd| {
                println!("tfsapp-hub: {relaunched}: relaunched with {args:?} from {cwd}");
            },
        ))
        .setup(move |app| {
            let mut window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title(&identity.product_name)
                    .inner_size(1000.0, 700.0);

            // Per-app, at runtime, from a path — the shape the hub needs, since
            // there is no per-app build step to bake an icon into. A failed
            // load is already reported by `load_icon` and leaves the window
            // with the generic one.
            if let Some(path) = &identity.icon_path {
                if let Some(icon) = identity::load_icon(path) {
                    window = window.icon(icon)?;
                }
            }

            window.build()?;
            Ok(())
        })
        .run(context)
        .expect("failed to run the app window");
}
