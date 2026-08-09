# Changelog

## 0.1.0

First packaged release: `make build` produces a repaired, self-contained
`TFSAppHub_<version>_amd64.AppImage` — its bundled FrankenPHP runs instead of
segfaulting, no symlink in it points outside itself, it carries no frozen
`libwayland-*`, and a `.versions.txt` beside it names the frozen WebKit/GTK
stack and the glibc floor it requires. Copied alone to a machine with no Rust,
no PHP and no Tauri toolchain, that one file installs a Symfony app, shows it
in the shell's grid under its own name and icon, opens it into a working
window, and removes it again. `docker compose run --rm build` (under `build/`)
rebuilds the same artifact against an older base for anyone the default
release does not run on.
