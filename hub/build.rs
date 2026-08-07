fn main() {
    // Generates gen/schemas/* and validates tauri.conf.json, which is what
    // `tauri::generate_context!()` in main.rs consumes. Unlike the station's
    // build.rs there is nothing to stub beforehand: the hub declares no
    // `bundle.resources`, so a fresh clone compiles before `make sidecar` has
    // ever run.
    tauri_build::build()
}
