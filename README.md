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
./TFSAppHub_*.AppImage install github:owner/repo
# or, for a project you have locally:
./TFSAppHub_*.AppImage publish path/to/project --local out/
./TFSAppHub_*.AppImage install out/<name>-<version>/<name>-<version>.tar.gz
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
the very interpreter that will later serve the app — and per-app update
resolves a new release without rebuilding the native half.

Status: **it is packaged, and it installs, and it opens.** `make build`
produces a self-contained AppImage from a machine with nothing else built —
`install` installs a release, from a forge or from a local archive
`publish --local` wrote, resolves its dependencies with the
bundled PHP, runs its lifecycle commands, registers it and — by default —
writes it a `.desktop` entry, so it opens from the shell's own grid and
search under its own name and icon, with no terminal and the hub not
otherwise running; `open` gives that app a real window on its own FrankenPHP,
with its own data directory and its own cookie store, so two apps open side
by side stay isolated; `list` and `remove` close the loop, the latter taking
the entry with it; `run <id> <alias>` runs one of an app's own declared
`bin/console` commands in the foreground, alongside `run --stop`/
`run --replace` to release one without a manual `kill`; `update <id>`
re-resolves an app's own source, or `update <id> <archive.tar.gz>` supplies a
local release explicitly, and replaces the installed version with it,
snapshotting the database first and reverting code and database together if
anything fails; `rollback <id>` undoes a successful update afterwards,
putting the previous source and database back and setting the one being left
behind aside as a named rescue dump; `--update` replaces the hub itself with
its own latest release, verified against the release's checksums, and
revalidates any installed app whose PHP moved under it the next time it is
used; `--rollback` undoes that — the previous hub binary and the registry it
recorded, both back exactly as they were, offline, in under a second. An app
declaring `file_associations` appears in the file manager's "Open with" menu
for the types it names — including `inode/directory`, with the receiver's
`directories` opt-in — and `open <id> -- <path>...` hands local paths to a
declared receiver, files for any receiver and directories for one that opted
in: a queue in the hub process, a targeted notification, an
explicit acknowledgement; replayable until acknowledged, never durable
across a crash. Git
sources are recognised and refused with a message saying so, rather than
pretended. The design record and the plan queue live under the git-ignored
`.project/`.

## Build

