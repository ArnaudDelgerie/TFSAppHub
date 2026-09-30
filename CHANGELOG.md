# Changelog

## 0.1.0

First packaged release: `make build` produces a repaired, self-contained
`TFSAppHub_<version>_amd64.AppImage` — its bundled FrankenPHP runs, no symlink
in it points outside itself, it carries no frozen `libwayland-*`, and a
`.versions.txt` beside it names the frozen WebKit/GTK stack and the glibc
floor it requires. Copied alone to a machine with no Rust, no PHP and no
Tauri toolchain, that one file installs a Symfony app, shows it in the
shell's grid under its own name and icon, opens it into a working window,
and removes it again. `docker compose run --rm build` (under `build/`)
rebuilds the same artifact against an older base for anyone the default
release does not run on.

When an app launched from its desktop entry fails to launch — a renamed
snapshot, a broken dependency, anything `open` refuses — a native dialog
names the app and the way out. The same failure from a terminal prints one
line and raises no dialog. `open`'s routine output — `is listening at`, the
teardown lines — goes to `log/hub.log` beside the app's other logs
(CONTRACT.md §5), with a dated header per launch; `dev` and `run` keep
their terminal attached for the whole of the child's life and print
everything there.

Closing an app's window tears its backend down in about a third of a
second, and tears it down *gracefully*. Teardown destroys the app's windows
before signalling the server — a webview holding a stream open would
otherwise keep Caddy's graceful shutdown waiting for a client that will
never let go — and the `SIGTERM`-then-`SIGKILL` escalation is an exception
path for something genuinely stuck: a process that has exited but not yet
been reaped does not count as still running, so it cannot spend the
escalation's budget.

For app authors, CONTRACT.md §4 states the bound on close: PHP's shutdown
functions run when the app closes, and a request still in flight gets two
seconds to finish before its connection is closed under it. Work that needs
longer than a request belongs in `async_worker` or in a `run` command.

Hub self-update checks that the downloaded AppImage runs on this machine and
reports the version promised by the release tag before replacing either
copy; an incompatible release is refused without changing the installed hub.
`--update --from <AppImage>` installs a local rebuild of the current or a
newer version through the same swap and rollback path; older versions are
refused with a pointer to `--rollback`.

Official releases build in Docker on Ubuntu 22.04, the oldest Ubuntu LTS
still in standard support. The AppImage's measured glibc floor is 2.35, so
it can run on Ubuntu 22.04, Debian 12, Mint 21 and compatible newer systems
whose C++ runtime also provides its required symbols. The adjacent
`.versions.txt` records `glibcxx_floor=3.4.30` alongside `glibc_floor`,
making the libstdc++ requirement visible. The repair pass leaves graphics
driver libraries to the host. The official base moves to Ubuntu 24.04 when
Jammy's standard support ends in April 2027.

The AppImage bundles the build host's GStreamer plugins and helpers, so apps
that declare microphone access can record inside the AppImage. The build
refuses an image missing a capture plugin and records its frozen GStreamer
version beside the artifact; the official Jammy image is 151,558,648 bytes
with 103 plugins.

An app's frontend build output ships in its release archive without being
committed: `"build_outputs": ["public/build"]` in `tfsapp.config.json` names
the project-relative, gitignored directories the author's own build step
produces, and `publish` — forge and `--local` alike — copies them into the
archive as they stand at publish time, after refusing, before building
anything, a declared path that escapes the project, sits under an excluded
top-level component, is absent or empty, is not gitignored, holds a tracked
file, or holds anything but regular files and directories. The pinned-commit
guarantee covers the tracked files; the declared outputs are the author's
bytes at publish time, under the same `SHA256SUMS.txt`. `install`, `update`
and `dev` ignore the key. The announcement prints `Inputs  pinned Git tree +
build outputs` when any are declared, plus one line per output with its file
count, total size and newest mtime.

