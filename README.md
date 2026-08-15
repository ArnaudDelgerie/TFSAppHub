# TFSAppHub

One AppImage that installs, updates and runs **several** Symfony desktop apps
from their source, using its own bundled FrankenPHP as the PHP interpreter.

Since **2026-08-08 it is the only host**. `TFSAppWorkstation`, which used to build
one standalone AppImage per app and to host the dev loop, is archived; the dev loop
moved here (`tfsapp-hub dev <path>`). The point of that move: writing
a TFSApp will need no Rust toolchain, no Tauri CLI and no GTK dev libraries — just
the hub and a Symfony project.

## Install

Download the latest `TFSAppHub_<version>_amd64.AppImage` from the
[releases repo](https://github.com/ArnaudDelgerie/TFSAppHub-releases/releases),
make it executable, and use it — nothing else to install first:

```sh
chmod +x TFSAppHub_*.AppImage
./TFSAppHub_*.AppImage install path/to/project
./TFSAppHub_*.AppImage open myapp
```

That is the whole prerequisite list: no Rust, no PHP, no Tauri CLI, no GTK
development libraries. The AppImage carries its own FrankenPHP and its own
Composer, and installs and runs every app with them — see "Trust posture"
below for what that means. If it does not start on your machine, see
"It does not start".

The fragile native half (FrankenPHP, WebKitGTK, the glibc floor) is built once;
everything added afterwards is pure PHP and has no ABI surface. N binaries
collapse to 1, `composer install` *is* the compatibility manifest — it runs with
the very interpreter that will later serve the app — and per-app update falls out
of `git fetch` for free.

Status: **it is packaged, and it installs, and it opens.** `make build`
produces a self-contained AppImage from a machine with nothing else built —
`install` snapshots a local project, resolves its dependencies with the
bundled PHP, runs its lifecycle commands, registers it and — by default —
writes it a `.desktop` entry, so it opens from the shell's own grid and
search under its own name and icon, with no terminal and the hub not
otherwise running; `open` gives that app a real window on its own FrankenPHP,
with its own data directory and its own cookie store, so two apps open side
by side stay isolated; `list` and `remove` close the loop, the latter taking
the entry with it; `run <id> <alias>` runs one of an app's own declared
`bin/console` commands in the foreground, alongside `run --stop`/
`run --replace` to release one without a manual `kill`; `update <id>`
re-resolves an app's own source and replaces the installed version with it,
snapshotting the database first and reverting code and database together if
anything fails; `rollback <id>` undoes a successful update afterwards,
putting the previous source and database back and setting the one being left
behind aside as a named rescue dump; `--update` replaces the hub itself with
its own latest release, verified against the release's checksums, and
revalidates any installed app whose PHP moved under it the next time it is
used; `--rollback` undoes that — the previous hub binary and the registry it
recorded, both back exactly as they were, offline, in under a second. Git
sources are recognised and refused with a message saying so, rather than
pretended. The design record and the plan queue live under the git-ignored
`.project/`.

## Build

Building the hub yourself — as opposed to downloading it, see "Install" above
— is only for contributing to the hub itself, or for producing an AppImage
against a lower glibc floor than the published release (see "It does not
start").

```sh
make resources # fetch the pinned FrankenPHP and composer.phar into hub/resources (once)
make check     # cargo fmt --check, clippy -D warnings, unit tests, shellcheck
make build     # the hub's own AppImage, target/release/bundle/appimage/
```

`make resources` is not only a packaging step: the hub installs *and* runs every
app with that interpreter and that Composer, so no system-wide PHP is required —
and nothing works without them. (`make sidecar` and `make composer` fetch one
half each, if you want them separately.) `make build` refuses up front if
`make resources` has not run.

### Prerequisites

**To *use* the hub: nothing** — see "Install" above. Everything below is only
for building it.

- **A local toolchain, the primary and fully-supported path:**
  - **Rust** + the Tauri CLI (`cargo install tauri-cli`, or `cargo tauri` v2).
  - Linux build deps for Tauri v2 / WebKitGTK (`libwebkit2gtk-4.1-dev`,
    `libgtk-3-dev`, `libdbus-1-dev`, `librsvg2-dev`, `build-essential`, `curl`,
    `file` — `appimagetool` shells out to it, and so does this repo's own
    `fix-appimage-bundle.sh`, …).
  - **curl** (to fetch the FrankenPHP sidecar and composer.phar).
  - **rustfmt** and **clippy** (`rustup component add rustfmt clippy`) and
    **shellcheck** — needed for `make check`. A missing Rust component fails
    with rustup's own `rustup component add …` message.
  - **`gnome-keyring`** (the `gnome-keyring-daemon` binary) and **`dbus`**
    (`dbus-run-session`) — **test-only**, needed only for the explicit
    production-backend integration check. `make check` needs neither one and
    never contacts a Secret Service. That integration check starts an
    *ephemeral*, throwaway provider, never your real login keyring. Neither is
    the GNOME desktop, and neither is a runtime dependency of the packaged hub.
- **Or, a second door onto the exact same build: only Docker.**
  `cd build && docker compose run --rm build` installs everything above
  inside a container and runs `make build` there — nothing in the recipe
  is container-only, and nothing under `build/scripts/` or in Rust knows the
  container exists. `docker compose run --rm check` runs `make check` the
  same way. See `build/compose.yaml`'s own header for the uid/gid, FUSE and
  caching details, and "It does not start" for `BASE_IMAGE`, the one
  container-only knob.

## It does not start

You downloaded the AppImage, made it executable, and running it does
nothing — or the terminal prints something about a version of `GLIBC` it
cannot find. That is the dynamic loader failing before a single line of the
hub's own code runs, so there is no error message for the hub to improve:
the release was linked against a glibc newer than the one on your machine.

Every release's floor is recorded beside it: download the matching
`TFSAppHub_<version>_amd64.versions.txt` from the same
[release](https://github.com/ArnaudDelgerie/TFSAppHub-releases/releases) and
read its `glibc_floor` line — the lowest glibc that build can possibly run
on. Compare it against your own with `ldd --version` (the number on its first
line) or `getconf GNU_LIBC_VERSION`. If yours is lower, that AppImage will
never start on your machine, whatever else you try.

The fix is one command, with only Docker installed — no Rust, no Tauri CLI,
nothing this repo's own build normally needs:

```sh
git clone https://github.com/ArnaudDelgerie/TFSAppHub.git
cd TFSAppHub/build
BASE_IMAGE=debian:12 docker compose run --rm build
```

`BASE_IMAGE` is a plain Docker image reference — pick one whose own glibc is
at or below yours. `debian:12` (glibc 2.36) and `ubuntu:24.04` (glibc 2.39,
this project's own default) are the two bases this repo has actually built
against; either is a safe starting guess older than most machines still in
use. The result lands at
`TFSAppHub/target/release/bundle/appimage/TFSAppHub_<version>_amd64.AppImage`,
with its own `.versions.txt` recording the lower floor it now needs — chasing
the *lowest possible* base is deliberately not this project's job (see
`ARCHITECTURE.md`'s "Packaging"); one older base, chosen by you, is.

## Writing an app for it

An app is a Symfony project with three files and one route: `bin/console`,
`public/index.php`, a `tfsapp.config.json` manifest, and `GET /healthz` → `200`.
No Rust, no Tauri CLI, no GTK development libraries, no base class to extend.

[**`CONTRACT.md`**](CONTRACT.md) is what the hub and an app promise each other,
and it is canonical — there is no second host to keep it in step with. Apps use
the [TFSAppBundle](https://github.com/ArnaudDelgerie/TFSAppBundle) Symfony bundle
for the PHP side of it.

[**`ARCHITECTURE.md`**](ARCHITECTURE.md) is how the hub itself works: the crate
boundary, runtime identity, the install pipeline, the launch sequence, the
bundled interpreter.

Install here is a **snapshot**: editing the original source has no effect until
an explicit `update`, which is what makes `composer install`, migrations and a
warm persistent cache meaningful — and exactly what makes it useless as a dev
loop. The dev loop is its own mode (`dev <path>`), and it serves
live source **in place**: it watches nothing, compiles nothing and builds no
assets. Your build tool already has a `--watch`.

### Where the station went

[TFSAppWorkstation](https://github.com/ArnaudDelgerie/TFSAppWorkstation) built
one standalone AppImage per app and hosted the dev loop. It was archived on
2026-08-08 and this repo took both jobs. The reasoning is written once, in
`.project/decisions/001-single-host.md`; the short version is that a second host
cost a permanent parity tax and its own compatibility problem per app, while the
thing it was protecting — a single double-clickable file — is recoverable as a
*packaging mode* of the hub rather than as a second program.

This repo is a fresh start, not a fork: the reusable Rust modules were copied
over and adapted. The modules that diverge — identity resolution above all —
could not serve both identity models (baked at build vs resolved at runtime) in
one tree.

## Trust posture

Installing an app runs a third party's PHP, Composer scripts included, with the
user's full rights. So does downloading an unsigned AppImage — the trust decision
is the same one, and the hub is the more auditable of the two (a repo at a pinned
tag can be read and diffed; a ~130 MB binary cannot). What genuinely differs is
friction and co-residency, not the kind of risk.

No sandbox is claimed. Sources default to a pinned tag or commit, never a branch.
For a developer audience this is exactly `composer require`, and pretending
otherwise would be worse than saying it.

Checking `SHA256SUMS.txt` establishes download integrity, not publisher
authenticity; choosing which source or repository to trust remains the user's
call ([decision 004](.project/decision/004-integrity-not-authenticity.md)).

### Stored secrets are namespaced, not isolated

Each app's secrets go into the OS keyring under that app's `identifier` as the
service name. That keeps two apps from **colliding**. It does not keep them from
**reading each other**: the Secret Service authorises per login session, so any
process running as this user can list and read any service's entries. Nothing
the hub does introduces this and nothing it could do would remove it — the only
real fix is sandboxing the processes, which is a change of distribution format
and is not on the roadmap.

Read per-`identifier` storage as tidiness, not as secrecy.
[`CONTRACT.md` §5](CONTRACT.md) states the full guarantee, including the one
thing the hub *is* strict about: a webview reaches its secret store through the
window it belongs to and can never name another app's.
