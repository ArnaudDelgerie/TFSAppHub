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
5. The **same window** is navigated to the backend, via `location.replace`
   rather than a plain navigation — it replaces the splash's history entry
   instead of stacking on it, so the cold-start page leaves no history entry
   behind for Back to return to.

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

**A guarded close asks before any of that begins.** An app that declares
`actions.close_guard` (§7) can mark a document's unsaved work from the webview
and app-wide background work from PHP; closing a window that carries such a
mark vetoes the default close and shows one native confirmation, `Cancel` as
the safe default, before the window is hidden or the backend stopped. Backend
guards only matter on the close that would stop the shared backend — a clean
secondary window closes without a word even while jobs run elsewhere. The
decision machinery lives in `close_guard.rs`, plain state with no GTK in it,
and its effects are coordinated on the event loop: one pending decision at a
time across the app's windows (repeated clicks veto rather than stack
dialogs), the guards rechecked against the topology at answer time (a guard
that appeared mid-dialog earns a fresh warning, never a stale approval), and
the approval consumed the moment it is applied. The final guard check and the
close commitment are one atomic transition: a registration racing a close is
either part of its decision or refused with `closing`, never accepted and
then ignored by a teardown that revalidates nothing — and a committed close
reserves its window until destruction or failed posting, so two
nearly-simultaneous closes cannot both count the other as the survivor that
preserves the backend. A pending decision belongs to its document: a reload
while the dialog is open invalidates the answer. Once the final checks accept
that answer, closing the window is committed and later loads do not reverse
it. The approved secondary close uses the captured native window handle's
`close()`; the resulting `CloseRequested` observes the reservation and proceeds
without another dialog. No native-request token, acknowledgement or submission
worker is needed.

Second-instance admission and creation run together on the event loop,
serialized with normal close decisions. Signal and fatal-error cleanup keep
their independent worker paths so cleanup can start even if GTK is blocked.
A signal concurrent with window creation may miss that new window in the
cleanup snapshot; it can remain until process exit and extend the bounded
window-destruction wait. This residual shutdown race is accepted for this
local single-user application.

Normal close and signal teardown share a one-shot execution latch. Fatal
startup errors keep their own cleanup, error dialog and exit status.
Mandatory shutdown never asks: the first step of every teardown path — the
approved close itself, a signal, a fatal error — commits shutdown one-way,
which turns any dialog still standing stale and refuses new guards. Cancelling
is the null outcome: the window stays visible, the backend keeps serving, the
guards stay standing. An app that declares nothing sees none of this.

A third lock joins these two once a `run` command exists — see "Running a
declared command" below for what it answers and how a launch reads it.

### File requests: `open <id> -- <path>...` and the "Open with" menu

An app that declares the pair — `file_associations.mime_types` in §2 plus
`actions.open_files.ipc` in §7 — can be handed local paths, either through
`open <id> -- <path>...` or by choosing it in the desktop environment's
"Open with" menu (its `.desktop` entry gains `MimeType=` and an `Exec=` ending
in `-- %F`, the freedesktop field code the environment expands into separate
local-file arguments; see "Installing"). Declaring `inode/directory` in the
MIME list plus the receiver's `directories` opt-in (§2's pair rule) is what
puts the app in the menu for a directory; apps declaring nothing keep every
surface they had.

**One invocation is one request**: an opaque id the hub generates, plus the
ordered path list that invocation carried. Repeating the same open on the same
path is a new request, always. The whole batch is validated before anything is
enqueued — every path a local, existing, regular file, or an existing local
directory when the receiver opted into directories (a directory counts as one
path and is never enumerated; symbolic links are followed, as for files) —
and a batch that fails is refused whole, naming the offending path, with
nothing enqueued. That is not a readability promise: deletion, permission
changes and unsuitable content remain the app's to handle when it opens what
it received.

The queue lives in the hub process's memory and exists before the splash or
the backend — a cold start's paths are enqueued in the child before any window
is created, and an arrival during the splash simply waits for the first app
document. Its lifetime is the process's: a hub crash, a failed startup or
ordinary exit ends it. Bounds are finite and overflow is loud — at most 64
pending requests per app, 64 paths per request, refused past either, never
evicted.