The microphone can be switched off per installation, and a running capture
is visible in the window's title. `"revoked": {"media": {"microphone":
true}}` in an app's `data/config.json` — hand-edited, like
`port_override`, and kept across updates — makes the hub deny every capture
request with a logged reason and report `TFS_MEDIA_MICROPHONE=0` although
the manifest declares the microphone; a `config.json` that does not parse
fails closed. While a capture runs, the title reads `Microphone on —
<product name>` (`Microphone muted — <product name>` while muted) and
returns to the plain name when it stops or the web process dies
mid-capture; under Wayland the indicator also writes tao's header bar,
whose title a plain `set_title` cannot reach.

A web process killed before its window's first page ever committed gets the
crash page as well: the Reload link targets the URL the window was created
with (the splash's own URL, or the bundled fallback's), so an early engine
death has the same page in the app's colours and the same way back as any
other. `TFS_MEDIA_MICROPHONE` reports the microphone grant only once the
window's `with_webview` closure really wrote the setting and connected the
permission handler — the launch waits up to 2 s for that report before
starting the sidecar, and a timeout, a window closed first or a webview
without settings reports `0` with one warning line.

A set, delete, get, has or list that the keyring or the fallback file fails
answers `storage_failed` over IPC and `500 {"error": "storage_failed"}` on
the bridge, with one warning line in `hub.log` naming the cause (never the
key or the value). Every keyring call — the startup probe included —
carries a 5-second deadline, so a frozen Secret Service makes the launch
fall back to the file store (or answers an operation as a failure) rather
than holding the splash or a request forever. A corrupt `secrets.json` is
left byte-for-byte untouched rather than overwritten by the next write, and
`purge` reports a keyring delete it could not make as `FAILED (<cause>)`.

`install` and `update` take no working-directory argument: every install and
update starts from a checksummed release archive, from a forge or from
`publish <project> --local <dir>`. A directory argument is refused with the
`publish --local` route (and `dev <path>` for running a project in place).
An equal version is always refused — there is no `update --force` — and
`list` shows no "changed since install" marker. A registry that holds a
`local-path` entry is refused as a whole; there is no migration, since no
user exists yet.

Apps can be published to a local folder with `publish <project> --local
<dir>`. The folder contains a release archive, `SHA256SUMS.txt` and
`NOTES.md`. `install <archive.tar.gz>` verifies the adjacent checksums, and
`update <id> <archive.tar.gz>` applies a newer archive. Backups passed to
install or update, and releases passed to import, are pointed to the
appropriate command.

Two lifecycle guards hold under the registry's lock. Every lifecycle lease
holder except `repair` re-reads the update journal while it owns its lease,
so a command that wins its lease after an `update` left a journal behind
refuses and names `tfsapp-hub repair <id> --yes` instead of running over the
interrupted state. An install claims `apps/<id>/` atomically and re-checks
the app's `id` and pinned port under the registry's own lock, so two
installs with different identifiers cannot land on one directory, one
registry entry or one port; a refusal there removes the copied tree and
keeps the data directory.

Archive extraction refuses hard links, devices, fifos, sparse files and
other unsupported entry types. Release downloads, extraction and imports
check available disk space before writing large payloads; imports also cap
`manifest.json` at 1 MiB. Export streams database and upload files instead
of loading them into memory, and preserves their modification times.

Simultaneous `run` starts claim their own records before checking a
non-`concurrent` alias's exclusion, so they cannot both launch commands.
Scans preserve entries named for live launchers, including entries created
just before their lock is taken. `sidecar.pid` is replaced through a synced
temporary file, preserving the previous full pid list if a rewrite is
interrupted.

`tfsapp-hub repair <id>` is safe against a kill at any instant of an
interrupted `update`, not only right after one of its journal writes — the
gap between a filesystem change and the journal record of it is covered too.
It finishes rather than reverts an update that was killed after the
registry already recorded the new version: the update had already happened
at that point, so `repair` completes the rollback-point promotion instead
of undoing a commit that already landed, leaving `rollback <id>` usable
afterwards. Any other interruption restores the outgoing version exactly.

An interrupted `import` or `rollback` is recoverable too. `import` extracts
the archive into a staging directory before touching the live data, records
a durable intent, and only then switches the live database and `uploads/`
over through renames to reserved rescue names: a kill leaves a state the
hub names, and `tfsapp-hub repair <id>` puts back exactly what the import
replaced before its commit, or finishes the import after it. An import
whose extraction fails never touches the live data at all, and a failure
between the switch and the commit is backed out in-process. A `rollback`
killed partway through is finished by running `tfsapp-hub rollback <id>`
again, never rewound; until it finishes, every other command for that app,
`repair` included, refuses and names it. All three interrupted-operation
records — update journal, import intent, rollback marker — are checked by
every lifecycle lease holder and by the dispatch advisory check.

An app can declare `actions.paths` — one boolean per GLib special directory
(`desktop`, `documents`, `downloads`, `music`, `pictures`, `public_share`,
`templates`, `videos`) — and have the hub resolve each declared member
through `glib::user_special_dir` (the same file, `~/.config/user-dirs.dirs`,
GLib itself reads) and report it as `TFS_USER_<NAME>_DIR`. Like `media`,
this group's members name a resource rather than a transport, so
`ipc`/`bridge` are refused under it. A declared member GLib cannot resolve
reports as an absent variable — never an empty string, never a guessed
`$HOME`-based fallback. `$HOME` is deliberately not a ninth member: it
already reaches PHP through the ordinary process environment. No filesystem
grant and no existence guarantee either — PHP's rights are the same as
without the declaration, and a resolved path may name a directory deleted a
moment ago.

An app can declare `actions.media.microphone` and reach the microphone
through the ordinary web platform — `getUserMedia({audio: true})` on its own
page, never through IPC or the bridge, which is why the group has neither
transport (`ipc`/`bridge` are refused under it, like `picker.bridge`).
Undeclared, the webview keeps WebKitGTK's own default of no capture at all
— no dialog, nothing to retry. Declared, exactly two WebKit permission
requests are ever granted, both only on the app's own origin: an audio-only
capture (a combined audio+video request is refused as a whole) and a
device-info request, the second needed for `enumerateDevices()` to return
labels at all. Every other permission kind stays denied, whatever the
manifest says. `TFS_MEDIA_MICROPHONE` reports whether the app's page is
granted the request in the first place, never whether hardware answers or
a recording API works — the hub adds no audio stack, so an app should
expect `MediaRecorder` itself to vary by machine and fall back to Web
Audio, and must set its own CSP (`media-src 'self' blob:`) to play a
recorded clip back. No permission prompt ships — the declaration, readable
before install, is the whole of the consent story.

An app can declare `file_associations.mime_types` plus the
`actions.open_files` receiver and be handed local paths — through
`tfsapp-hub open <id> -- <path>...` or by choosing the app in the desktop
environment's "Open with" menu, its `.desktop` entry gaining `MimeType=`
and an `Exec=` ending in `-- %F`. One invocation is one opaque request in an
in-memory queue that exists before the splash: the whole batch is validated
first (every path a local existing regular file, plus existing directories
when the receiver opts into `actions.open_files.directories` — a directory
counts as one path, is never enumerated, and `inode/directory` in the MIME
list is what advertises the app for one in the file manager; the offending
path named on refusal), then delivered to the most recently focused
eligible window through a path-free `tfsapp://open-files-pending`
notification the receiver answers with
`open_files_pending`/`open_files_ack` — subscribe before the first read,
then read on notification only. Acknowledgement is idempotent removal and
delivery is replayable until it happens, so app-side acceptance must be
idempotent by request id; a reload re-exposes an unacknowledged one. The
queue is bounded (64 requests, 64 paths, overflow refused loudly, never
evicted) and not durable: process exit ends its lifetime, and the
single-instance handoff/shutdown race is diagnosed rather than guaranteed.
Apps that declare nothing are unaffected.

