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

**A forced import atomically rescue-copies before it overwrites.** The database
it is about to replace is copied by `lifecycle::copy_rescue_dump` — the same
mechanism `rollback` uses for the database it cannot keep — to
`app.db.rescue-<YYYYMMDDTHHMMSSZ>` (or a numeric collision suffix). The helper
creates the selected target exclusively while it copies, retrying the next
suffix on a collision; an announcement before confirmation therefore names the
unreserved `app.db.rescue-<timestamp>[-N]` pattern, while outcomes name the
actual committed path. Once the copies are safe, import removes each live
`DB_FILE_NAMES` file before extraction, so an archive lacking a WAL cannot
inherit one from the database it replaces.

**A non-empty `uploads/` is rescued the same way, by rename rather than by
copy.** `lifecycle::move_rescue_dump_dir` follows the identical
`uploads.rescue-<YYYYMMDDTHHMMSSZ>` naming and collision rule, its path
printed beside `app.db.rescue-*`, but moves the directory aside instead of
duplicating it: nothing downstream needs the source gone the way the
database's own removal does, and copying a tree that can be gigabytes would
make every forced import pay for a case nobody asked for. The archive's
`uploads/` entries are then extracted into the fresh directory with a second
`archive::extract_prefix` call — an archive with none, every one written
before this guarantee existed included, simply leaves it empty rather than
merging. Every `import_incomplete` error from this point on names both rescue
paths, not only the database's.

**A forced import also discards the rollback anchor, both halves.** The
anchor pairs `apps/<id>.previous` with the `.pre-update` snapshot; seeding
foreign data over the snapshot half would leave a `rollback` that restores
last version's code onto a database that never ran it, and `anchor_state` has
no way to notice the mismatch. So the import discards both
(`discard_rollback_anchor`, `discard_db_snapshot`, `update::discard_tree`)
before it extracts, announced in the overwrite list ahead of the
confirmation — losing the ability to roll back is the honest price of
replacing the data underneath it.

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
archive's own), right after extraction: `pre-update` then `post-update` run
over the freshly imported database, and the version record lands on what is
actually installed, not on the archive's older one. An archive at the
installed version skips this — nothing to migrate — and one newer is already
refused before either path is reached.

The archive version is written to `data/config.json` immediately after
extraction, before this forward migration. Thus a migration failure reads as
an unfinished update on the next `open`, which refuses it; it never lies that
the installed version's migration completed. Every error after the rescue is
wrapped with its underlying reason, the resulting data-directory state and the
full rescue path, rather than leaving a user to infer which database is safe.

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
64 MiB safety margin and the live database's size, preserving room for the
database rescue copy. The archive is then read again for extraction.

**Import never touches the keyring.** The archive carries no secret, so the
destination keeps whatever `APP_SECRET` it already had or resolves one fresh
on first use, exactly as an ordinary first launch does. `import` prints the
consequence rather than leaving it to be discovered — every session in the
imported database is invalid on the new machine, and `actions.secrets`
values must be re-provisioned there — CONTRACT.md §5 states the guarantee
this follows from.
