## Exporting and importing

`export <id> <path>` writes a curated `.tar.gz`; `import <id> <path> [--force]
[--yes]` seeds an existing installation of the same app from one
(`portability.rs`, plan 022). Both share a guard and a shape with `install`
and `update` above rather than inventing their own.

An import reads `manifest.json` at the archive root before touching the data
directory. If it is absent and exactly one top-level directory directly holds
`tfsapp.config.json`, the archive is identified as a release and refused with
`install <path>` or `update <id> <path>` advice. An unrelated tarball keeps the
ordinary "not written by export" refusal. Conversely, install and update
recognise export's first `manifest.json` entry and point to `import`.

**The busy guard is the same probe `install` already made private, now
shared.** `lifecycle::data_dir_holder` — `is_owner_live` on `sidecar.pid`,
then `lifecycle::probe_run_lock` — moved out of `install.rs` so `export` and
`import` read the same two locks `install::check_data_dir_available` does,
rather than each re-deriving "is anything using this data dir". A live window
or an active `run` command refuses both commands, naming what holds it; a
data directory that does not exist yet is never busy, there being nothing yet
to guard.

**The database is copied raw, under that guard, never through SQLite.**
`export` streams `app.db` and whichever of `app.db-wal`/`app.db-shm`
exist — `lifecycle::DB_FILE_NAMES`, the same three files `snapshot_db` copies
for an update — straight to the archive. The guard is what makes this safe:
nothing has the files open, so there is no hot WAL to reconcile and no reader
to race. `VACUUM INTO` was not on the table either way — the hub links no
SQLite library — but even with one, it would buy consistency the guard
already provides for free.

**`uploads/` travels beside the database, under the same guard (plan 049 /
decision 006).** `export` walks `<data>/uploads/` recursively, sorted at every
level, and streams every regular file it finds under the archive's own
`uploads/` prefix. Each disk entry records its source file's modification
time, and the in-memory `manifest.json` has modification time zero. Two
exports of an unchanged tree differ only in the manifest's `exported_at`. A
symlink, socket or fifo is skipped and named on stderr rather than followed or
embedded; an absent `uploads/` is the ordinary case for an app that has never
written a file, not a skip and not an error.

**`--force` draws the same line `017` already drew for `update`.** It unlocks
exactly one refusal — a populated data directory, now read from either the
database or `uploads/` holding anything at all — and none of the other
three: not a foreign `identifier` (nothing to override), not an archive newer
than the installed app (writing a future version into `data/config.json`
would leave the next launch on `LifecycleDecisionError::Downgrade`, which has
no recovery path), not a window or `run` command holding the app (nothing to
override, wait or stop it instead).

**An accepted import clears the destination's `cache/` and `build/` and
discards its stamp before anything else changes (plan 052).** The stamp
compares the app's version, the installed path and the PHP platform — never
the database — so an equal-version restore changes nothing it can see, and
the container compiled from the previous database would otherwise keep
serving its stale settings (the Papermark report: theme, language and
provider configuration read from cache after a same-version restore). The
cleanup runs past the busy guard, the refusals and the confirmation, still
under the maintenance lease and before the rescue phase below — the first
thing the import changes, and the last step that can fail without anything
persistent having been touched. It is strict, unlike `Mode::Launch`'s
best-effort wipe: a directory it cannot remove stops the import in its own
error, naming the path and saying data replacement has not started, because
`Mode::Run` and `Mode::Install` deliberately clear nothing later and nothing
else would catch the leftovers. The stamp goes first, so a partial cleanup
cannot leave a stamp claiming the old container is still reusable. An
equal-version import leaves no stamp behind and warms nothing — the next
`open` rebuilds, exactly as it would for any unstamped data directory; an
older archive migrates forward with the old cache already gone, and the
fresh stamp its successful warm-up writes survives the import. `log/` and
`sessions/` are not part of it (§5 keeps the distinction between not
travelling and being cleared).

**An accepted import stages first and switches after, under a durable intent
(plan 064).** The archive's `data/` and `uploads/` prefixes are extracted
into `<data>/.import-transaction/` — nothing live is touched while it fills.
Only once the whole payload is staged does the import write
`import-transaction.json`, its intent record, through the same
temp-file-fsync-rename path the update journal uses; the intent names the
archive's version, the version record it found on disk, and one reserved
rescue name per live database member present plus one for a non-empty
`uploads/`. The names are reserved *before* the record is written, so the
record already names where everything will go — under the maintenance lease,
nothing else creates them.

