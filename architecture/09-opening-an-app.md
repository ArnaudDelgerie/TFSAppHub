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
   `messenger:setup-transports` runs, then the server, then every worker.
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

### The cache stamp, and why a launch only sometimes rebuilds (plan 024)

`APP_CACHE_DIR`/`APP_BUILD_DIR` hold the compiled Symfony container —
`CONTRACT.md` §3 lets the host empty them at any launch, and for a while the
hub always did: the station wiped both on every launch because a random
`/tmp/.mount_*` FUSE path baked itself into the compiled container, so reusing
one across an upgrade meant running last version's container against this
version's code, and the hub inherited that workaround deliberately without
inheriting its cause — installed apps run from a stable real path
(`<OS data>/TFSApp/hub/apps/<id>/`), never a fresh mount per launch. Doing
that unconditionally on every `open` meant every single launch of an
installed app compiled the container from scratch,
in a process with no terminal attached, while the user watched the cold-start
splash — the dominant cost of a launch by far, confirmed by the measurement
in `.project/plan/024-a-persistent-warm-symfony-cache.md`.

The replacement moves the build to the moment that has a terminal and makes
the invalidation explicit. `install` and `update` each run `bin/console
cache:warmup` themselves, through the app's own toolchain, right after the
lifecycle event's commands succeed — the hub runs it, not the app, because an
app that forgets to declare a warm-up must not be the one paying for it at
every launch, and `cache/`/`build/` are the hub's own directories to manage.
Only once that warm-up succeeds does the hub write a stamp to
`data/cache.json`, beside `data/config.json`: the app's `app_version`, the
absolute path of the snapshot the container was compiled from, and the
`Platform` fingerprint (below) the PHP that compiled it ran under — the same
shape and the same "never written speculatively" rule as the version record.

At `open`, `Mode::Launch` compares that stamp against what the launch is
actually about to run, and empties `cache/`/`build/` only when one of the
three no longer matches, logging which one to `hub.log` before it does:

- **`app_version`** moves on every `update`, and moves *back* on a
  `rollback` — which restores the tree but, deliberately, does not rewrite
  the stamp `update`'s own warm-up left behind (out of scope for this plan:
  "warming at rollback" makes the first launch after one *correct*, by
  wiping; making it *fast* too is a separate, smaller question). So the
  first launch after a rollback rebuilds, at the un-warmed cost, and every
  launch after it keeps rebuilding the same way until the next `install` or
  `update` re-stamps it.
- **the snapshot path** only moves if an installed app's tree were relocated
  outside the hub's own commands — not something anything here does, but
  cheap to check since the stamp already carries it.
- **the `Platform` fingerprint** moves when a hub self-update changes the
  bundled PHP or its extensions. `open::resolve` carries the registry entry's
  platform provisionally into the child; after the splash is painted and the
  launch locks are held, `revalidate::revalidate` returns the freshly probed
  `Platform`, which replaces that provisional value before the stamp is
  compared or written. Otherwise a revalidation that itself detected the
  drift would immediately paper back over it. `revalidate.rs` itself never
  touches `cache.json`: the mismatch is caught here, naturally, the same way
  a rollback's is.
- **a hand-emptied `cache/`** overrides an otherwise-matching stamp: the
  stamp is a claim about what was built, not a promise that it is still on
  disk, and a launch must never reuse a container that is not there.

The one thing that invalidates without moving any of the three dimensions is
`import` — which is why it does not lean on this comparison at all. The
database a restored archive replaces is not a stamp dimension, so an
equal-version restore compares clean and would hand the next launch the
previous database's container, stale settings included. `import` therefore
clears explicitly, discarding the stamp and emptying both directories itself
before its rescue phase (`architecture/06`), whether the archive matches the
installed version or is older. An equal-version import leaves no stamp and
adds no warm-up of its own: the next `open` rebuilds at the cold-start cost,
exactly as it does for any data directory that was never stamped. An older
archive's migrate-forward warm-up may write a fresh stamp for the installed
version; the import is already past its cleanup by then and leaves that fresh
one alone.

`Mode::Install`, `Mode::Dev` and `Mode::Run` are untouched by any of this —
`Mode::Install` always starts from an empty cache by construction, and dev's
own container invalidates itself on file change (`Mode::Dev` never wipes,
matching the CONTRACT.md §3 parenthetical).

**Why the layout does not move.** Putting `cache/`/`build/` inside the
snapshot (`apps/<id>/`) was considered and rejected: it would make
update/rollback invalidation structural and free — a new tree simply has no
old cache to find — but it drags the cache out of the `0700` directory
`paths::create_app_data_dir` enforces, and tightening `apps/<id>/` to match
is a permissions change plan 024 was not worth spending. The five
directories (`cache/`, `build/`, `log/`, `sessions/`, `data/`) stay exactly
where §3 already puts them; only *when* two of them are emptied changed.

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
then every worker is stopped, then the server, and only then does the process
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

The pid file is the server's pid, then one line per live worker — plan 045's
shape, one slot per declared consumer, written through a single shared table
so no slot's supervisor can overwrite a sibling's line by rewriting the file
from its own knowledge alone. That shape is what lets the next launch reap
every one of them if this process never gets the chance. Children are killed
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

An app that declares off-window work gets one consumer per declared worker
(§2's `count`, flattened into one slot each), each recycled on its own time and
memory limits, respawned with an exponential backoff, and given up on after
five consecutive failed starts on that slot alone — a crash-looping consumer
does not make a healthy sibling give up. The user is told at most once per
launch, even if more than one slot gives up, and the message names the
transports whose consumption stopped.

Those constants are the app's guarantee rather than the host's, which is why
they are not tuned here: a host that supervised differently would make the same
manifest mean two different things.

Every worker belongs to the **sidecar's** lifetime, not the window's. A second
`open` of the same app cannot spawn a second consumer, because it never gets
past the serving lock.

### Navigation policy

Every window enforces the same classification:

| target | outcome |
| --- | --- |
| the backend's own origin, or the bundled-asset origin | let through |
| `tfsapp-splash:` before the backend hand-over | let through |
| another `http`/`https` origin | cancelled here, opened in the user's own browser |
| `javascript:`, `file:`, `data:`, `blob:`, or a custom scheme after hand-over | cancelled, and said so |

No in-app popup is ever created. An external link belongs in the browser where
the user has their bookmarks, their sessions and an address bar.

