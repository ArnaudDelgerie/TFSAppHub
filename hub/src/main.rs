// The hub binary. Plan 001 leaves it at the minimum that compiles and answers
// `--version`; the CLI grammar it grows into — `--flags` act on the hub, bare
// words act on an app — arrives in plan 005, and the window that serves an app
// in plan 007.

fn main() {
    // The context is read here rather than from CARGO_PKG_* so that the
    // version the hub reports is the one baked into tauri.conf.json's package
    // info — the same source `--update` will later compare against a release
    // tag. It also keeps the Tauri codegen path exercised by an ordinary
    // `cargo build`.
    // Annotated because nothing here hands the context to a `tauri::Builder`
    // yet (plan 007 does): without a Builder to pin the runtime, `Context<R>`'s
    // default `R = Wry` is not enough for inference.
    let context: tauri::Context = tauri::generate_context!();
    let package_info = context.package_info();

    if std::env::args().skip(1).any(|arg| arg == "--version") {
        println!("{} {}", package_info.name, package_info.version);
        return;
    }

    println!(
        "{} {} (core {})",
        package_info.name,
        package_info.version,
        tfsapp_core::version()
    );
}
