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
<OS data dir>/TFSApp/hub/scratch/<pid>/      a release download/verify/extract, removed on exit
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

1. Resolve the source to a directory. A local path is used as it stands; a
   release source (`github:owner/repo`) is downloaded, verified and extracted
   into scratch space — see "Resolving a release" below.
2. Read and validate the manifest. Missing required fields, a wrong type on a
   known key, or a non-semver `app_version` all stop here, naming the file and
   the field. An unknown key warns and does not stop anything.
3. Four gates, all against the already-loaded registry or the data directory
   `identifier` names — none of them write anything, so a refusal here costs
   nothing:
   - settle the hub-local `id` and refuse a collision (`check_id_free`);
   - refuse a second `id` for an `identifier` another entry already carries
     (`check_identifier_free`) — the registry is keyed on `id`, the data on
     `identifier`, and this is what keeps that mapping one-to-one;
   - refuse a port another installed app already pinned (`check_port_free`);
   - refuse a data directory a live window or an active `run` command still
     owns (`check_data_dir_available`) — `install` is that directory's third
     writer, reading the same two locks `open`'s launch guard and `run`'s own
     rule 3 already do.
4. Decide which lifecycle event (`CONTRACT.md` §6) this install may run,
   against the version the data directory's own record holds — `None` means a
   first install, and only `record == app_version` survives past this point;
   a record naming a newer version is refused as a downgrade, and one naming
   an older version is refused as the update event, which `install` does not
   run (see below). The confirmation prompt names the record's version when
   one exists, so it is a question the user can actually answer.
5. Copy the tree to `apps/<id>/`, minus the app's own runtime droppings.
6. Assemble the app's environment (`CONTRACT.md` §3) and create its data
   directory. The path is printed before anything writes into it, because the
   next thing on screen is a migration writing a database.
7. `composer install` with the bundled interpreter — every event, since
   dependencies are the freshly copied tree's own and not a lifecycle command.
8. Run `pre-install` then `post-install`, in order, stopping at the first
   failure — **only** when step 4 decided this is a first install. A record
   equal to the app's own version (the reinstall-after-`remove` path) runs
   neither.
9. Record the app in the registry, with the platform fingerprint of the hub that
   installed it.
10. Write the version record into the data directory — **only** once every
    step above has succeeded, and every event, including an equal record
    rewriting itself.
11. Refresh the stable copy of the hub itself, and write the app's `.desktop`
    entry — best-effort, and skipped together by `--no-desktop-entry`.

Step 10 is the one worth defending. The record is what decides whether a later
moment is an install, an update, or a downgrade to refuse, and writing it
speculatively would make a failed install look like a completed one. A failure
leaves the directory undated, so the next attempt replays the whole event rather
than resuming into the middle of it.

