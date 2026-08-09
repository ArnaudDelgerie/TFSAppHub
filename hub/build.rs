use std::{fs, path::Path};

/// Declaring `bundle.resources` in `tauri.conf.json` means `tauri_build::build()`
/// validates every declared resource path exists — even on a plain
/// `cargo check`, with no AppImage in sight. `hub/resources/` is git-ignored
/// (`make sidecar` and `make composer` populate it), so a fresh clone would
/// fail to compile before those targets have ever run. Writing empty stubs
/// here keeps that build green; `tfsapp_core::sidecar::is_present` and the
/// packaged/dev/system resolution in `platform.rs` and `php.rs` make sure a
/// 0-byte stub is never mistaken for the real download.
fn main() {
    ensure_resource_stub(Path::new("resources/frankenphp"));
    ensure_resource_stub(Path::new("resources/composer.phar"));
    tauri_build::build()
}

fn ensure_resource_stub(relative: &Path) {
    if relative.exists() {
        return;
    }
    if let Some(parent) = relative.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|error| panic!("{}: {error}", parent.display()));
    }
    fs::write(relative, []).unwrap_or_else(|error| panic!("{}: {error}", relative.display()));
}
