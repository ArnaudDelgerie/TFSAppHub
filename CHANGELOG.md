# Changelog

## 0.4.0

Official releases now build in Docker on Ubuntu 22.04, the oldest Ubuntu LTS
still in standard support. The AppImage's measured glibc floor is 2.35, so
it can run on Ubuntu 22.04, Debian 12, Mint 21 and compatible newer systems
whose C++ runtime also provides its required symbols. The adjacent
`.versions.txt` now records `glibcxx_floor=3.4.30` alongside `glibc_floor`,
making the libstdc++ requirement visible. The repair pass leaves graphics
driver libraries to the host. The official base moves to Ubuntu 24.04 when
Jammy's standard support ends in April 2027.

The AppImage now bundles the build host's GStreamer plugins and helpers, so
apps that declare microphone access can record inside the AppImage. The build
refuses an image missing a capture plugin and records its frozen GStreamer
version beside the artifact. Bundling raised the initial image size by
86,482,944 bytes against the 0.4.0 candidate measured before this change;
the local Ubuntu 24.04 image was 226,601,464 bytes. The official Jammy
image is 151,558,648 bytes with 103 plugins.

An app's frontend build output now ships in its release archive without being
committed: `"build_outputs": ["public/build"]` in `tfsapp.config.json` names
the project-relative, gitignored directories the author's own build step
produces, and `publish` — forge and `--local` alike — copies them into the
archive as they stand at publish time, after refusing, before building
anything, a declared path that escapes the project, sits under an excluded
top-level component, is absent or empty, is not gitignored, holds a tracked
file, or holds anything but regular files and directories. The pinned-commit
guarantee now covers the tracked files; the declared outputs are the author's
bytes at publish time, under the same `SHA256SUMS.txt`. `install`, `update`
and `dev` ignore the key. The announcement prints `Inputs  pinned Git tree +
build outputs` when any are declared, plus one line per output with its file
count, total size and newest mtime.