**Why step 4 refuses the update event rather than running it.** A record
older than the app being installed is, by `CONTRACT.md` §6's own definition,
the update event — and it would be tempting to have `install` run
`pre-update`/`post-update` there instead of refusing. It must not, for one
reason: §6 hands that event a guarantee `install` has no snapshot to back —
an update must never leave the app's database between two versions. `update
<id>` is the command that owns it (see "Updating", below), and `install`
refuses instead, naming it.

Step 11 runs **last**, after the app is registered and ready, for the same
reason registration itself runs last: an entry advertising an app whose
`composer install` failed would sit in the user's application grid pointing at
nothing. The app is usable from the CLI before it is advertised anywhere else.
A failure here — the copy or the entry — is a warning naming the path and the
cause, never a reason to undo an install that has already succeeded.

### Resolving a release

A release source never touches `apps/` directly. `source::resolve` fetches the
release (latest, or the tag `--ref` names) through the forge's release API,
then works entirely inside a scratch directory under the hub's own root —
sized for a multi-hundred-megabyte archive landing on the same filesystem
`apps/<id>/` is about to receive it, and removed by the caller (`install` or
`update`) once it is done, on success or failure alike. The order is the
guarantee:

1. **Download** the release's `<project_name>-<app_version>.tar.gz` asset,
   streamed to a file — never buffered whole in memory.
2. **Verify** it against the `SHA256SUMS.txt` asset beside it, hashed the same
   way. A missing line for the archive's own name is a failure, never a pass
   by absence.
3. **Extract**, only once the checksum has matched — every entry checked
   before it is written (no absolute path, no `..` component, no symlink or
   hard link resolving outside the extraction root), and the whole archive
   rejected unless it holds exactly one top-level directory.
4. **Walk** the extracted tree (`tree_hash`) and read its manifest, which
   step 2 above validates exactly as it would a local path's.

Nothing about `apps/<id>/` is touched until all four have succeeded — a
refusal at any point leaves scratch removed by the caller and the app root
exactly as it was.

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
an `ETXTBSY`. It is also what `--update` (`hub_update.rs`) replaces this same
file with, which is the other reason it lives under the hub's own root
rather than beside a per-app path.

## Updating

`update <id> [--ref <tag>] [--force]` re-resolves an app's already recorded
source and, when it is newer, replaces the installed tree with it —
the update event (`CONTRACT.md` §6), and the guarantee `install`'s own
refusal defers: an update must never leave the app's database between two
versions.

1. Load the registry entry, re-resolve its source (the recorded selector, or
   `--ref`'s), and validate it exactly as `install` does.
2. Decide the action (`update_decision`) from the same `lifecycle_decision`
   `install`/`open`/`run` all read, applied to the registry's recorded
   `app_version` against the freshly resolved source's own:
   - a newer source → `Apply`, the update event;
   - an equal source → refused, naming `--force`;
   - an equal source with `--force` → `ResyncOnly` — re-copy the code and
     re-run `composer install`, no lifecycle command, no database snapshot,
     the existing anchor (if any) left exactly alone;
   - an older source → refused as a downgrade; `--force` does not unlock it,
     since it is for a tree edited without bumping the version, not for
     going backwards;
   - no record at all → refused; that moment belongs to `install`.
3. Guard the data directory exactly as `install` does (016's
   `check_data_dir_available`), then confirm.
4. `Apply`, in order: snapshot `app.db` (+ `-wal`/`-shm`), retain the
   outgoing tree at `apps/<id>.previous` (a rename, never a copy — the cost
   of holding an anchor is one generation of the tree, not a copy pass over
   it), copy the new tree in, empty the app's own cache/build directories,
   run `install::prepare` under the update event (`pre-update` then
   `post-update`), then — only once every step above has succeeded — write
   the rollback anchor's third half from the *outgoing* registry entry,
   stamp the registry with the new one, and rewrite the desktop entry.
5. Any failure from the tree swap onward reverts the whole attempt: the
   database snapshot is restored, the copied-in tree is removed, the
   outgoing tree is renamed back — and the registry is never touched, so the
   next `open` does not know an update was attempted at all.

**The rollback anchor is three halves, or none.** `rollback <id>` refuses
unless all three are present, naming whichever is missing:

| half | where |
| --- | --- |
| the retained source tree | `apps/<id>.previous` |
| the pre-update database snapshot | `<data>/data/app.db.pre-update` (+ `-wal`/`-shm` twins) |
| the outgoing registry state | `<data>/data/rollback.json` (`app_version`, `source_revision`, `created_at`) |

The third half exists because the first two cannot answer what it does:
`source_revision` is hashed over the *source*, while the retained tree was
copied with `install`'s own excluded paths — recomputing it from the tree
would produce a different string that means nothing.

A rollback rescue-dumps the current database first, printing its path, then
restores the snapshot as the live database, deletes the current tree and
renames `.previous` back in its place, restores `data/config.json` and the
registry entry to the anchor's recorded version, and rewrites the desktop
entry. It then **consumes** the anchor — the snapshot and `rollback.json`
are discarded, and there is no new `.previous` to roll forward into. A
rollback is one step back; a `.previous` naming the version just left would
invite a "rollback forward" this plan does not define.

Anchor hygiene runs both directions, so a stale one never outlives the
install it belonged to: `remove <id>` takes `apps/<id>.previous` with it
alongside `apps/<id>` itself, and a fresh `install` discards any
snapshot/`rollback.json` it finds in the data directory it is about to write
into — a data directory a `remove` without `--purge` left behind can
otherwise carry a stale anchor into an unrelated later install, one that has
nothing of its own to roll back to.

## Exporting and importing

`export <id> <path>` writes a curated `.tar.gz`; `import <id> <path> [--force]
[--yes]` seeds an existing installation of the same app from one
(`portability.rs`, plan 022). Both share a guard and a shape with `install`
and `update` above rather than inventing their own.

**The busy guard is the same probe `install` already made private, now
shared.** `lifecycle::data_dir_holder` — `is_owner_live` on `sidecar.pid`,
then `lifecycle::probe_run_lock` — moved out of `install.rs` so `export` and
`import` read the same two locks `install::check_data_dir_available` does,
rather than each re-deriving "is anything using this data dir". A live window
or an active `run` command refuses both commands, naming what holds it; a
data directory that does not exist yet is never busy, there being nothing yet
to guard.

**The database is copied raw, under that guard, never through SQLite.**
`export` byte-copies `app.db` and whichever of `app.db-wal`/`app.db-shm`
exist — `lifecycle::DB_FILE_NAMES`, the same three files `snapshot_db` copies
for an update — straight to the archive. The guard is what makes this safe:
nothing has the files open, so there is no hot WAL to reconcile and no reader
to race. `VACUUM INTO` was not on the table either way — the hub links no
SQLite library — but even with one, it would buy consistency the guard
already provides for free.

**`--force` draws the same line `017` already drew for `update`.** It unlocks
exactly one refusal — a populated data directory — and none of the other
three: not a foreign `identifier` (nothing to override), not an archive newer
than the installed app (writing a future version into `data/config.json`
would leave the next launch on `LifecycleDecisionError::Downgrade`, which has
no recovery path), not a window or `run` command holding the app (nothing to
override, wait or stop it instead).

**A forced import rescue-dumps before it overwrites.** The database it is
about to replace is moved aside via `lifecycle::rescue_dump_path` — the same
mechanism `rollback` uses for the database it cannot keep — and the path is
printed before the confirmation prompt fires, so "wrong archive" costs a
rename back, not a re-export from wherever the original came from.

**A forced import also discards the rollback anchor, both halves.** The
anchor pairs `apps/<id>.previous` with the `.pre-update` snapshot; seeding
foreign data over the snapshot half would leave a `rollback` that restores
last version's code onto a database that never ran it, and `anchor_state` has
no way to notice the mismatch. So the import discards both
(`discard_rollback_anchor`, `discard_db_snapshot`, `update::discard_tree`)
before it extracts, announced in the overwrite list ahead of the
confirmation — losing the ability to roll back is the honest price of
replacing the data underneath it.

**Extraction reuses `archive.rs`'s safety checks under a different shape.**
`archive::extract` requires exactly one top-level directory, which does not
fit an archive holding `manifest.json` and `data/` side by side, so
`extract_prefix` is a sibling that keeps `check_safe_path`/`check_safe_link`
— the traversal and symlink-escape checks — and drops only the
single-top-level constraint. One point worth getting right on the page rather
than only in the diff: the symlink-escape depth is computed against the path
*relative to the prefix*, not the archive's full path — the full path carries
`data/`'s own extra depth, and checking against it would permit one level of
escape more than is actually safe relative to the destination directory.

**Import never touches the keyring.** The archive carries no secret, so the
destination keeps whatever `APP_SECRET` it already had or resolves one fresh
on first use, exactly as an ordinary first launch does. `import` prints the
consequence rather than leaving it to be discovered — every session in the
imported database is invalid on the new machine, and `actions.secrets`
values must be re-provisioned there — CONTRACT.md §5 states the guarantee
this follows from.

## Publishing a release

`publish path/to/project [--repo owner/repo]` is `git.rs`/`gh.rs`'s pair
(`publish.rs`), and the mirror image of "Resolving a release" above: that
path downloads and verifies an archive a release already carries; this one
builds the archive and hands it to the one call that makes a release exist.
Gates, then archive, then checksums, then one `gh` call — the same order
`CONTRACT.md`'s "Publishing a release" states as an artefact, run backwards.

1. **Gates 1–8, all local, all before anything is built.** The manifest
   loads and its `app_version` is canonical semver (1–2); `git.rs`'s
   `Git::ensure_pushed` proves the project sits in a work tree with nothing
   uncommitted or untracked, that its branch has a pushed, non-ahead
   upstream, and resolves the repository — `--repo` if given, otherwise the
   upstream remote, parsed as `owner/repo` (3–6); the `CHANGELOG.md` gate
   extracts the `## <version>` section verbatim as the release notes (7);
   and, only when `actions.secrets.ipc` is on, a confirmation with
   deliberately no `--yes` escape, since a release ships that setting to
   every future installer (8). This is the one command where the hub runs
   `git` at all — decision 003's narrow exception to decision 002's "never
   shells out to `git`", scoped to the author's own machine.
