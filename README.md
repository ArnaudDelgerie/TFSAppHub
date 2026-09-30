# TFSAppHub

One program installs, updates and runs several Symfony desktop apps on your
machine: **one host runs every app.** `TFSAppHub` is a single AppImage that
carries its own PHP interpreter (FrankenPHP) and its own Composer; an app is
just a Symfony project. `tfsapp-hub install github:owner/repo` installs one,
and `tfsapp-hub open myapp` gives it a real window of its own, with its own
data directory and its own cookie store — two apps open side by side stay
isolated.

The fragile native half — interpreter, webview, the glibc floor — is built
once, here. Everything an app adds afterwards is pure PHP with no ABI surface:
writing one needs no Rust toolchain, no Tauri CLI and no GTK development
libraries, just the hub and a Symfony project. See
["Make your first app"](#make-your-first-app).

## Install

Download the latest `TFSAppHub_<version>_amd64.AppImage` from this
repository's [releases](https://github.com/ArnaudDelgerie/TFSAppHub/releases),
make it executable, and run it:

```sh
chmod +x TFSAppHub_*_amd64.AppImage
./TFSAppHub_*_amd64.AppImage --version
```

That is the whole prerequisite list: no Rust, no PHP, no Tauri CLI, no GTK
development libraries. The AppImage carries its own FrankenPHP and its own
Composer, and installs and runs every app with them — see "Trust posture"
below for what that means. If it does not start on your machine, see
["It does not start"](#it-does-not-start).

The commands below assume it is reachable as `tfsapp-hub` — rename the file,
or symlink it:

```sh
ln -s "$(pwd)/TFSAppHub_"*"_amd64.AppImage" ~/.local/bin/tfsapp-hub
```

## Use

`tfsapp-hub --help` prints every command, with its options. The everyday
shape:

**Install an app**, from a forge release or from a local archive:

```sh
tfsapp-hub install github:owner/repo
tfsapp-hub publish path/to/project --local out/     # write a release archive locally
tfsapp-hub install out/<name>-<version>/<name>-<version>.tar.gz
```

Install is a snapshot of one release: nothing edits the installed tree until an
explicit `update`, which is what makes `composer install`, migrations and a
warm persistent cache meaningful. By default the hub also writes the app a
`.desktop` entry (`--no-desktop-entry` opts out), so it opens from your
shell's grid and search under its own name and icon — no terminal, the hub not
otherwise running. A plain `git:` source is recognised and refused with a
message saying so, rather than pretended.

**Apps:**

- `list` — the installed apps.
- `open <id>` — open an installed app's window; `open <id> -- <path>...`
  hands local paths to an app that declared a receiver. An app declaring
  `file_associations` also appears in the file manager's "Open with" menu for
  the types it names.
- `update <id>` — re-resolve the app's source and replace the installed
  version; `update <id> <archive.tar.gz>` supplies a local release instead.
  The database is snapshotted first, and code and database revert together if
  anything fails. `rollback <id>` undoes a successful update afterwards,
  putting the previous source and database back and setting the version left
  aside as a named rescue dump.
- `run <id> <alias>` — run one of the app's own declared commands in the
  foreground; `run --stop` / `run --replace` release it without a manual kill.
- `export <id> <path>` / `import <id> <path>` — an app's data to and from a
  `.tar.gz`, between machines.
- `remove <id>` — uninstall, desktop entry with it; `--purge` (or
  `purge <identifier>` later, on leftover data) also drops its data.

**The hub itself:**

- `--update` — update the hub to the latest release. The release's checksum is
  checked and the new AppImage is run with `--version` before anything is
  replaced; a release that cannot run on this machine is refused with nothing
  changed. `--update --from <AppImage>` installs a local rebuild of the same
  or a newer version through the same swap.
- `--rollback` — undo the last hub update: the previous hub binary and the
  registry it recorded, both back exactly as they were, offline, in under a
  second.

## It does not start

You downloaded the AppImage, made it executable, and running it does
nothing — or the terminal prints something about a version of `GLIBC`,
`GLIBCXX`, or `CXXABI` it cannot find. That is the dynamic loader failing
before a single line of the hub's own code runs: a system library is older
than the version required by the release.

Every release's floor is recorded beside it: download the matching
`TFSAppHub_<version>_amd64.versions.txt` from the same
[release](https://github.com/ArnaudDelgerie/TFSAppHub/releases) and
read its `glibc_floor` and `glibcxx_floor` lines. They are the lowest glibc
version and `GLIBCXX` symbol version the machine's `libstdc++.so.6` must
provide. Compare glibc with `ldd --version` (the number on its first line)
or `getconf GNU_LIBC_VERSION`. A machine below either floor cannot start that
AppImage. `CXXABI` symbols are provided by the same C++ runtime.

When the official release is built on a base newer than your machine can run
(starting with the move to Ubuntu 24.04 in April 2027), you can rebuild on
Jammy with Git and Docker installed — no local Rust or Tauri CLI is needed:

```sh
git clone https://github.com/ArnaudDelgerie/TFSAppHub.git
cd TFSAppHub/build
BASE_IMAGE=ubuntu:22.04 docker compose run --rm build
```

The explicit `BASE_IMAGE=ubuntu:22.04` keeps this rebuild on Jammy when the
official default moves to 24.04, whatever your distribution. Jammy is the
oldest base that provides `webkit2gtk-4.1`;
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

Install the rebuilt AppImage with
`tfsapp-hub --update --from <rebuilt AppImage>` (or run that command through
your current AppImage). This also
updates the copy used by desktop launchers and leaves an anchor for
`--rollback`; replacing the downloaded file by hand does neither.

If the hub starts but shows a blank or broken window after an update, run
`tfsapp-hub --rollback` from a terminal. To check whether GPU rendering is
involved, retry with `WEBKIT_DISABLE_DMABUF_RENDERER=1` in the environment.

If the hub starts but microphone capture fails, check the installed app's
`log/hub.log` for GStreamer element or plugin loading errors. The AppImage
carries the build host's GStreamer plugins, while some optional graphics and
audio libraries and the audio server come from your machine. A Docker build
on an older compatible base can avoid newer symbol requirements; it cannot
provide a missing host audio service or device.

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
authenticity; which source or repository to trust remains your call.

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
one.

### Declared user directories are a legible line, not a filesystem boundary

Declaring `actions.paths` (`downloads`, `pictures`, and the rest of GLib's
eight special directories) buys the app a `TFS_USER_<NAME>_DIR` variable it
would otherwise have to guess at — never a sandbox: PHP runs with the user's
full rights regardless, exactly like `save_path` and the microphone above. A
declared member GLib cannot resolve reports as an absent variable rather than
a guessed `$HOME`-based path, and `$HOME` itself is deliberately not a ninth
member — it already reaches PHP through the ordinary process environment.

## Make your first app

An app is a Symfony project. You need PHP and Composer on your machine —
nothing else: no Rust, no Tauri CLI, no GTK development libraries.

```sh
composer create-project symfony/skeleton myapp
cd myapp
composer require arnauddelgerie/tfs-app-bundle
bin/console tfsapp:init
```

`tfsapp:init` generates the app's `tfsapp.config.json` at the project root —
it prompts for four identity fields (`project_name`, `product_name`,
`identifier`, `app_version`), each with a sensible derived default, and Enter
accepts the default — plus a starter `CHANGELOG.md`. The bundle registers
`/healthz` on its own; no route to declare.

Run it under the hub, live:

```sh
tfsapp-hub dev .
```

A window opens on your app, served from this directory as it is. `dev` serves
live source **in place**: it watches nothing, compiles nothing and builds no
assets — your build tool already has a `--watch`; the hub serves and
restarts. Ctrl-C stops the session.

Then see it the way your users will, as an installed app:

```sh
git init && git add -A && git commit -m "First app"
cd ..
tfsapp-hub publish myapp --local out/
tfsapp-hub install out/<name>-<version>/<name>-<version>.tar.gz
tfsapp-hub list
tfsapp-hub open <id>
```

A release is pinned to a git commit — that is what the `git init` line is
for; `publish` prints the archive's path when it is written. After `install`
the app shows in `list`, opens with `open <id>`, and appears under its own
name and icon in your shell's grid.

Where to go from here:

- the [TFSAppBundle README](https://github.com/ArnaudDelgerie/TFSAppBundle) —
  the PHP-side reference: what the bundle you just required does, its
  configuration, its commands.
- the contract's [what an app must provide](contract/1-what-an-app-must-provide.md)
  — the rest of the three files and one route, and every optional field of
  the manifest.

## Where to read next

- [**TFSAppBundle**](https://github.com/ArnaudDelgerie/TFSAppBundle) — the
  Symfony bundle an app uses; its README is the PHP-side reference.
- [**`CONTRACT.md`**](CONTRACT.md) — what the hub and an app promise each
  other. Written for app authors: read it alone and you know exactly what to
  build.
- [**`ARCHITECTURE.md`**](ARCHITECTURE.md) — how the hub itself works: the
  crate boundary, runtime identity, the install pipeline, the launch
  sequence, the bundled interpreter. Written for contributors.
- [**`CONTRIBUTING.md`**](CONTRIBUTING.md) — building the hub, its checks,
  its release process.
- [**`CHANGELOG.md`**](CHANGELOG.md) — what changed, release by release.
