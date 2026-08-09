# TFSAppHub — one binary that installs and runs several Symfony apps from their
# installed source. See README.md and the station's CONTRACT.md §1–§5 for the
# app contract both hosts share.
#
# Usage:
#   make resources
#   make check

.PHONY: resources sidecar composer check build

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
# rustfmt.toml. Tests run inside build/scripts/run-tests.sh's throwaway Secret
# Service, never against the host's real login keyring. No Tauri/AppImage
# integration here: opening a real window stays manual, as it does over there.
check:
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings
	build/scripts/run-tests.sh
	shellcheck build/scripts/*.sh

# The hub's own TFSAppHub_<version>_amd64.AppImage under
# hub/target/release/bundle/appimage/. Requires `make resources` first — see
# build-hub.sh's own up-front check — and the local Tauri/AppImage toolchain
# (see README.md Prerequisites).
build:
	build/scripts/build-hub.sh
