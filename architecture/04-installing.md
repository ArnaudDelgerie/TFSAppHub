## Installing

`install <source>` snapshots a project into the hub's root and makes it ready to
open. In order:

1. Resolve the source to a directory. A local path is used as it stands; a
   local `.tar.gz` archive is verified and extracted into scratch space, while
   a forge release (`github:owner/repo`) is downloaded first — see the release
   resolution below.
2. Read and validate the manifest. Missing required fields, a wrong type on a
   known key, or a non-canonical `app_version` all stop here, naming the file and
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

   The `id` and port gates here are only a *first* answer: the maintenance
   lease an install holds is keyed on `identifier`, so a second source
   installing under a different one shares no lease with this one, and both
   can pass these checks on a registry neither has written to yet. Step 9
   re-checks both under the registry's own lock — that is the answer that
   counts.
4. Decide which lifecycle event (`CONTRACT.md` §6) this install may run,
   against the version the data directory's own record holds — `None` means a
   first install, and only `record == app_version` survives past this point;
   a record naming a newer version is refused as a downgrade, and one naming
   an older version is refused as the update event, which `install` does not
   run (see below). The confirmation prompt names the record's version when
   one exists, so it is a question the user can actually answer.
5. Copy the tree to `apps/<id>/`, minus the app's own runtime droppings. The
   copy *claims* `apps/<id>/` atomically — one `create_dir` decides the
   winner, and an install that lost the race refuses with the directory
   intact rather than merging into the winner's tree.
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
   installed it, re-checking `id` and port inside `registry::update`'s own
   lock — an `id` or a pinned port that appeared since step 3 is a refusal,
   never a replacement of the entry that claimed it. A refusal there removes
   the copied tree (as a failed `prepare` already does) but keeps the data
   directory, as a plain `remove` does: a later install of the same app finds
   a matching version record and takes the reinstall path.
10. Write the version record into the data directory — **only** once every
    step above has succeeded, and every event, including an equal record
    rewriting itself.
11. Run `bin/console cache:warmup` through the same toolchain the lifecycle
    commands just ran with, and, only if it succeeds, write the cache stamp
    (plan 024 — "Opening an app", below, has the full mechanism). A failure
    here is a warning, not an install failure: the cache is a derived
    artefact, and the next launch rebuilds it the slow way instead.
12. Refresh the stable copy of the hub itself, and write the app's `.desktop`
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

1. **Name-check, then download**: the release tag must be
   `vMAJOR.MINOR.PATCH`, with a canonical app version after the transport
   prefix, and its one source archive must end in
   `-<version>.tar.gz`; only then is that
   `<project_name>-<app_version>.tar.gz` asset streamed to a file — never
   buffered whole in memory. The download stops when it exceeds available
   disk space minus a 64 MiB safety margin, even if the server omits or
   understates `Content-Length`. The tag supplies the version, but not yet
   the project name.
2. **Verify** it against the `SHA256SUMS.txt` asset beside it, hashed the same
   way. A missing line for the archive's own name is a failure, never a pass
   by absence. This establishes integrity, not publisher authenticity; the
   user chooses which source to trust ([decision 004](.project/decision/004-integrity-not-authenticity.md)).
3. **Extract**, only once the checksum has matched. A read-only pre-pass
   rejects unsafe paths, unsupported entry types (including hard links,
   devices, fifos and sparse entries), and a regular-file payload larger
   than available disk space minus a 64 MiB safety margin. Extraction then
   checks every entry again before writing it (no absolute path, no `..`
   component, no symlink resolving outside the extraction root), and the
   whole archive must hold exactly one top-level directory.
4. **Walk and confirm identity**: read the extracted manifest without
   reporting its warnings yet, refuse it unless its `app_version` equals the
   tag's version and its `project_name` equals the archive-name prefix, then
   walk the tree (`tree_hash`). `install` later loads the accepted manifest in
   its normal warning-reporting path.

Nothing about `apps/<id>/` is touched until all four have succeeded — a
refusal at any point leaves scratch removed by the caller and the app root
exactly as it was.

### Resolving a local archive

`source::resolve` canonicalises the `.tar.gz` path and requires a regular file.
It first reads the archive's first non-PAX entry: `manifest.json` marks an
`export` backup and is refused with `import <id> <path>` advice. It then
requires `SHA256SUMS.txt` in the same directory, hashes the archive and checks
the line named by its file name before extracting anything. The shared
extraction and manifest checks require one safe top-level tree, a canonical
version and an archive name matching `<project_name>-<app_version>.tar.gz`.
`--ref` cannot select a revision of a local archive. The registry records
`local-archive` and its canonical path, which is informational: later updates
must name a new archive explicitly.

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

**A declaring app's entry advertises what it can open.** An app whose manifest
declares `file_associations.mime_types` (§2) gets two more things in its
entry: a `MimeType=` line in the specification's list form — `;` between each
and one trailing — and an `Exec=` ending in `open <id> -- %F`, where `%F` is
a standalone, unquoted field code the desktop environment expands into
separate local-file arguments (§7's receiver meets them there). MIME types
are never part of app identity: they are a declaration of support, not a
default-application claim, and which app the desktop *prefers* for a type
stays the user's own setting. The desktop database is refreshed best-effort
after both a write and a real removal, so the association cache never
advertises an entry that is gone — and never touches a foreign entry, since
`update-desktop-database` rebuilds the cache from the directory's own
contents.