Official releases are built by `make release` in Docker on the oldest Ubuntu
LTS still in standard support: `ubuntu:22.04` now, then `ubuntu:24.04` from
April 2027. This pins the release's native ABI floor to a supported base.
Local development builds can use the host toolchain; a user who needs to
rebuild the release can use the Docker command below (see "It does not
start").

```sh
make resources # fetch the pinned FrankenPHP and composer.phar into hub/resources (once)
make check     # cargo fmt --check, clippy -D warnings, unit tests, shellcheck, release-chain test
make keyring-integration # explicit production Secret Service smoke; needs dbus + gnome-keyring
make build     # the hub's own AppImage, target/release/bundle/appimage/
```

`make resources` is not only a packaging step: the hub installs *and* runs every
app with that interpreter and that Composer, so no system-wide PHP is required —
and nothing works without them. (`make sidecar` and `make composer` fetch one
half each, if you want them separately.) `make build` refuses up front if
`make resources` has not run.

Releasing the hub (`make release`, see `build/scripts/release.sh`) accepts only a
clean tree whose `HEAD` is pushed to its upstream: the released AppImage is
built from — or, when reused, recorded from — exactly that revision, and the
release notes end with a `Built from <repo>@<sha>` line naming it. That line is
provenance, not authenticity: it says where the binary came from, and signs
nothing. A fresh release build runs through Docker Compose on the official
base; a same-commit AppImage can still be reused.

### Prerequisites

**To *use* the hub: nothing** — see "Install" above. Everything below is only
for building it.

- **A local toolchain for development builds:**
  - **Rust** + the Tauri CLI (`cargo install tauri-cli`, or `cargo tauri` v2).
  - Linux build deps for Tauri v2 / WebKitGTK (`libwebkit2gtk-4.1-dev`,
    `libgtk-3-dev`, `libdbus-1-dev`, `librsvg2-dev`, `build-essential`, `curl`,
    `pkg-config`, `git`, `patchelf`, and `file` — `appimagetool` shells out to
    it, and so does this repo's own `fix-appimage-bundle.sh`).
  - GStreamer plugins from the build host: `gstreamer1.0-plugins-base`,
    `gstreamer1.0-plugins-good`, and either `gstreamer1.0-pipewire` or
    `gstreamer1.0-pulseaudio`. The AppImage freezes the installed plugins.
  - **curl** (to fetch the FrankenPHP sidecar and composer.phar).
  - **rustfmt** and **clippy** (`rustup component add rustfmt clippy`) and
    **shellcheck** — needed for `make check`. A missing Rust component fails
    with rustup's own `rustup component add …` message.
  - **`gnome-keyring`** (the `gnome-keyring-daemon` binary) and **`dbus`**
    (`dbus-run-session`) — **test-only**, needed only for the explicit
    production-backend integration check. `make check` needs neither one and
    never contacts a Secret Service. `make keyring-integration` starts an
    *ephemeral*, throwaway provider, never your real login keyring. Neither is
    the GNOME desktop, and neither is a runtime dependency of the packaged hub.
- **Docker for official releases and self-builds without a local toolchain.**
  `cd build && docker compose run --rm build` installs everything above
  inside a container and runs `make build` there — nothing in the recipe
  is container-only inside the build itself. `BASE_IMAGE` defaults to the
  official `ubuntu:22.04` base. `docker compose run --rm check` runs
  `make check` the same way. See `build/compose.yaml`'s own header for the
  uid/gid, FUSE and caching details.

## It does not start

You downloaded the AppImage, made it executable, and running it does
nothing — or the terminal prints something about a version of `GLIBC`,
`GLIBCXX`, or `CXXABI` it cannot find. That is the dynamic loader failing
before a single line of the hub's own code runs: a system library is older
than the version required by the release.

Every release's floor is recorded beside it: download the matching
`TFSAppHub_<version>_amd64.versions.txt` from the same
[release](https://github.com/ArnaudDelgerie/TFSAppHub-releases/releases) and
read its `glibc_floor` and `glibcxx_floor` lines. They are the lowest glibc
version and `GLIBCXX` symbol version the machine's `libstdc++.so.6` must
provide. Compare glibc with `ldd --version` (the number on its first line)
or `getconf GNU_LIBC_VERSION`. A machine below either floor cannot start that
AppImage. `CXXABI` symbols are provided by the same C++ runtime.

To replace an older AppImage built on a newer host, rebuild on the official
base with Git and Docker installed — no local Rust or Tauri CLI is needed:

```sh
git clone https://github.com/ArnaudDelgerie/TFSAppHub.git
cd TFSAppHub/build
docker compose run --rm build
```

The default `BASE_IMAGE=ubuntu:22.04` is the official release base, whatever
your distribution. It is the oldest base that provides `webkit2gtk-4.1`;
its measured floors are glibc 2.35 and `GLIBCXX_3.4.30`. The result lands at
`TFSAppHub/target/release/bundle/appimage/TFSAppHub_<version>_amd64.AppImage`,
with its own `.versions.txt`. A machine below either Jammy floor needs newer
system libraries; rebuilding on the same base cannot lower those floors.
The project pins this base by its support rule
instead of chasing the lowest possible ABI on every build. Custom
`BASE_IMAGE` values must be Debian/Ubuntu family images: the build uses
`apt-get` and records package versions with `dpkg`. Non-Debian bases are
unsupported. `debian:12` is no longer suggested: its WebKitGTK is marked
end-of-life with limited support in bookworm. From April 2027 the official
base moves to `ubuntu:24.04`; rebuilding on Jammy after its standard support
ends would freeze a WebKitGTK that no longer receives those fixes.

If the hub starts but microphone capture fails, check the installed app's
`log/hub.log` for GStreamer element or plugin loading errors. The AppImage
carries the build host's GStreamer plugins, while some optional graphics and
audio libraries and the audio server come from your machine. A Docker build
on an older compatible base can avoid newer symbol requirements; it cannot
provide a missing host audio service or device.

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

Install here is a **snapshot of a release**: nothing edits the installed tree
until an explicit `update`, which is what makes `composer install`, migrations
and a warm persistent cache meaningful — and exactly what makes it useless as a
dev loop. The dev loop is its own mode (`dev <path>`), and it serves
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
[`CONTRACT.md` §5](contract/5-the-apps-own-state.md) states the full guarantee, including the one
thing the hub *is* strict about: a webview reaches its secret store through the
window it belongs to and can never name another app's.

### A declared microphone can be captured silently

Declaring `actions.media.microphone` buys a line in a manifest, readable
before install — it does not buy a runtime prompt or an indicator that a
capture is in progress; neither ships yet. Refusing the capability outright
would not close this gap either: an installed app already runs with the
user's full rights and can reach a microphone through a subprocess with
nothing declared anywhere, the same trust decision the AppImage comparison
above already makes. The declared route is the auditable one, not the only
one — see [decision 007](.project/decision/007-the-microphone-is-a-declared-capability.md)
for the reasoning and what is honestly still missing.

### Declared user directories are a legible line, not a filesystem boundary

Declaring `actions.paths` (`downloads`, `pictures`, and the rest of GLib's
eight special directories) buys the app a `TFS_USER_<NAME>_DIR` variable it
would otherwise have to guess at — never a sandbox: PHP runs with the user's
full rights regardless, exactly like `save_path` and the microphone above. A
declared member GLib cannot resolve reports as an absent variable rather than
a guessed `$HOME`-based path, and `$HOME` itself is deliberately not a ninth
member — it already reaches PHP through the ordinary process environment. See
[decision 008](.project/decision/008-user-directories-are-a-declared-capability.md)
for the reasoning and what is left open.
