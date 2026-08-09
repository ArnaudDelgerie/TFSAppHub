# Architecture

How TFSAppHub is built, and why it is built that way. The companion document is
[`CONTRACT.md`](CONTRACT.md), which states what the hub and an app promise each
other; nothing here is a promise to an app, and anything an app could depend on
belongs there instead.

---

## The problem

A Symfony application can be a perfectly good desktop application. Making it one
means shipping a PHP interpreter, a web server and a browser engine alongside it,
and that is where the cost lives: those three are native, they are large, and
they are compiled against the machine that built them.

The obvious answer — package each app as its own self-contained binary — makes
that cost recur. Every app pays a build pipeline, every app author needs a Rust
toolchain and GTK development headers, and every release re-answers the question
of which glibc and which WebKitGTK it links against, on whichever machine
happened to run the build.

The hub inverts it. **The fragile native half is built once. Everything added
afterwards is PHP.**

## The model

One binary installs, updates and runs N Symfony applications from their source,
using its own bundled FrankenPHP as the interpreter.

Three properties follow, and they are the whole argument:

**`composer install` *is* the compatibility manifest.** It runs at install time
with the very interpreter that will later serve the app. There is no linking, no
ABI surface, and no baseline to get wrong — the question "will this app run on
this machine?" is answered by Composer's own platform requirements against a PHP
the hub controls.

**Per-app update is `git fetch`.** An app is source at a pinned ref. Updating it
is re-resolving that ref and replaying the lifecycle, not rebuilding and
redistributing a 150 MB binary.

**N binaries collapse to 1.** The disk win comes from sharing the *binary*, never
from sharing the process — see "One process per open app" below.

The cost is honest and worth stating: the hub is a single point of failure, in a
way N independent binaries were not. A hub that will not start takes every
installed app with it. That is accepted, and its mitigation is deliberately
deferred rather than pretended away.

## The two crates

```
core/   identity-agnostic: process supervision, sidecar spawning, port
        allocation, health polling, log rotation, APP_SECRET, external links
hub/    everything that knows which app it is serving
```

**`core/` never depends on `tauri`.** The rule is enforced by the dependency
graph rather than by convention, and it is what keeps the supervision half
testable in CI on a machine with no display, no GTK and no session bus.

The boundary is not "reusable versus not". It is "does this need to know which
app it is?" — a process group, a port, a health poll and a log file do not. A
window, a keyring namespace, a data directory and a manifest do.

Three modules currently sit in `hub/` that belong below the line — `secrets`,
`bridge` and `worker` — because they read hub-side configuration types and
Tauri state. They move down when those inputs become parameters, and when a
second consumer exists to justify it. Not for tidiness.

## Where things live

```
<OS data dir>/TFSApp/hub/apps/<id>/          the installed snapshot — what actually runs
<OS data dir>/TFSApp/hub/registry.json       what is installed, from where, at what version
<OS data dir>/TFSApp/hub/bin/php             the interpreter shim (see below)
<OS data dir>/TFSApp/hub/bin/tfsapp-hub      the stable hub copy .desktop entries point at
<OS data dir>/TFSApp/<identifier>/           the app's own data — CONTRACT.md §5
<OS data dir>/applications/<identifier>.desktop  the generated entry — XDG's own directory,
                                                  a sibling of TFSApp/, not a child of it
```

The last line is load-bearing and the reason the first three are siblings of it
rather than parents. An app's data directory derives from its `identifier`
alone: not from the hub, not from the handle it was given, not from where any
binary lives. **The hub never invents its own layout for app data.** Its root
holds installed source and its own registry, and nothing else.

That gives two distinct keys per app, kept apart on purpose:

| key | what it names | where it comes from |
| --- | --- | --- |
| `id` | a directory under the hub's root, and the handle you type | assigned at install, recorded in the registry |
| `identifier` | the data dir, the keyring namespace, the window identity | the app's own manifest |

## Runtime identity: one binary, N applications

The hard problem, and the one that gated the whole design.

Everything an operating system keys per application — the GTK application id,
the D-Bus name, the single-instance key, `WM_CLASS`, and the WebKitGTK
website-data directory holding the app's cookies — normally comes from a value
baked into the binary at build time. The hub has one binary and N apps, so all
of it has to follow a value resolved from argv instead.