An app can declare `actions.close_guard` and register independent guards
for unsaved frontend work over IPC and background jobs over the PHP bridge.
Closing a protected window shows a native confirmation with Cancel as the
safe default; cancellation keeps the app running. Backend guards warn only
when closing would stop the shared backend. Both transports default to off.

Guards and window topology are rechecked when confirmation is answered. A
new relevant guard earns a fresh warning, and a document replaced while
the dialog is open invalidates the answer. Once approval is accepted, the
window is committed to close: a later reload does not cancel it. Window
reservations coordinate simultaneous closes, and normal close/signal
teardown has one owner. Mandatory shutdown does not wait for confirmation
or for an event-loop commitment; rare signal/window-creation overlap
remains an accepted limitation.

An app declaring `actions.picker.ipc` can restrict what the native file
chooser shows: `pick_path` accepts the same optional `filters` list as
`save_path` — one entry per filter, in the order given, the first one
active, extensions written without a leading dot. Omitted, `null` and an
empty list all leave the chooser unrestricted. A well-formed filter list on
a directory selection is ignored rather than refused, so callers can share
one options object between both kinds; a wrongly typed filter field still
fails the call before any dialog opens.

Restoring a backup does not keep settings cached from the previous
database. `import` clears the destination's `cache/` and `build/` and
discards its stamp before replacing anything — including when the archive's
version matches the installed app's, the one case where the cache stamp's
comparison cannot tell the compiled container it is stale, and the one that
would otherwise leave a restored app serving the previous database's theme,
language and provider settings. A `run` command executed right after the
import reads the imported database, and an older archive migrates forward
without the old cache; the fresh cache that migration builds survives the
import. A cache the import cannot clear stops it before any data is
replaced, naming the path.

