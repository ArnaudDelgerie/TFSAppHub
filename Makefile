# TFSAppHub — one binary that installs and runs several Symfony apps from their
# installed source. See README.md and CONTRACT.md §1–§5 for the app contract.
#
# Usage:
#   make resources
#   make check

.PHONY: resources sidecar composer check keyring-integration build release

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
# build/scripts/*.sh and build/scripts/tests/*.sh, then the release-chain
# script — five gates, cheapest failure first: formatting needs no compilation
# at all, clippy is the cheapest failure to report after that. The fifth
# gate, the release-chain script, runs build-hub.sh and release.sh end to end
# in a throwaway git repository against a fake cargo and gh — nothing it does
# leaves the machine. Formatting is
# stock rustfmt with no project rustfmt.toml. The ordinary unit suite never
# starts or contacts a Secret Service, so `make check` is safe without D-Bus or
# gnome-keyring-daemon. A separate explicit integration target will exercise
# that production backend. The hub test binary includes process and filesystem
# lifecycle probes; run it in one thread so one probe's short-lived child cannot
# race another's cleanup. No Tauri/AppImage integration here: opening a real
# window stays manual, as it does over there.
check:
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test -- --test-threads=1
	shellcheck build/scripts/*.sh build/scripts/tests/*.sh
	build/scripts/tests/release-chain.sh

# The deliberately explicit production-backend smoke checks. This alone starts
# the repository-owned private D-Bus session and throwaway Secret Service; the
# ordinary `check` target must never invoke this harness. The filter runs both
# ignored production tests — the round trip, and the frozen-service test that
# SIGSTOPs the harness's own daemon (the PID the harness exports) and expects
# the deadline to answer. One test thread, so the stopped daemon cannot stall
# the round-trip test behind it.
keyring-integration:
	build/scripts/run-tests.sh -p tfsapp-hub secrets::tests::production_keyring_ -- --ignored --test-threads=1

# The hub's own TFSAppHub_<version>_amd64.AppImage under
# target/release/bundle/appimage/ (the workspace's shared target/, not
# hub/target/). Requires `make resources` first — see build-hub.sh's own
# up-front check — and the local Tauri/AppImage toolchain (see CONTRIBUTING.md
# Prerequisites).
build:
	build/scripts/build-hub.sh

# Build (or reuse a build recorded from this exact HEAD) the AppImage, then
# publish it, its .versions.txt and its SHA256SUMS.txt on the source repo as
# GitHub release assets. Requires a clean tree pushed to its upstream — the
# released HEAD is pinned, re-checked before publishing, and named in the
# notes' Built from line — plus `gh auth login` and a "## <version>" section
# in CHANGELOG.md matching hub/Cargo.toml's version. Never runs unattended —
# see release.sh's own header for the full flow.
release:
	build/scripts/release.sh
