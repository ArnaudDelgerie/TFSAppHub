# Contributing to TFSAppHub

This is the contributor's document: how to build the hub, how to check your
work, how it is released. If you want to *use* the hub or write an app for it,
start from the [README](README.md). If you want to change how the hub itself
works, the reading starts here:

- [**`ARCHITECTURE.md`**](ARCHITECTURE.md) — how the hub works: the crate
  boundary, runtime identity, the install pipeline, the launch sequence, the
  bundled interpreter.
- [**`CONTRACT.md`**](CONTRACT.md) — what the hub and an app promise each
  other. Most behaviour changes are contract changes; read the relevant clause
  before changing the code that implements it.

## The three sibling checkouts

Development happens against three repositories, checked out side by side under
one parent directory:

| repository | what it is |
|---|---|
| `TFSAppHub` | this repository: the host — installs, runs, updates the apps, and serves the dev loop. |
| `TFSAppBundle` | the Symfony bundle an app uses on its PHP side (`TFSAppKernel`, `/healthz`, `tfsapp:init`, `tfsapp:doctor`). |
| `TFSAppTest` | a real Symfony project, the fixture app the hub's checks run against. |

[TFSAppTest](https://github.com/ArnaudDelgerie/TFSAppTest)'s README holds the
day-to-day procedure for trying an install, an update or a rollback against a
local hub checkout; this file does not restate it.

## Prerequisites

**To *use* the hub: nothing** — see the README's "Install". Everything below
is only for building it.

- **A local toolchain for development builds:**
  - **Rust** + the Tauri CLI (`cargo install tauri-cli`, or `cargo tauri` v2).
  - Linux build deps for Tauri v2 / WebKitGTK (`libwebkit2gtk-4.1-dev`,
    `libgtk-3-dev`, `libdbus-1-dev`, `librsvg2-dev`, `build-essential`, `curl`, `pkg-config`, `git`, `patchelf`, and `file` —
    `appimagetool` shells out to it, and so does this repo's own
    `fix-appimage-bundle.sh`).
  - GStreamer plugins from the build host: `gstreamer1.0-plugins-base`,
    `gstreamer1.0-plugins-good`, and either `gstreamer1.0-pipewire` or
    `gstreamer1.0-pulseaudio`. The AppImage freezes the installed plugins.
  - **curl** (to fetch the FrankenPHP sidecar and composer.phar).
  - **rustfmt** and **clippy** (`rustup component add rustfmt clippy`) and
    **shellcheck** — needed for `make check`.
  - **`gnome-keyring`** (the `gnome-keyring-daemon` binary) and **`dbus`**
    (`dbus-run-session`) — **test-only**, needed only for the explicit
    production-backend integration check (`make keyring-integration`). `make
    check` needs neither one.
- **Docker** for official releases and self-builds without a local toolchain —
  see "Building in Docker" below.

## Building

```sh
make resources # fetch the pinned FrankenPHP and composer.phar into hub/resources (once)
make build     # the hub's AppImage, target/release/bundle/appimage/
```

`make resources` is not only a packaging step: the hub installs *and* runs
every app with that interpreter and that Composer, so nothing works without
them. (`make sidecar` and `make composer` fetch one half each, separately.)
`make build` refuses up front if `make resources` has not run.

## Checking your work

`make check` is the gate, and it is the whole gate. It runs, cheapest failure
first:

1. `cargo fmt --check` — stock rustfmt, no project `rustfmt.toml`.
2. `cargo clippy --all-targets --all-features -- -D warnings`.
3. the whole workspace's unit suite, serialized.
4. `shellcheck` over the build scripts.
5. the release-chain script — `build-hub.sh` and `release.sh` run end to end in
   a throwaway git repository against a fake `cargo` and `gh`; nothing it does
   leaves the machine.

**Never a bare `cargo test`.** In plain terms: the unit suite spawns real
short-lived child processes that probe the filesystem and process lifecycle,
and the Makefile runs them one at a time precisely so one probe's child cannot
race another probe's cleanup — a plain `cargo test` runs them in parallel.
Separately, the production Secret Service tests are excluded from the ordinary
suite and run only under `make keyring-integration`, which starts a private
D-Bus session with a throwaway keyring; running them any other way would touch
your real login keyring. `cargo fmt` and `cargo clippy` are fine to run
standalone while iterating — they touch nothing outside the tree.

`make keyring-integration` is the explicit production-backend smoke check: the
round trip through a real `gnome-keyring-daemon`, started *ephemeral* by the
harness — never your real login keyring — and torn down with its temporary
state on exit. A missing `dbus-run-session` or `gnome-keyring-daemon` is a
named setup failure, not a reason to skip it.

## Building in Docker

`cd build && docker compose run --rm build` installs the whole toolchain inside
a container and runs `make build` there. `docker compose run --rm check` runs
`make check` the same way. The base image defaults to the official release
base (`ubuntu:22.04` today); see `build/compose.yaml`'s own header for the
uid/gid, FUSE and caching details.

## Releasing

Official releases are built by `make release` (see `build/scripts/release.sh`)
in Docker on the oldest Ubuntu LTS still in standard support: `ubuntu:22.04`
now, `ubuntu:24.04` from April 2027. This pins the release's native ABI floor
to a supported base. Local development builds can use the host toolchain.

`make release` never runs unattended and refuses anything that would make the
release unprovenanced:

- a **clean tree whose `HEAD` is pushed to its upstream** — the released
  AppImage is built from (or, when reused, recorded from) exactly that
  revision;
- `gh auth login` — the release is published as GitHub release assets on the
  source repository itself; `build/releases-repo` names that repository and is
  the single source for both the upload destination and the repository baked
  into the hub for its self-updates — **a fork edits that file**;
- a `## <version>` section in `CHANGELOG.md` matching `hub/Cargo.toml`'s
  version — the notes are taken from it and end with a
  `Built from <repo>@<sha>` line. That line is provenance, not authenticity:
  it says where the binary came from, and signs nothing.

A same-commit AppImage can be reused instead of rebuilt; the build host's
floors are recorded in the release's `.versions.txt` beside it.