An app declaring `actions.picker.ipc` can open a native Save As dialog
(`save_path`), with its own filters, suggested file name, starting
directory and GTK's own overwrite confirmation, resolving to the path typed
or to `null` on cancel — the hub never writes to it. A plain
`Content-Disposition: attachment` response saving into the OS download
directory is documented, definitive behaviour; `save_path` is the way to
offer a person a choice of destination instead.

Opening an installed app is measurably faster the second time, and every
time after that: `install` and `update` warm `cache/`/`build/` themselves,
right after their own lifecycle commands, and a launch reuses what they
built as long as nothing has moved under it. Measured on the reference app,
that is the difference between roughly 3 s and roughly 1.1 s per launch.
`cache/`/`build/` are still emptied — `hub.log` names why — whenever the
app's version, its installed path, or the hub's own PHP fingerprint no
longer matches what the cache was built against: after an `update`, after a
`rollback` (whose next launch rebuilds at the un-warmed cost, and every
launch after it until the next `install` or `update`), and after a hub
self-update that moves PHP under an app pending revalidation. Nothing an
app declares is involved, and CONTRACT.md §3's guarantee that
`APP_CACHE_DIR`/`APP_BUILD_DIR` may be emptied at any launch still holds —
the hub simply empties them less often.

A window whose web process dies — a crash, a memory limit, any end to
`WebKitWebProcess` the hub did not ask for itself — shows the hub's own
crash page instead of sitting on a dead page: `hub.log` records the reason,
and the page comes in the app's declared `splash_bg` / `splash_text` with
its `product_name`, with a Reload link that goes back through the same
navigation policy and page-load hooks as any other load. There is no
automatic reload and no crash-loop heuristic: the user's click is the only
way back, and the app is never told a reload followed a crash. A close
guard held by the document that crashed does not block closing the window.

The hub's own releases carry their compatibility record and their
provenance: the AppImage's `.versions.txt` is attached beside it and listed
in `SHA256SUMS.txt`, and the release notes end with a
`Built from <repo>@<sha>` line naming the exact revision the binary was
built from.

`rollback` restores the recorded source along with the version, tree,
database and source revision: after an update and a rollback, `list` and
`registry.json` name the tag, location and kind the restored version was
installed from, for a forge release and a local archive alike. A bare
`update <id>` after a forge → archive → rollback therefore queries the
forge again. The rollback anchor records the outgoing source, and
`rollback` prints the source change it is about to make; an anchor without
a recorded source keeps the current source and says so.
