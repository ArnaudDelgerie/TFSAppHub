# Changelog

## Unreleased

An app can now declare `file_associations.mime_types` plus the
`actions.open_files` receiver and be handed local files — through
`tfsapp-hub open <id> -- <file>...` or by choosing the app in the desktop
environment's "Open with" menu, its `.desktop` entry gaining `MimeType=` and
an `Exec=` ending in `-- %F`. One invocation is one opaque request in an
in-memory queue that exists before the splash: the whole batch is validated
first (local existing regular files only, the offending path named on
refusal), then delivered to the most recently focused eligible window through
a path-free `tfsapp://open-files-pending` notification the receiver answers
with `open_files_pending`/`open_files_ack` — subscribe before the first read,
then read on notification only. Acknowledgement is idempotent removal and
delivery is replayable until it happens, so app-side acceptance must be
idempotent by request id; a reload re-exposes an unacknowledged one. The queue
is bounded (64 requests, 64 paths, overflow refused loudly, never evicted) and
not durable: process exit ends its lifetime, and the single-instance
handoff/shutdown race is diagnosed rather than guaranteed. Apps declaring
nothing keep every surface they had.

An app can now declare `actions.close_guard` and register independent guards
for unsaved frontend work over IPC and background jobs over the PHP bridge.
Closing a protected window shows a native confirmation with Cancel as the
safe default; cancellation keeps the app running. Backend guards warn only
when closing would stop the shared backend. Both transports default to off.

Guards and window topology are rechecked when confirmation is answered. A
new relevant guard earns a fresh warning, and a document replaced while the
dialog is open invalidates the answer. Once approval is accepted, the window
is committed to close: a later reload does not cancel it. Window reservations
coordinate simultaneous closes, and normal close/signal teardown has one
owner. Mandatory shutdown does not wait for confirmation or for an event-loop
commitment; rare signal/window-creation overlap remains an accepted limitation.

An app declaring `actions.picker.ipc` can now restrict what the native file
chooser shows: `pick_path` accepts the same optional `filters` list as
`save_path` — one entry per filter, in the order given, the first one
active, extensions written without a leading dot. Omitted, `null` and an
empty list all leave the chooser unrestricted. A well-formed filter list on
a directory selection is ignored rather than refused, so callers can share
one options object between both kinds; a wrongly typed filter field still
fails the call before any dialog opens.

Restoring a backup no longer keeps settings cached from the previous
database. `import` now clears the destination's `cache/` and `build/` and
discards its stamp before replacing anything — including when the archive's
version matches the installed app's, the one case where the cache stamp's
comparison could not tell the compiled container it was stale, and the one
that left a restored app serving the previous database's theme, language
and provider settings. A `run` command executed right after the import reads
the imported database too, and an older archive migrates forward without
the old cache; the fresh cache that migration builds survives the import. A
cache the import cannot clear stops it before any data is replaced, naming
the path.

An app declaring `actions.picker.ipc` can now open a native Save As dialog
(`save_path`), with its own filters, suggested file name, starting directory
and GTK's own overwrite confirmation, resolving to the path typed or to
`null` on cancel — the hub never writes to it. And a plain
`Content-Disposition: attachment` response saving into the OS download
directory is no longer provisional: it is documented, definitive behaviour,
with `save_path` as the way to offer a person a choice of destination
instead.

Opening an installed app is now measurably faster the second time, and every
time after that: `cache/`/`build/` used to be emptied on every single launch,
so each `open` compiled the Symfony container from scratch — now `install`
and `update` warm it themselves, right after their own lifecycle commands,
and a launch reuses what they built as long as nothing has moved under it.
Measured on the reference app, that is the difference between roughly 3s and
roughly 1.1s per launch. `cache/`/`build/` are still emptied — `hub.log`
names why — whenever the app's version, its installed path, or the hub's own
PHP fingerprint no longer matches what the cache was built against: after an
`update`, after a `rollback` (whose next launch rebuilds at the un-warmed
cost, and every launch after it until the next `install` or `update`), and
after a hub self-update that moves PHP under an app pending revalidation.
Nothing an app declares changes, and `CONTRACT.md` §3's guarantee that
`APP_CACHE_DIR`/`APP_BUILD_DIR` may be emptied at any launch is unchanged —
only how often the hub actually chooses to.

## 0.3.0

No user-facing change — this release exists to validate `tfsapp-hub --update`
and `--rollback` end to end against a real release (plan 020, step 8).

## 0.2.0

An app launched from its desktop entry, whose launch then fails — a renamed
snapshot, a broken dependency, anything `open` refuses — now puts a native
dialog on screen naming the app and the way out, where before it silently
failed to open with nothing on screen at all. The same failure from a
terminal still prints one line and raises no dialog. And `open`'s routine
output — `is listening at`, the teardown lines — no longer lands on a
terminal the shell has already moved on from: it goes to `log/hub.log`
beside the app's other logs (CONTRACT.md §5), with a dated header per
launch. `dev` and `run` are unaffected — their terminal stays attached for
the whole of the child's life, and both still print everything there.

Closing an app's window now tears its backend down in about a third of a
second, where it took 6.3 s — and tears it down *gracefully*, which it never
once did. Two independent causes, both of them invisible from the outside: a
process that had exited but not yet been reaped was counted as still running,
so the `SIGTERM`-then-`SIGKILL` escalation spent its full budget and then
killed a corpse on every single close; and the window was only hidden, so its
webview held a Mercure stream open against Caddy's graceful shutdown, which
waited for a client that was never going to let go. Teardown now destroys its
windows before signalling the server, and the escalation has become what it was
always meant to be — an exception path for something genuinely stuck.

For app authors, one thing is newly guaranteed and worth reading in CONTRACT.md
§4: PHP's shutdown functions actually run when the app closes, and a request
still in flight gets two seconds to finish before its connection is closed
under it. Work that needs longer than a request belongs in `async_worker` or in
a `run` command, as it always did — the difference is that this is now a stated
bound rather than an accident.

## 0.1.0

First packaged release: `make build` produces a repaired, self-contained
`TFSAppHub_<version>_amd64.AppImage` — its bundled FrankenPHP runs instead of
segfaulting, no symlink in it points outside itself, it carries no frozen
`libwayland-*`, and a `.versions.txt` beside it names the frozen WebKit/GTK
stack and the glibc floor it requires. Copied alone to a machine with no Rust,
no PHP and no Tauri toolchain, that one file installs a Symfony app, shows it
in the shell's grid under its own name and icon, opens it into a working
window, and removes it again. `docker compose run --rm build` (under `build/`)
rebuilds the same artifact against an older base for anyone the default
release does not run on.