It does, and almost for free: Tauri reads the app id from the **runtime** config
when the builder starts GTK, so mutating the context's identifier beforehand
moves all of them at once. This was measured before it was relied on — two
identities of one binary launched side by side on Wayland and X11 produced two
owned bus names, two window classes and two separate `~/.local/share/<identifier>/`
trees, with no cookie crossing in either direction despite both windows sharing
the `127.0.0.1` cookie origin.

The constraint that falls out: **all of it must happen before the builder runs,
and before anything else touches GTK.** That is why the config mutation and
`set_prgname` live in the same entry point — they share one deadline, and
splitting them is how one of them eventually gets forgotten.

## Installing

`install <source>` snapshots a project into the hub's root and makes it ready to
open. In order:

1. Resolve the source to a directory. A local path is used as it stands; a git
   source is cloned at a pinned ref (not yet built).
2. Read and validate the manifest. Missing required fields, a wrong type on a
   known key, or a non-semver `app_version` all stop here, naming the file and
   the field. An unknown key warns and does not stop anything.
3. Copy the tree to `apps/<id>/`, minus the app's own runtime droppings.
4. Assemble the app's environment (`CONTRACT.md` §3) and create its data
   directory. The path is printed before anything writes into it, because the
   next thing on screen is a migration writing a database.
5. `composer install` with the bundled interpreter.
6. Run `pre-install` then `post-install`, in order, stopping at the first
   failure.
7. Record the app in the registry, with the platform fingerprint of the hub that
   installed it.
8. Write the version record into the data directory — **only** once every step
   above has succeeded.
9. Refresh the stable copy of the hub itself, and write the app's `.desktop`
   entry — best-effort, and skipped together by `--no-desktop-entry`.

Step 8 is the one worth defending. The record is what decides whether a later
moment is an install, an update, or a downgrade to refuse, and writing it
speculatively would make a failed install look like a completed one. A failure
leaves the directory undated, so the next attempt replays the whole event rather
than resuming into the middle of it.

Step 9 runs **last**, after the app is registered and ready, for the same
reason registration itself runs last: an entry advertising an app whose
`composer install` failed would sit in the user's application grid pointing at
nothing. The app is usable from the CLI before it is advertised anywhere else.
A failure here — the copy or the entry — is a warning naming the path and the
cause, never a reason to undo an install that has already succeeded.

**No sidecar runs during an install.** Lifecycle commands get the full
environment and a real database, on a terminal where a failure is legible — but
they cannot reach their own app over HTTP, which is exactly what `CONTRACT.md`
§6 tells app authors not to attempt.

### The stable path a `.desktop` entry points at

`Exec=` in a generated entry never names the AppImage the user downloaded —
`~/Downloads/tfsapp-hub-0.3.0.AppImage` is a path a launcher cannot depend on,
since tidying that folder would silently break every app's launcher at once.
Every entry instead points at `<OS data dir>/TFSApp/hub/bin/tfsapp-hub`, a copy
of the hub kept current beside the `php` shim, in the `bin/` directory the
"Where things live" table above already gives the hub's own executables.

The image copied is `$APPIMAGE` when the process is running as one, and
`current_exe()` otherwise. That split exists because `current_exe()` resolves
to a FUSE mount (`/tmp/.mount_XXXX/usr/bin/tfsapp-hub`) inside an AppImage — a
path that disappears the moment the process exits, so a launcher built from it
would point at nothing the next time it was clicked. `$APPIMAGE` is what the
AppImage runtime exports for exactly this case.

The replacement is a **rename**, not an in-place write: a temp file written
beside the target, then renamed over it. Two apps sharing this one file is the
ordinary case, not an edge case — one may be open from it while a second
install refreshes it for a newer hub — and a rename swaps the whole inode
atomically, so a process already running the old copy keeps running the
generation it started with instead of meeting a half-written 170 MB binary or
an `ETXTBSY`. It is also what hub self-update (queued, not yet built) will
replace this same file with, which is the other reason it lives under the
hub's own root rather than beside a per-app path.

## Opening an app

**One OS process per open app.** `open <id>` resolves the app, then re-executes
the hub binary as a child carrying that app's identity. The child mutates its
runtime identity, boots the app's sidecar and opens the window; the parent
resolves, fails fast with a message a terminal user can act on, and gets out of
the way without holding the shell.

This was decided, not defaulted. A single process serving N apps, or a single
shared FrankenPHP, were both considered and rejected:

- every isolation guarantee in `CONTRACT.md` §5 holds verbatim in this shape,
  because it is one process, one identifier, one data dir, one cookie store;
