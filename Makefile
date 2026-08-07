# TFSAppHub — one binary that installs and runs several Symfony apps from their
# installed source. See README.md and the station's CONTRACT.md §1–§5 for the
# app contract both hosts share.
#
# Usage:
#   make sidecar
#   make check

.PHONY: sidecar check

# Download the FrankenPHP sidecar binary once into hub/resources. Idempotent:
# skips the download if the binary is already present. Unlike the station, this
# is not only a packaging prerequisite — the hub installs and runs every app
# with this interpreter, so nothing works without it.
sidecar:
	build/scripts/download-frankenphp-sidecar.sh

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