**The switch is a short run of renames.** Each live database member moves to
the rescue name the intent reserved for it, a non-empty `uploads/` moves to
its own (an empty or absent one is simply removed), and then the staged
members move to their live places — only the `DB_FILE_NAMES` members ever
leave `staging/data/`, so anything else a forged archive put under `data/`
stays in staging and is removed with it. The rescues are renames, not copies:
nothing downstream needs the source gone the way the database's own removal
once did, and copying a tree that can be gigabytes would make every forced
import pay for a case nobody asked for. The announcement before
confirmation therefore names the unreserved `app.db.rescue-<timestamp>[-N]`
pattern, while the success output names the actual paths from the intent.
The version record and the forward migration follow the switch, the intent
is rewritten as committed, and only then does the import consume the
rollback anchor (`discard_rollback_anchor`, `discard_db_snapshot`,
`update::discard_tree`), remove the staging directory and remove the intent
itself — announced in the overwrite list ahead of the confirmation as the
honest price of replacing the data underneath an anchor.

**A failure before the commit is backed out in-process; a kill is resolved by
`repair`.** Every step between the intent and its commit is reversible,
because the rescues are renames that can be renamed back: an error from the
switch, the version record or the forward migration makes the import put the
database, `uploads/` and the version record back and clear the destination
cache again — the app opens on its previous data and nothing is left behind.
A kill leaves the staged intent where it is, and while it is present every
command but `repair <id>` refuses, naming an interrupted import; before the
commit, repair puts back exactly what the import replaced, after it, repair
finishes the cleanup. The one failure that needs no repair at all is a failed
extraction: it happens before the intent exists and touches nothing live, so
it is reported as such. A stale staging directory with no intent behind it
can only be an extraction that was killed, and the next import clears it
before extracting.

**An archive older than the installed app is migrated forward, right inside
import.** This is the plan's own ordinary case — restore a backup onto an
installation whose source has since moved on — and it very nearly shipped
without it: `lifecycle::check_version`, the guard every `open` runs, reads
only `data/config.json` and cannot tell "an update never finished" from "an
older archive just landed here" — it refuses either way, naming
`update <id>` as the way out. `update <id>` cannot actually rescue it, though:
its own decision compares the registry's `app_version` to the freshly
re-resolved *source*, never to the data directory
(`update::update_decision`'s own doc), so an unchanged source reads as
"nothing to update" regardless of what `data/config.json` says. Left
unhandled, importing the ordinary case would have left the app permanently
unopenable — caught only by running the end-to-end validation for real rather
than trusting the design read. The fix reuses `install::prepare` under
`LifecycleEvent::Update` — the exact event `update`'s own `Apply` runs —
against the *installed* manifest (reloaded from `apps/<id>`, never the
archive's own), once the switch has put the archive's database in place:
`pre-update` then `post-update` run over the freshly imported database, and
the version record lands on what is
actually installed, not on the archive's older one. An archive at the
installed version skips this — nothing to migrate — and one newer is already
refused before either path is reached.

The archive version is written to `data/config.json` only after the switch —
never while the live data still holds that path — and the forward
migration's own success point then overwrites it with the installed version,
once every hook has succeeded, exactly as `install::prepare` stamps any
event. A migration failure therefore never leaves a record it did not earn:
the back-out above restores the outgoing version record along with the data,
and the import reports that nothing changed.

**The export temporary is also protected from overwrite.** `write_archive`
uses the full target filename plus `.tmp`, refuses a pre-existing temp as a
manual-cleanup leftover from a failed export, and best-effort removes a temp
it created if a later write or rename fails.

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
Only regular files, directories and confined symbolic links are accepted;
hard links, devices, fifos, sparse entries and other tar entry types are
refused before extraction. The manifest is capped at 1 MiB. Before clearing
any destination state, import walks the archive and totals its regular-file
payload. It refuses a payload larger than the available disk space after a
64 MiB safety margin. The live database's size is no longer subtracted from
that budget: it stays in place while the staging directory fills, and the
rescue it will become is a rename, not a copy, so no room needs reserving
for it. The archive is then read again for extraction.

**Import never touches the keyring.** The archive carries no secret, so the
destination keeps whatever `APP_SECRET` it already had or resolves one fresh
on first use, exactly as an ordinary first launch does. `import` prints the
consequence rather than leaving it to be discovered — every session in the
imported database is invalid on the new machine, and `actions.secrets`
values must be re-provisioned there — CONTRACT.md §5 states the guarantee
this follows from.
