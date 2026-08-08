# TFSAppHub

One AppImage that installs, updates and runs **several** Symfony desktop apps
from their source, using its own bundled FrankenPHP as the PHP interpreter.

Since **2026-08-08 it is the only host**. `TFSAppWorkstation`, which used to build
one standalone AppImage per app and to host the dev loop, is archived; the dev loop
moves here (`tfsapp-hub dev <path>`, not built yet). The point of that move: writing
a TFSApp will need no Rust toolchain, no Tauri CLI and no GTK dev libraries — just
the hub and a Symfony project.

A dev or a small IT team installs one binary. From it they add the Symfony apps
they care about, by path or by git repository:

```sh
tfsapp-hub install path/to/project
tfsapp-hub open myapp
```

The fragile native half (FrankenPHP, WebKitGTK, the glibc floor) is built once;
everything added afterwards is pure PHP and has no ABI surface. N binaries
collapse to 1, `composer install` *is* the compatibility manifest — it runs with
the very interpreter that will later serve the app — and per-app update falls out
of `git fetch` for free.

Status: **it installs, and it opens.** `install` snapshots a local project,
resolves its dependencies with the bundled PHP, runs its lifecycle commands and
registers it; `open` gives that app a real window on its own FrankenPHP, with
its own data directory and its own cookie store, so two apps open side by side
stay isolated; `list` and `remove` close the loop. Not there yet:
`run <id> <alias>`, desktop entries, and update/rollback. Git sources are
recognised and refused with a message saying so, rather than pretended. The
design record and the plan queue live under the git-ignored `.project/`.

## Build

```sh
make resources # fetch the pinned FrankenPHP and composer.phar into hub/resources (once)
make check     # cargo fmt --check, clippy -D warnings, unit tests, shellcheck
```

`make resources` is not only a packaging step: the hub installs *and* runs every
app with that interpreter and that Composer, so no system-wide PHP is required —
and nothing works without them. (`make sidecar` and `make composer` fetch one
half each, if you want them separately.)

### Prerequisites

- **Rust** + the Tauri CLI (`cargo install tauri-cli`, or `cargo tauri` v2).
- Linux build deps for Tauri v2 / WebKitGTK (`libwebkit2gtk-4.1-dev`,
  `libgtk-3-dev`, `libdbus-1-dev`, `build-essential`, `curl`, …).
- **curl** (to fetch the FrankenPHP sidecar and composer.phar).
- **rustfmt** and **clippy** (`rustup component add rustfmt clippy`) and
  **shellcheck** — needed for `make check`. A missing Rust component fails with
  rustup's own `rustup component add …` message.
- **`gnome-keyring`** (the `gnome-keyring-daemon` binary) and **`dbus`**
  (`dbus-run-session`) — **test-only**, needed for `make check`. They stand up
  an *ephemeral*, throwaway Secret Service the test suite runs against, so it
  never touches your real login keyring. Neither is the GNOME desktop, and
  neither is a runtime dependency of the packaged hub.

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
loop. The dev loop is its own mode (`dev <path>`, not built yet), and it serves
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
tag can be read and diffed; a 144 MB binary cannot). What genuinely differs is
friction and co-residency, not the kind of risk.

No sandbox is claimed. Sources default to a pinned tag or commit, never a branch.
For a developer audience this is exactly `composer require`, and pretending
otherwise would be worse than saying it.

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