On the CLI, `--` is the separator that makes path operands possible: after it
nothing is a flag, so spaces, Unicode, quotes and option-looking names are one
argument each, taken verbatim with no shell or URL interpolation. Relative
paths are normalized against the calling process's working directory before
the child is detached, and a separator with nothing after it is the no-file
form — a bare menu launch of a declaring app expands `%F` to zero arguments
and leaves exactly that trailing `--`.

A second launch while the app runs is where the single-instance handoff
earns its keep: the second process's argv reaches the live instance through
the pinned plugin's callback, and a file-bearing argv is parsed and enqueued
on the event loop — the same admission boundary a second window goes through —
instead of being discarded. A no-file argv keeps the new-window behaviour it
has always had. The request then targets the most recently focused eligible
window, with a deterministic fallback to a surviving one and a best-effort
raise; file requests never navigate or reload a page.

**The receiver's half is pull, not push.** After any queue update the hub emits
one Tauri event, `tfsapp://open-files-pending`, to the selected window only —
path-free, a bell rather than an envelope. The queue stays the source of
truth: an emit failure is diagnosed and the requests stay queued, because the
next notification or the receiver's own startup retrieves everything still
pending. A receiver registers its listener and awaits it before the first
`open_files_pending` read — that ordering is what covers an arrival racing the
initial read — and afterwards reads on notification only, never on a timer.
The listener is registered on the receiver's own `WebviewWindow`, matching the
hub's one-window emission; a target-less `listen()` would also receive the
notifications emitted to the app's other windows.
`open_files_ack` removes a request, idempotently, and only the selected
window can read or acknowledge its requests; a window closing with requests
still pending hands them to a surviving eligible window, never to one whose
close has committed.

Delivery is replayable until acknowledgement, not exactly-once: a reload
between the app's acceptance and its ack re-exposes the same id, so acceptance
must be idempotent by request id on the app's side — the example in §7 is the
pattern. Two limits are stated rather than papered over: the queue is not
durable across a crash or shutdown, and the single-instance plugin's callback
carries no application-acceptance reply, so the narrow handoff/shutdown race
is diagnosed where observable but not guaranteed delivered.

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

### Declared microphone capture

An app that declares `actions.media.microphone` (§7, decision 007) gets the
capability at the window, not at the backend: after `create_splash_window` or
`create_app_window` builds a window, `media::install_permission_handler`
reaches its webview through `with_webview` and does two things. It writes
WebKitGTK's `enable-media-stream` setting — only when the microphone's
access is `Allowed`; left alone otherwise, keeping WebKitGTK's own default of
no capture at all. And it connects one `permission-request` handler, on
**every** window regardless of the access: wry itself connects none, so an
app without the grant would otherwise fall through to whatever WebKit's own
default turns out to be rather than an explicit, logged refusal.

The access is one value, computed once per launch where the manifest and the
data directory are both known (`media::MicrophoneAccess`, pure and
unit-tested): `Undeclared`, `Revoked` — the manifest declares the microphone
but this installation's `data/config.json` switches it off (§5) — or
`Allowed`. A revoked microphone follows the undeclared rule exactly:
`enable-media-stream` is not written either, so the app meets the platform
with no capture at all, and `TFS_MEDIA_MICROPHONE` reports `0` (§3).

