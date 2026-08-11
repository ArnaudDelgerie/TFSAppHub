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
    emit_releases_repo(Path::new("../build/releases-repo"));
    tauri_build::build()
}

/// Bake `build/releases-repo`'s value into `TFSAPP_RELEASES_REPO`
/// (`release.rs`'s `RELEASES_REPO`), one shared source of truth with
/// `build/scripts/release.sh` rather than a second copy of the same string.
/// Missing or empty is a build failure with the path in the message — not a
/// silent empty repo that would only surface as `--update` 404ing at
/// runtime.
fn emit_releases_repo(relative: &Path) {
    println!("cargo::rerun-if-changed={}", relative.display());
    let contents = fs::read_to_string(relative)
        .unwrap_or_else(|error| panic!("{}: {error}", relative.display()));
    let repo = contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or_else(|| {
            panic!(
                "{}: no value line (comments and blanks only)",
                relative.display()
            )
        });
    println!("cargo::rustc-env=TFSAPP_RELEASES_REPO={repo}");
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
