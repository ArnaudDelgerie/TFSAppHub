# TFSAppHub — one binary that installs and runs several Symfony apps from their
# installed source. See README.md and CONTRACT.md §1–§5 for the app contract.
#
# Usage:
#   make resources
#   make check

.PHONY: resources sidecar composer check build release

# Everything the hub ships that is not built from source. Both halves are
# runtime prerequisites, not packaging ones: the hub installs and runs every
# app with its own PHP and its own Composer, so `install` does nothing without
# them.
resources: sidecar composer

# Download the FrankenPHP sidecar binary once into hub/resources. Idempotent:
# skips the download if the binary is already present. Unlike the station, this
# is not only a packaging prerequisite — the hub installs and runs every app
# with this interpreter, so nothing works without it.
sidecar:
	build/scripts/download-frankenphp-sidecar.sh

# Download composer.phar once into hub/resources, likewise idempotent. Run
# under the bundled FrankenPHP, never under a host PHP — which is what makes an
# app's dependency tree resolve against the interpreter that will serve it.
composer:
	build/scripts/download-composer.sh

# Run cargo fmt --check, then clippy, then unit tests, then shellcheck over
# build/scripts/*.sh — same four gates and same order as the station's, cheapest
# failure first: formatting needs no compilation at all, clippy is the cheapest
# failure to report after that. Formatting is stock rustfmt with no project
# rustfmt.toml. The ordinary unit suite never starts or contacts a Secret
# Service, so `make check` is safe without D-Bus or gnome-keyring-daemon. A
# separate explicit integration target will exercise that production backend.
# No Tauri/AppImage integration here: opening a real window stays manual, as it
# does over there.
check:
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test
	shellcheck build/scripts/*.sh

# The hub's own TFSAppHub_<version>_amd64.AppImage under
# target/release/bundle/appimage/ (the workspace's shared target/, not
# hub/target/). Requires `make resources` first — see build-hub.sh's own
# up-front check — and the local Tauri/AppImage toolchain (see README.md
# Prerequisites).
build:
	build/scripts/build-hub.sh

# Build (or reuse) the AppImage, then publish it and its SHA256SUMS.txt to the
# releases repo as GitHub release assets. Requires `gh auth login` and a
# "## <version>" section in CHANGELOG.md matching hub/Cargo.toml's version.
# Never runs unattended — see release.sh's own header for the full flow.
release:
	build/scripts/release.sh