2. **Gates 9–11, `gh.rs`'s `Gh`.** `gh` is installed (9) and authenticated
   (10); no release already carries the tag `v<app_version>`, draft included
   (11) — each its own refusal, checked before the archive is ever built.
3. **The archive**, only once every gate above has passed: the project's
   tracked source tree, the same exclusion rules `tree_hash` already applies
   on the install side, packed into `<project_name>-<app_version>.tar.gz`
   inside a scratch directory under the hub's own root — removed on success
   or failure alike, exactly as install's own scratch is.
4. **`SHA256SUMS.txt`**, hashed over the archive just built, beside it in the
   same scratch directory.
5. **The announcement**, then one confirmation (this one does honour
   `--yes`) — the repository, the commit and branch gate 3–6 proved, the tag,
   the archive and its size and hash, and the notes gate 7 extracted.
6. **One `gh release create --target <sha>` call**, the sha gate 3–6 already
   proved is on the forge — so the tag lands on the exact commit the archive
   was built from, never on a separately-pushed tag that could name a
   different one. A failed asset upload is inspected (`gh release view`) and,
   only when the release is missing an asset, deleted, so a retry is a clean
   retry rather than the version guard refusing the author's own wreckage.

**Two boundaries worth stating once.** The hub holds no forge credential
anywhere in this path — the one authenticated call is `gh`'s, already
installed and authenticated on the author's machine, and the hub never
prompts for, stores, or reads a token. And `publish` writes nothing into the
project: no build runs, no file is generated beside `tfsapp.config.json`, no
tag is pushed by the hub itself — `git.rs` only ever reads the project's
state, it never runs a command that changes it. `publish` is reachable from
no bridge route, no IPC command and no manifest key; an app never triggers
its own publish.