The handler's decision is pure and unit-tested (`media::decide`): a
`UserMediaPermissionRequest` is allowed only when it is for an audio device
and not a video device (WebKit grants a combined request wholly or not at
all), a `DeviceInfoPermissionRequest` is allowed under the same two
conditions — without it `enumerateDevices()` returns no labels — and every
other permission kind is denied outright. Both conditions have to hold: the
microphone's access is `Allowed`, **and** the requesting page is the app's own
origin at the moment of the request, read from the webview's current URI and
compared against the published `AppOriginSlot` with the same `same_origin`
`window.rs` already uses for navigation. Before that slot is published — the
whole of the splash's life before hand-over — no page can be the app's own
origin, whatever it asks for; this is what keeps the cold-start page, on its
own scheme, from ever receiving a grant. The splash and the app share one
webview (this file's own opening section), so the one install made on the
splash window covers the app's whole first life — a further window from a
second `open` (`window::create_app_window`) gets its own install, on the same
terms. The closure reports on a channel whether it really wrote the setting
and connected the handler, and `serve` waits up to 2 s for that report before
resolving the environment — only when the access is `Allowed`; anything else
resolves the environment without waiting, `TFS_MEDIA_MICROPHONE=0` already
settled — and never from the main thread, which is the thread the closure
itself needs — so `TFS_MEDIA_MICROPHONE` says what actually happened, not
what was scheduled.

**A running capture is visible in the window's title.** Only on a window
whose access is `Allowed` does the same closure connect two more signals,
both before the grant report is sent. `microphone-capture-state-notify` fires
on every state change and sets the toplevel's title from
`media::capture_title` (pure and unit-tested): `Microphone on — <product
name>` while a capture is active, `Microphone muted — <product name>` while
it is muted, the plain product name once it stops, with one `hub.log` line
per change. Under Wayland the title is written to the `gtk::HeaderBar` tao
builds into the window as well as to the window itself, because that header
bar's title is a snapshot `gtk_window_set_title` cannot reach. And a second
`web-process-terminated` handler — the same signal `crash.rs` already owns,
touching nothing of the crash page — puts the plain name back, so a web
process dying mid-capture cannot leave the indicator stuck.

Every denied `UserMediaPermissionRequest` is logged to `hub.log` with its
reason (revoked, undeclared, wrong origin, or video asked for) — `DeviceInfoPermissionRequest`
and every other kind are denied silently, the same as any other navigation
refused outside this group.

### Recovering from a dead web process (plan 058)

`crash::install_crash_recovery_handler` connects `web-process-terminated`
through `with_webview`, the same shape as the microphone handler above and on
the same two windows — one install covers the window's whole life, since the
splash and the app share one webview across hand-over.

WebKit's own reason decides what happens next (`crash::decide`, pure and
unit-tested). `TerminatedByApi` is logged and left alone: nothing in this tree
calls `webkit_web_view_terminate_web_process` today, but the signal fires the
same way for it, and treating it as a crash would show the crash page over a
termination the hub itself caused. `Crashed`, `ExceededMemoryLimit`, and any
reason a future WebKitGTK adds — the enum is `#[non_exhaustive]` — are
recovered from: a `hub.log` line names the window, the reason and the URI that
was displayed, then two things happen.

**The dead document's close-guard state is released.**
`close_guard::CloseGuardState::end_document` chains `rotate_context` with the
very `context()` fetch that erases what does not match it (`close_guard.rs`'s
own header) — a crash ends a document as surely as a navigation does, but
unlike a navigation, no successor page will ever make that fetch to trigger
the usual erasure, so `end_document` forces it immediately. Without this, a
guard the dead document held would block closing the window forever.

**The hub's own crash page replaces what the dead process last painted.**
`crash::render_crash_page` builds it inline, in the app's declared
`splash_bg` / `splash_text` with its `product_name` — the same palette and
fallback the cold-start page uses (`CONTRACT.md` §8) — and it is shown with
`load_alternate_html`, not a navigation: the content is displayed *for* the
URI that died without issuing a real load, so the navigation policy above
never sees it. The Reload link's `href` is that same dead URI, so clicking it
is an ordinary link navigation that goes through the very same
`classify_navigation` and `on_page_load` context rotation as any other load —
no separate reload path exists to drift from those. If no URI had committed
yet when the process died, the Reload link targets the URL the window was
created with instead — the splash's own URL, or the bare `tauri://localhost`
Tauri resolves the bundled fallback to — which `classify_navigation` lets
through exactly like any other bundled-asset URL, so that early a death gets
the same crash page and the same way back as any other. There is deliberately
no automatic reload: whatever crashed the process may crash again on the same
input, and the user's click is what breaks that loop.

**`open_files` requests pending when the process died are untouched by any of
this.** The queue lives in the hub process, not the webview, so it survives
the crash intact; the existing reload replay (`open_files_pending`, above)
delivers them again once the app's own listener re-registers after Reload
brings the real page back.