The microphone can now be switched off per installation, and a running capture
is visible in the window's title. `"revoked": {"media": {"microphone":
true}}` in an app's `data/config.json` — hand-edited, like `port_override`,
and kept across updates — makes the hub deny every capture request with a
logged reason and report `TFS_MEDIA_MICROPHONE=0` although the manifest
declares the microphone; a `config.json` that does not parse fails closed.
While a capture runs, the title reads `Microphone on — <product name>`
(`Microphone muted — <product name>` while muted) and returns to the plain
name when it stops or the web process dies mid-capture; under Wayland the
indicator also writes tao's header bar, whose title a plain `set_title`
cannot reach.

A web process killed before its window's first page ever committed now gets
the crash page too: the Reload link targets the URL the window was created
with (the splash's own URL, or the bundled fallback's), so an early engine
death has the same page in the app's colours and the same way back as any
other. `TFS_MEDIA_MICROPHONE` now reports the microphone grant only once the
window's `with_webview` closure really wrote the setting and connected the
permission handler — the launch waits up to 2 s for that report before
starting the sidecar, and a timeout, a window closed first or a webview
without settings reports `0` with one warning line.

The secret store no longer reports a failed write as a success: a set, delete,
get, has or list that the keyring or the fallback file fails answers
`storage_failed` over IPC and `500 {"error": "storage_failed"}` on the bridge,
with one warning line in `hub.log` naming the cause (never the key or the
value). Every keyring call — the startup probe included — now carries a
5-second deadline, so a frozen Secret Service makes the launch fall back to
the file store (or answers an operation as a failure) instead of holding the
splash or a request forever. A corrupt `secrets.json` is left byte-for-byte
untouched instead of being overwritten by the next write, and `purge` reports
a keyring delete it could not make as `FAILED (<cause>)` instead of
"already clean".

`install` and `update` no longer accept a working directory: every install
and update now starts from a checksummed release archive, from a forge or
from `publish <project> --local <dir>`. A directory argument is refused with
the `publish --local` route (and `dev <path>` for running a project in
place). With it, `update --force` is gone — an equal version is always
refused — and `list` no longer shows a "changed since install" marker. A
registry that still holds a `local-path` entry is refused as a whole; there
is no migration, since no user exists yet (decision 009).

Apps can now be published to a local folder with `publish <project> --local
<dir>`. The folder contains a release archive, `SHA256SUMS.txt` and `NOTES.md`.
`install <archive.tar.gz>` verifies the adjacent checksums, and `update <id>
<archive.tar.gz>` applies a newer archive. Backups passed to install or update,
and releases passed to import, now point to the appropriate command.

Two guards audit 020 found checked once, outside the lock that was supposed
to make them true, now hold under it. Every lifecycle lease holder except
`repair` re-reads the update journal while it owns its lease, so a command
that wins its lease after an `update` left a journal behind refuses and names
`tfsapp-hub repair <id> --yes` instead of running over the interrupted state.
An install claims `apps/<id>/` atomically and re-checks the app's `id` and
pinned port under the registry's own lock, so two installs with different
identifiers can no longer land on one directory, one registry entry or one
port; a refusal there removes the copied tree and keeps the data directory.

Archive extraction now refuses hard links, devices, fifos, sparse files and
other unsupported entry types. Release downloads, extraction and imports
check available disk space before writing large payloads; imports also cap
`manifest.json` at 1 MiB. Export streams database and upload files instead
of loading them into memory, and preserves their modification times.

Simultaneous `run` starts now claim their own records before checking a
non-`concurrent` alias's exclusion, so they cannot both launch commands.
Scans preserve entries named for live launchers, including entries created
just before their lock is taken. `sidecar.pid` is now replaced through a
synced temporary file, preserving the previous full pid list if a rewrite
is interrupted.

`tfsapp-hub repair <id>` is now safe against a kill at any instant of an
interrupted `update`, not only right after one of its
journal writes — the gap between a filesystem change and the journal record
of it is now covered too. It also finishes rather than reverts an update
that was killed after the registry already recorded the new version: the
update had already happened at that point, so `repair` now completes the
rollback-point promotion instead of undoing a commit that already landed,
leaving `rollback <id>` usable afterwards where it previously could not be.
Every earlier interruption still restores the outgoing version exactly as
before.

An interrupted `import` or `rollback` is now recoverable too. `import`
extracts the archive into a staging directory before touching the live data,
records a durable intent, and only then switches the live database and
`uploads/` over through renames to reserved rescue names: a kill leaves a
state the hub names, and `tfsapp-hub repair <id>` puts back exactly what the
import replaced before its commit, or finishes the import after it. An
import whose extraction fails never touches the live data at all, and a
failure between the switch and the commit is backed out in-process. A
`rollback` killed partway through is finished by running
`tfsapp-hub rollback <id>` again, never rewound; until it finishes, every
other command for that app, `repair` included, refuses and names it. All
three interrupted-operation records — update journal, import intent,
rollback marker — are now checked by every lifecycle lease holder and by
the dispatch advisory check.

An app can now declare `actions.paths` — one boolean per GLib special
directory (`desktop`, `documents`, `downloads`, `music`, `pictures`,
`public_share`, `templates`, `videos`) — and have the hub resolve each
declared member through `glib::user_special_dir` (the same file,
`~/.config/user-dirs.dirs`, GLib itself reads) and report it as
`TFS_USER_<NAME>_DIR`. Like `media`, this group's members name a resource
rather than a transport, so `ipc`/`bridge` are refused under it. A declared
member GLib cannot resolve reports as an absent variable — never an empty
string, never a guessed `$HOME`-based fallback. `$HOME` is deliberately not a
ninth member: it already reaches PHP through the ordinary process
environment. No filesystem grant and no existence guarantee either — PHP's
rights are unchanged, and a resolved path may name a directory deleted a
moment ago; see
[decision 008](.project/decision/008-user-directories-are-a-declared-capability.md)
for the full reasoning.

An app can now declare `actions.media.microphone` and reach the microphone
through the ordinary web platform — `getUserMedia({audio: true})` on its own
page, never through IPC or the bridge, which is why the group has neither
transport (`ipc`/`bridge` are refused under it, like `picker.bridge`).
Undeclared, the webview keeps WebKitGTK's own default of no capture at all —
no dialog, nothing to retry. Declared, exactly two WebKit permission
requests are ever granted, both only on the app's own origin: an audio-only
capture (a combined audio+video request is refused as a whole) and a
device-info request, the second needed for `enumerateDevices()` to return
labels at all. Every other permission kind stays denied, whatever the
manifest says. `TFS_MEDIA_MICROPHONE` reports whether the app's page is
granted the request in the first place, never whether hardware answers or a
recording API works — the hub adds no audio stack, so an app should expect
`MediaRecorder` itself to vary by machine and fall back to Web Audio, and
must set its own CSP (`media-src 'self' blob:`) to play a recorded clip
back. No prompt and no capture-in-progress indicator ship yet — the
declaration, readable before install, is the whole of the consent story for
now.

An app can now declare `file_associations.mime_types` plus the
`actions.open_files` receiver and be handed local paths — through
`tfsapp-hub open <id> -- <path>...` or by choosing the app in the desktop
environment's "Open with" menu, its `.desktop` entry gaining `MimeType=` and
an `Exec=` ending in `-- %F`. One invocation is one opaque request in an
in-memory queue that exists before the splash: the whole batch is validated
first (every path a local existing regular file, plus existing directories
when the receiver opts into `actions.open_files.directories` — a directory
counts as one path, is never enumerated, and `inode/directory` in the MIME
list is what advertises the app for one in the file manager; the offending
path named on refusal), then delivered to the most recently focused eligible
window through
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

A window whose web process dies — a crash, a memory limit, any end to
`WebKitWebProcess` the hub did not ask for itself — no longer sits on a dead
page. `hub.log` records the reason, and the window shows the hub's own crash
page in the app's declared `splash_bg` / `splash_text` with its
`product_name`, with a Reload link that goes back through the same
navigation policy and page-load hooks as any other load. There is no
automatic reload and no crash-loop heuristic: the user's click is the only
way back, and the app is never told a reload followed a crash. A close guard
held by the document that crashed no longer blocks closing the window.

The hub's own releases now carry their compatibility record and their
provenance: the AppImage's `.versions.txt` is attached beside it and listed
in `SHA256SUMS.txt`, and the release notes end with a
`Built from <repo>@<sha>` line naming the exact revision the binary was
built from.

`rollback` now restores the recorded source along with the version, tree,
database and source revision: after an update and a rollback, `list` and
`registry.json` name the tag, location and kind the restored version was
installed from, for a forge release and a local archive alike. A bare `update
<id>` after a forge → archive → rollback therefore queries the forge again.
The rollback anchor records the outgoing source; an anchor written by an
earlier hub has none, and its rollback keeps the current source and says so.
`rollback` prints the source change it is about to make.

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
