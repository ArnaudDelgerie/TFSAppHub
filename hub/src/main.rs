// The hub binary. Plan 001 leaves it at the minimum that compiles and answers
// `--version`; plan 003 adds a temporary `open --identity <id>` form that opens
// one window under a runtime identity. The CLI grammar it grows into —
// `--flags` act on the hub, bare words act on an app — arrives in plan 005,
// and the app the window actually serves in plan 007.

mod identity;
mod manifest;
mod paths;
mod registry;

use identity::Identity;
use tauri::{WebviewUrl, WebviewWindowBuilder};

const OPEN_USAGE: &str =
    "usage: tfsapp-hub open --identity <identifier> [--name <product name>] [--icon <path.png>]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The context is read here rather than from CARGO_PKG_* so that the
    // version the hub reports is the one baked into tauri.conf.json's package
    // info — the same source `--update` will later compare against a release
    // tag. It also keeps the Tauri codegen path exercised by an ordinary
    // `cargo build`.
    // Annotated because the fall-through branch below hands the context to no
    // `tauri::Builder`: without a Builder to pin the runtime, `Context<R>`'s
    // default `R = Wry` is not enough for inference.
    let context: tauri::Context = tauri::generate_context!();

    if args.iter().any(|arg| arg == "--version") {
        let package_info = context.package_info();
        println!("{} {}", package_info.name, package_info.version);
        return;
    }

    if args.first().map(String::as_str) == Some("open") {
        let identity = parse_open(&args[1..]).unwrap_or_else(|error| {
            eprintln!("tfsapp-hub: {error}");
            eprintln!("{OPEN_USAGE}");
            std::process::exit(2);
        });
        open(identity, context);
        return;
    }

    let package_info = context.package_info();
    println!(
        "{} {} (core {})",
        package_info.name,
        package_info.version,
        tfsapp_core::version()
    );
}

/// Temporary argv parsing for `open`, deliberately minimal: the identity of an
/// app comes from its manifest and the registry (plan 004, `manifest::Manifest::
/// identity`), to be read by the real dispatcher (plan 005) behind a plain
/// `open <id>` (plan 007). Until an app is installed there is nothing to
/// resolve, so argv stays the only source of an identity here.
fn parse_open(args: &[String]) -> Result<Identity, String> {
    let mut identifier = None;
    let mut product_name = None;
    let mut icon_path = None;
    let mut index = 0;

    while index < args.len() {
        let flag = args[index].as_str();
        let slot = match flag {
            "--identity" => &mut identifier,
            "--name" => &mut product_name,
            "--icon" => &mut icon_path,
            other => return Err(format!("unknown argument {other}")),
        };
        *slot = Some(
            args.get(index + 1)
                .ok_or_else(|| format!("{flag} needs a value"))?
                .clone(),
        );
        index += 2;
    }

    let identifier = identifier.ok_or_else(|| "open needs --identity".to_string())?;
    Ok(Identity {
        // Defaulting to the identifier rather than to some prettified form of
        // it: the window title is the only place plan 003's manual checks can
        // read which identity a given window carries, so a lossless default is
        // worth more here than a nice one. Plan 004 replaces it with the
        // manifest's own field.
        product_name: product_name.unwrap_or_else(|| identifier.clone()),
        icon_path: icon_path.map(std::path::PathBuf::from),
        identifier,
    })
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
