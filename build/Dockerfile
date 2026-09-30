# A second door onto exactly `make build` / `make check` — nothing this file
# does is container-only. It installs what README.md's "Prerequisites"
# already lists for a local build, then hands off to the same Makefile
# targets; every container-specific trick (the base image choice, the uid/gid
# match, the mounted caches, FUSE) lives in compose.yaml instead. The split is
# deliberate: the Dockerfile is the recipe — what a machine needs to run
# `make build`, nothing more — while the tricks are choices a person changes
# per machine (the base to try when the default fails, the uid/gid that keeps
# `target/` from coming back root-owned, the caches whose downloads depend on
# persisting, FUSE for `appimagetool`), and compose.yaml is the layer they
# edit without rebuilding the image.

# The default is the official release base: the oldest Ubuntu LTS still in
# standard support (ubuntu:22.04 now, ubuntu:24.04 from April 2027).
# A custom base must be Debian or Ubuntu: apt-get below and dpkg in
# fix-appimage-bundle.sh require Debian-family package tools.
ARG BASE_IMAGE=ubuntu:22.04
FROM ${BASE_IMAGE}

# The same prerequisites README.md's "Build" section lists for building
# outside a container: Tauri v2 / WebKitGTK's Linux deps, curl (to fetch the
# FrankenPHP sidecar and composer.phar), shellcheck (`make check`), and
# dbus + gnome-keyring (test-only — the ephemeral Secret Service
# run-tests.sh stands up, never the host's real login keyring).
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential curl ca-certificates pkg-config file git \
      libwebkit2gtk-4.1-dev libgtk-3-dev libdbus-1-dev librsvg2-dev \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-pipewire \
      dbus gnome-keyring shellcheck \
    && rm -rf /var/lib/apt/lists/*

# Rust + the Tauri CLI, installed system-wide under a fixed path rather than
# any one user's $HOME: compose.yaml runs this image with `--user <uid>:<gid>`
# matching whoever invoked it (so the mounted target/ and hub/resources come
# back owned by them, not root), and that uid has no account in this image —
# a fixed, world-writable install location is what lets the same image serve
# any uid without baking one in at build time. $HOME itself is set separately
# (nothing under it is baked here) purely so `cargo tauri build`'s own
# linuxdeploy cache has somewhere to resolve to — compose.yaml mounts it.
#
# compose.yaml mounts the *host's* `~/.cargo/registry` over
# `$CARGO_HOME/registry` (not the whole `$CARGO_HOME`) so a container run
# reuses crates the host's own `cargo build` already downloaded, without
# shadowing `$CARGO_HOME/bin` — the rustup shims and `cargo-tauri` installed
# below, which only exist inside this image and would otherwise disappear
# under the host's own (toolchain-less) directory of the same name.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    HOME=/home/builder
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --component rustfmt --component clippy \
    && cargo install tauri-cli --version "^2" \
    && mkdir -p "$CARGO_HOME/registry" "$HOME/.cache/tauri" \
    && chmod -R a+rwX "$RUSTUP_HOME" "$CARGO_HOME" "$HOME"

# linuxdeploy-plugin-gstreamer invokes patchelf directly when it rewrites the
# bundled plugin binaries; the Tauri CLI does not install that host tool.
RUN apt-get update && apt-get install -y --no-install-recommends patchelf \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
CMD ["make", "build"]