## Opening an app

**One OS process per open app.** `open <id>` resolves the app, then re-executes
the hub binary as a child carrying that app's identity. The child mutates its
runtime identity, boots the app's sidecar and opens the window; the parent
resolves, fails fast in a way both a terminal user and a desktop-entry launch
can act on, and gets out of the way without holding the shell.

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

### Where each process's output goes, and who reads a failure

Three processes write output during a launch, and each has a different
audience (plan 015):

- **The app's own sidecar** — FrankenPHP and the Messenger worker — always
  writes to `<state_root>/log/sidecar.log`. It never had a terminal to write
  to in the first place: it is a grandchild, spawned by the child long after
  the parent that had one has returned.
- **`open`'s own detached child** writes its routine lines — `is listening
  at`, the teardown lines — to `<state_root>/log/hub.log`, beside
  `sidecar.log`. Its parent returns as soon as it has the pid, so by the time
  the child has anything to say, a terminal that ran `open` has already moved
  on; inheriting would write those lines to a prompt nobody is reading. `dev`
  and `run` are the two paths where a terminal genuinely stays attached for
  the child's whole life, and both keep inheriting stdio unchanged.
- **A fatal startup error**, on either side of `open`'s re-exec, goes through
  one reporter: a line on stderr always, and the same blocking native dialog
  only when stderr is not a terminal (`std::io::IsTerminal`). A developer's
  typo at a terminal gets the line where they typed it; a `.desktop` launch,
  which has no stderr anyone will read, gets the dialog instead of failing
  silently. The decision is sound on both sides of the re-exec for two
  different reasons that land on the same rule: `rfd` is safe exactly while
  nothing has claimed GTK yet, which is true of the child before
  `tauri::Builder` runs and true of the parent for the whole of its life,
  since it never builds one at all.

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
window-close that started it, rather than once it is actually gone.

A launch probes both, in that order. A live sibling holding the serving lock
is what makes a second `open` of the same app surface the first instead of
starting a second server — `CONTRACT.md` §5's per-identifier guarantee,
mechanically. A sibling holding only the liveness lock — its serving claim
already released, still mid-teardown — is not a live sibling to hand off to:
the arriving launch waits, bounded, for the liveness lock to free, then starts
its own. Without that second lock a closed window and a gone process were the
same signal, and a launch arriving in the gap between them attached to a
backend already dying instead of starting a fresh one.

**Teardown runs in one order, and every step of it is load-bearing**: the
serving claim is released, then this process's webview windows are *destroyed*,
then the worker is stopped, then the server, and only then does the process
exit. The middle step is the one that is not obvious. Closing the last window
hides it rather than closing it, so the user's click lands while the work
happens off the GTK main thread — and a hidden webview still holds its
connection to the backend. The server's stop is graceful: it drains its
connections, the always-mounted Mercure hub makes one of them a stream, and a
stream never drains. So for as long as the client is alive, the server waits
for it and dies by `SIGKILL` instead of shutting down. The client goes first,
and the whole teardown takes about a third of a second.

For clients this process does not own — a browser opened on the app's port, a
`run` command mid-request — the bound is Caddy's own `grace_period` (§4), not
ours. The `SIGTERM`-then-`SIGKILL` escalation is what remains for something
genuinely stuck: an exception path, not the ordinary one.

The worker's pid is the second line of that pid file, which is what lets the
next launch reap it if this process never gets the chance. Children are killed
as a process group, so nothing survives a window closing — and a process that
has exited but has not yet been reaped is treated as gone rather than as
running, which is what lets the escalation stay an exception instead of firing
on every close.

Destroying the last window has one consequence worth stating, because it is
invisible and fatal: an event loop with no windows left asks to exit, and would
end the process mid-teardown, orphaning a FrankenPHP that has not been
signalled yet. The hub's run callback vetoes exactly that exit, and only while
a teardown is in flight, leaving its own `app.exit(0)` as the one thing that
ends the process.

A third lock joins these two once a `run` command exists — see "Running a
declared command" below for what it answers and how a launch reads it.

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

## Running a declared command

`run <id> <alias>` executes one of an app's own declared `bin/console`
commands in the foreground, from `hub/src/run.rs` — a transplant of the
station's own module, with one thing added in front of it: which app.
Everything it stands on already lived in `core::process` (the locks, the
signal machinery, the process-group teardown), because the station had
already built and measured it; the hub's own addition is the app-resolution
step, and the fact that it has two apps' worth of this state to keep apart —
`run.lock` is keyed on the app's own `identifier`, the same key the two locks
above already use, so two different apps running commands at once costs
nothing.

### A third lock, and which question each of the three answers

| Lock | Answers | Read by |
| --- | --- | --- |
| `serving.lock` | Will handing this launch's argv to that process get you a window right now? | a launch's own hand-off probe |
| `sidecar.pid.lock` (the liveness lock) | Does a process still own this data dir at all? | a launch's reap, and `run`'s own rule-3 concurrency probe |
| `run.lock` | Is a `run` command already active for this app? | `run`'s own rule 2, and a launch's rule-3-in-reverse refusal |

Rule 3's "is a window live" probe deliberately reads the **liveness** lock,
not the serving one: a process mid-teardown still owns the data dir, and a
`bin/console` command opening the app's SQLite while its server is being
killed is the same hazard as one opening it next to a live server. A launch's
own refusal reads `run.lock` the other way round — only once it has already
decided to launch (past the serving/liveness probe above), never against a
sibling it is about to hand off to, since a `concurrent` alias legitimately
running beside an already-open window must not be blocked by a second window
opening beside it.

`run.lock`'s content is not just a flock — it is a small record, `<alias>`
then `<alias>\n<pid>` once the child is spawned, so a refusal elsewhere can
name which alias is active rather than just the file. `run --stop`/`--replace`
read that record to know what to signal.

### Forwarding a signal past `Child::wait()`'s own retry

`std::process::Child::wait()` silently retries when the underlying wait is
interrupted by a caught signal, which means a thread blocked in it never
learns that `SIGINT`/`SIGTERM` arrived — a handler alone cannot forward what
the thread actually waiting never sees. The fix is the standard self-pipe
trick: the signal handler itself only writes one byte to a pipe (the one
thing sound to do inside a handler), and a *separate* thread blocks reading
that pipe and reacts once a byte arrives, leaving the thread in `wait()`
alone. `core::process::install_signal_forwarding` sets the pipe and the
handlers up once; `spawn_signal_forwarder` and the coexistence watchdog
(`spawn_coexistence_watchdog`) are two different reasons to terminate the
child, sharing the one mechanism — the first reacting to a caught signal, the
second to a poll of the liveness lock.

### Hub-side, and never `tauri`

`run.rs` depends on nothing from `tauri` — not the crate, not a running
`Builder`, not GTK. It runs to completion and exits before any window could
exist, exactly like `install`/`list`/`remove`. That is not a style
preference: a `run` command is an interactive terminal subcommand with the
terminal's own stdio inherited, and the app it runs beside may or may not
have a window open at all — nothing about it belongs behind the point where
Tauri claims the process.

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

The registry is keyed on `id`, the hub-local handle; the data directory, the
keyring namespace, the WebKit data directory and the `.desktop` entry are all
keyed on `identifier`, the app's own. `install` enforces that mapping being
one-to-one (`check_identifier_free`, plan 016): a second `id` for an
`identifier` another entry already carries is refused before anything is
written, since the two installs would otherwise share all four.

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

### Self-update, reconciliation and rollback

`--update` (`hub_update.rs`) cannot compare fingerprints itself: after it
swaps the binary, the process still answering is the *old* one, running the
*old* FrankenPHP — only a later invocation, of the *new* binary, can ask that
question honestly. Reconciliation (`reconcile.rs`) is therefore deferred and
keyed on state already recorded: `registry.json` carries the `hub_version` of
whichever hub last wrote it, and every `Level::App` command (the same table
the CLI grammar's own dispatch reads) compares that string against its own
version before doing anything else. Equal — the overwhelmingly common case —
costs one string comparison and nothing more. Different, it probes PHP once,
marks every app whose fingerprint moved `needs-revalidation`, leaves an
already-`broken` one alone, and re-stamps. This survives a hub replaced by
any means other than `--update` too — a hand download, a distribution
package, a `--rollback` — where a hook on `--update`'s own success would
silently miss every one of them.

Revalidation itself (`revalidate.rs`) is what a marked app's next `open` runs
into — described above. None of §6's lifecycle commands run alongside it: the
app's own version has not moved, so no event fires.

Two files must end up holding the new binary, not one: the stable copy under
`<hub root>/bin/tfsapp-hub`, which every generated `.desktop` entry's `Exec=`
names, and `$APPIMAGE`, wherever the user's own download happens to sit. The
stable copy is swapped first — every launcher on the machine silently
running the old hub, with nothing on screen to say so, is the worse of the
two possible partial failures, so it is the one `--update` protects against
finishing last.

The rollback anchor is the stable copy **renamed aside**
(`bin/tfsapp-hub.previous`), not a re-download: the hub already keeps a full
copy of what is running on the hub root's own filesystem, so renaming it
before the verified download replaces it is atomic, zero-copy and needs no
network of its own. A `registry.json.previous` snapshot is taken beside it,
under the registry lock, *before* the binary swap — a snapshot taken after
could already describe a state the new hub had begun to change. One
generation, like the per-app anchor `update <id>` leaves: a second
`--update` before a `--rollback` overwrites it rather than keeping two.

`--rollback` (`hub_rollback.rs`) touches no network at all. Its precondition
— both anchor halves present — is checked before anything else, including the
confirmation prompt, so a half-written anchor from an interrupted `--update`
reads as "nothing to roll back to" rather than a network problem. On
success it restores the binary, the registry (byte-identical to the
snapshot, not merely re-serialised from it) and `$APPIMAGE` — skipped
when that already names the stable copy, and noted rather than failed when
the user's own download no longer exists — then deletes the anchor: it is
consumed by the rollback that uses it, so a second `--rollback` finds
nothing rather than re-downloading an already-restored version.

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

## Four measurements that shaped the code

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

**`/proc/<pid>` outlives the process, so "is it still running" answered yes for
a corpse.** Nothing in this process reaps a child while the code that signalled
it is still waiting on it, so a child that obeyed its `SIGTERM` in 260 ms kept
its `/proc` entry for the whole three-second budget and was then `SIGKILL`ed
after the fact. Every teardown ended that way; both budgets were spent in full,
every time, and the six seconds were read as "FrankenPHP and the worker are slow
to stop" for two plans. `process_exists` reads `/proc/<pid>/stat`'s state field
for that reason, and the escalation went from being the rule to being an
exception path.

All four were found here rather than reasoned about, and the reason is
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