- a segfault in one app cannot take the whole suite down with it;
- the supervision, sidecar, worker and window code works per process essentially
  unmodified.

Each app pays its own FrankenPHP startup. That is the cost, and it is accepted.

### The launch sequence, and why the order is what it is

1. The lifecycle guards run **before the builder exists**: the version decision
   against the data record, the static-port conflict check, and the serving and
   liveness locks — hand off at once to a live sibling, wait out a dying one, or
   take both locks and reap a crashed one's pid file. Not a style choice — these
   refusals show a blocking native dialog, and once Tauri has claimed GTK a raw
   dialog deadlocks rather than appears.
2. The **splash window is created first**, before the sidecar is spawned and long
   before `/healthz` answers.
3. The Caddyfile is written into the app's data directory, then
   `messenger:setup-transports` runs, then the server, then the worker.
4. `/healthz` is polled every 250 ms for up to 60 seconds. If the server process
   dies before answering, the wait aborts immediately instead of burning the
   timeout.
5. The **same window** is navigated to the backend.

Step 2 and step 5 being one window is the point. Opening the app window only
once the backend was healthy would leave the screen empty for the length of a
cold start — tens of seconds of a user wondering whether their double-click
registered.

Step 3's order is load-bearing too: `messenger:setup-transports` runs before
anything is spawned, so an app that declares a worker without the Doctrine
Messenger bridge fails with no sidecar to tear down.

### The Caddyfile is written per app, per launch

The hub carries it in its own binary and writes it into the app's data
directory on the way up, rewritten every launch rather than created once. A hub
self-update can change what the file should say, and a stale one on disk would
be indistinguishable from a current one.

Nothing in it is per-app: document root and port come from the injected
environment, which is exactly what lets one file serve every installed app.

### Process supervision and teardown

Two non-blocking exclusive flocks, both held by the launcher for its whole
lifetime, answer two different questions.

The **liveness lock** answers "does this process still own this data dir".
Released only by the operating system, whenever this process ends — never by
any code in it — because that is the sole release that is genuinely
simultaneous with the process actually being gone. Acquiring it means any
previous launcher is confirmed dead, so a pid found in the pid file can be
reaped.

The **serving lock** answers "will handing this launch's argv to that process
get you a window right now". Released explicitly, at the very top of
teardown, alongside `tauri-plugin-single-instance`'s own bus name — both
before a single child process is signalled — so a sibling that has just begun
shutting down stops claiming to serve within milliseconds of the signal or
window-close that started it, long before the SIGTERM-then-SIGKILL escalation
below actually finishes.

A launch probes both, in that order. A live sibling holding the serving lock
is what makes a second `open` of the same app surface the first instead of
starting a second server — `CONTRACT.md` §5's per-identifier guarantee,
mechanically. A sibling holding only the liveness lock — its serving claim
already released, still mid-teardown — is not a live sibling to hand off to:
the arriving launch waits, bounded, for the liveness lock to free, then starts
its own. Without that second lock a closed window and a gone process were the
same signal, and a launch arriving in the gap between them attached to a
backend already dying instead of starting a fresh one.

The worker's pid is the second line of that pid file, which is what lets the
next launch reap it if this process never gets the chance. Teardown stops the
worker before the server. Children are killed as a process group, so nothing
survives a window closing.

### Worker supervision

An app that declares off-window work gets a consumer on its queue, recycled on
its own time and memory limits, respawned with an exponential backoff, and given
up on after five consecutive failed starts with the user told once.

Those constants are the app's guarantee rather than the host's, which is why
they are not tuned here: a host that supervised differently would make the same
manifest mean two different things.

The worker belongs to the **sidecar's** lifetime, not the window's. A second
`open` of the same app cannot spawn a second consumer, because it never gets
past the liveness lock.

### Navigation policy

Every window enforces the same classification:

| target | outcome |
| --- | --- |
| the backend's own origin, or the bundled-asset origin | let through |
| another `http`/`https` origin | cancelled here, opened in the user's own browser |
| `javascript:`, `file:`, `data:`, `blob:`, a custom scheme | cancelled, and said so |

No in-app popup is ever created. An external link belongs in the browser where
the user has their bookmarks, their sessions and an address bar.

## The bundled interpreter

The hub ships one FrankenPHP and one `composer.phar`, fetched at build time by
`make resources`. That is not only a packaging step: the hub installs *and* runs
every app with them, so no system-wide PHP is required — and nothing works
without them.

