// The hub binary. Plan 001 leaves it at the minimum that compiles and answers
// `--version`; the CLI grammar it grows into — `--flags` act on the hub, bare
// words act on an app — arrives in plan 005, and the window that serves an app
// in plan 007.

fn main() {
    if std::env::args().skip(1).any(|arg| arg == "--version") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return;
    }

    println!(
        "{} {} (core {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        tfsapp_core::version()
    );
}
