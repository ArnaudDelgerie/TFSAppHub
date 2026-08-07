# TFSAppHub

One AppImage that installs, updates and runs **several** Symfony desktop apps
from their source, using its own bundled FrankenPHP as the PHP interpreter.

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

Status: **it installs, it does not open yet.** `install` snapshots a local
project, resolves its dependencies with the bundled PHP, runs its lifecycle
commands and registers it; `list` and `remove` complete the loop. `open` — a
real window on a running app — is the next plan, so the second command above is
still what this is heading towards. Git sources are recognised and refused with
a message saying so, rather than pretended. The design record and the plan queue
live under the git-ignored `.project/`.

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

## Relationship to TFSAppWorkstation

[TFSAppWorkstation](https://github.com/ArnaudDelgerie/TFSAppWorkstation) (the
*station*) is not replaced by this repo. Three roles, none of which replaces
another:

| | |
|---|---|
| station, `make tauri-dev` | the development loop, against a live source tree |
| station, `make build` | one self-contained AppImage, for someone who has nothing |
| **hub** | run that same app from installed source, alongside others |

The station owns live source, the hub owns installed source. Install here is a
**snapshot**: editing the original source has no effect until an explicit
`update`, which is what makes `composer install`, migrations and a warm
persistent cache meaningful — and exactly what makes it useless as a dev loop.
Any pressure to add a "linked" or "watch" mode belongs to `make tauri-dev`.

Both hosts share one app contract (the station's `CONTRACT.md` §1–§5) and the
same Symfony bundle,
[TFSAppBundle](https://github.com/ArnaudDelgerie/TFSAppBundle), unchanged. An app
cannot tell which host it is running under. Because `identifier` comes from the
app's own `tfsapp.config.json` in both routes, a hub-installed app and a
station-built AppImage of the same project resolve to the **same data
directory** — data follows between routes, with no import step.

This is a fresh start, not a fork of the station: the reusable Rust modules get
copied over and adapted (see `.project/plan/`). The modules that diverge —
identity resolution above all — could not serve both identity models (baked at
build vs resolved at runtime) in one tree.

## Trust posture

Installing an app runs a third party's PHP, Composer scripts included, with the
user's full rights. So does downloading an unsigned AppImage — the trust decision
is the same one, and the hub is the more auditable of the two (a repo at a pinned
tag can be read and diffed; a 144 MB binary cannot). What genuinely differs is
friction and co-residency, not the kind of risk.

No sandbox is claimed. Sources default to a pinned tag or commit, never a branch.
For a developer audience this is exactly `composer require`, and pretending
otherwise would be worse than saying it.