**Resolved in packaged/dev/system order.** `platform::bundled_frankenphp` and
`php::bundled_composer` each return an ordered list of candidates: the
packaged AppImage's own resource dir first — read via
`tauri::utils::platform::resource_dir`, which needs no `AppHandle` and so
works from `install`, a headless CLI command that runs before any
`tauri::Builder` exists — then `<hub>/resources/`, where `make sidecar` and
`make composer` download them for a build run from source.
`core::sidecar::resolve_frankenphp_binary` walks that list and falls back to
a system-wide `/usr/bin/frankenphp` if neither resolves (`composer.phar` has
no such fallback — the hub never runs an app's Composer with anything but its
own). A candidate only counts when it is a real, non-empty file — `hub/build.rs`
writes 0-byte stubs at both paths so a fresh clone still compiles before
`make resources` has run, and a stub must never be mistaken for the download.

### The `PHP_BINARY` shim

`frankenphp php-cli` reports **no path for itself**: `PHP_BINARY` is empty.

That constant is the first thing Symfony's `PhpExecutableFinder` consults, and
it is how every PHP tool re-invokes "the interpreter running me" — Composer's
`@php`, Flex's auto-scripts, anything shelling out through `symfony/process`.
Empty, the finder walks on and picks up whatever `php` the *machine* has.
Measured, under the bundled 8.5.8:

```
> @php -r "echo PHP_VERSION, ' at ', PHP_BINARY;"
script ran under PHP 8.4.23 at /usr/bin/php8.4
```

No warning, no error — an app resolved against one interpreter quietly running a
piece of itself on another. On a machine with no PHP at all, the same call
simply fails, and that machine is the normal case for a desktop user.

A second, smaller problem came with it: `frankenphp php-cli` accepts a script or
`-r` and **no PHP CLI options**. `-d`, `-n`, `-v`, `-m` are each read as a
filename — which matters because Composer appends three `-d ini=value` options to
every `@php` it spawns.

Both are fixed by a `bin/php` shim under the hub's root that drops those options
and re-enters `frankenphp php-cli`, exported as `PHP_BINARY` and prepended to
`PATH` for every command the hub starts. It is rewritten on every use, for the
same reason the Caddyfile is.

## The registry, and what a hub update does to installed apps

`registry.json` holds one entry per installed app plus a little about the hub
that last wrote it. Three properties it owes its callers:

- **A missing file is an empty registry, not an error.** "Nothing installed" is
  the overwhelmingly common state on a fresh machine.
- **Writes are atomic and serialised** — temp file in the same directory,
  `fsync`, rename, under an exclusive lock. A reader always sees one complete
  generation, which is why reading takes no lock.
- **Unknown fields survive a read-modify-write**, at both the top level and per
  entry. The same additive-growth rule the manifest's unknown-key warning
  serves, applied to the hub's own state: a user who tries a newer hub and steps
  back must not lose what it recorded.

There is deliberately no format-version field; the hub version already recorded
answers the same question without inviting a migration framework.

### The platform fingerprint

A hub self-update replaces PHP *underneath apps that are already installed*,
whose `composer.lock` was resolved against the old one. The one-app-per-binary
route never had this problem: one binary, one PHP, one app, updated together or
not at all.

So each app records the fingerprint it was installed against — PHP's
`major.minor`, because that is the granularity a lock's platform requirements are
written at, plus the loaded-extension list, because a missing extension breaks a
lock just as surely as a version bump. Both come from one `php-cli` call at
runtime; nothing is baked.

After a self-update, apps whose fingerprint differs are marked for revalidation:
`composer install` against the **existing lock**, lazily, on that app's next use.
Never `composer update` — re-resolving a dependency tree during what the user
experiences as a *hub* update is the worst possible moment for a surprise.

**It is not a security check and not a version pin.** It authenticates nothing
and blocks no launch on its own. An app whose fingerprint differs is
revalidated, never refused.

## The CLI grammar

> **`--flags` act on the hub. Bare words act on an app.**

One sentence, no ambiguous case. Two rules keep the dispatcher small:

- **It parses and routes; it never works.** Argv becomes a command and nothing
  else — no filesystem, no registry, no process — which is what makes every form
  in the grammar testable on a machine with nothing installed.
- **The whole surface is declared from the start**, including the parts no plan
  has implemented. A recognised-but-unavailable command answers "not implemented
  yet", never "unknown command" — a wrong message there sends someone hunting
  for a typo that is not present.

One carve-out is worth naming because it is easy to get wrong: after a `run`
alias, **every word belongs to the app's own command**. `run demo console --help`
asks the app's console for its help, not the hub for its own. The property, not
the flag, is what the test pins — forwarded arguments are the app's — because
every other host-level flag sits in the same trap the day an app declares an
alias that takes one.

## Three measurements that shaped the code

Recorded because each one explains why a piece of code looks the way it does,
and each was silent enough that it would otherwise be "simplified" away.

**A 512×512 window icon never reaches the window property.** GDK writes it as
`2 + width × height` words and refuses past its selection size limit, which caps
at 262144; a 512×512 RGBA icon needs 262146 and misses by two words. It does not
warn and does not error — the property is simply never set and the window comes
up with a generic icon. Hence the downscale-and-say-so in `identity.rs`. The
full-size file stays the one every other surface uses.

**`--help` and `--version` were read past the point where argv stops being the
host's.** See the carve-out above; the fix is that `run`'s tail is claimed before
the host's own flag parser sees it.

**`PHP_BINARY` is empty under `frankenphp php-cli`.** See the shim above.

All three were found here rather than reasoned about, and the reason is
structural: the hub is the first thing to run this code path **twice in one
process image, from argv rather than from a build**, which makes it the first
place a silent default is visible as a difference.

## Packaging

The hub is packaged once, as an AppImage, and that build decides the
compatibility floor for the entire fleet — it is the only binary anyone links.
An AppImage's portability comes down to two numbers, the glibc it was linked
against and the WebKitGTK ABI it expects, and both come from the builder's
machine. Rather than fighting that with a pinned build base of its own, the
floor is simply **a property of whatever built the image**, measured and
recorded beside the artifact in `.versions.txt` — the highest `GLIBC_x.y`
symbol version any bundled ELF imports, the build host's own glibc, the OS,
and the frozen version of every WebKit/GTK/GLib library the bundle carries.
No single release covers every distribution a user might be on; the answer
offered to whoever it fails is `build/compose.yaml`'s `docker compose run --rm
build`, with `BASE_IMAGE` set to a base older than the one that produced the
release (see README.md's "It does not start"). The container is a wrapper
around exactly `make build` / `make check` and nothing else knows it exists —
no branch anywhere in `build/scripts/` or in Rust asks whether it is running
inside one.

**`cargo tauri build` gets three things wrong, repaired after the fact by
`build/scripts/fix-appimage-bundle.sh`.** linuxdeploy runs `patchelf` (rpath →
`$ORIGIN`) on every ELF file it bundles, which corrupts the FrankenPHP
sidecar — a static-PIE Go binary whose rewritten program headers SIGSEGV on
every exec (a known upstream patchelf limitation) — so the pristine binary is
copied back in after the fact. Its GTK plugin writes an `apprun-hooks` script
that hard-forces `GDK_BACKEND=x11`, pinning the whole app to XWayland on a
Wayland session; the hook is rewritten to defer to the session's own backend.
And `.DirIcon` is written as an absolute symlink into the *build machine's*
own AppDir path, broken the moment the image is mounted anywhere else; it is
replaced with a relative symlink to the icon named by the AppImage's own
`.desktop` `Icon=` line. The pass also deletes every bundled `libwayland-*`:
unlike the WebKit/GTK stack, the Wayland *client* library is an ABI the
running session owns, and freezing a copy older than the host's own
Mesa/compositor is a known way to fail on a newer distribution — so the
dynamic linker is left to fall through to the session's own copy instead.
Every repair is verified against the repacked artifact, not assumed from the
input to the repack.

## Source layout

```
core/src/          identity-agnostic supervision (see "The two crates")
hub/src/           the hub proper — one module per concern, tests beside each
hub/Caddyfile.desktop   written into each app's data dir at launch
hub/resources/     the fetched FrankenPHP and composer.phar
```

Every module has its tests in a sibling `*_tests.rs`, and every module carries a
header explaining what it is for and which decision it implements. Those headers
are the primary documentation of the code; this file is the map above them.

## Open questions

The queued design work lives in `.project/plan/000-index.md` rather than here, so
that one list stays authoritative. The structural decisions that outrank
everything else are in `.project/decisions/`.

The one worth naming here, because it shapes what is above rather than extending
it: the hub is a single point of failure for every installed app, and nothing in
this architecture mitigates that yet.
