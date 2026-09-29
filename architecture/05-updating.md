## Updating

`update <id> [<archive.tar.gz>] [--ref <tag>]` resolves the supplied
archive, or re-resolves the app's recorded source when none is supplied. An
app recorded as `local-archive` requires the explicit archive and refuses
`update <id>` alone with the needed command and last path. A supplied archive
can replace a forge source; the successful update records that new source.
When the resolved version is newer, update replaces the installed tree —
the update event (`CONTRACT.md` §6), and the guarantee `install`'s own
refusal defers: an update must never leave the app's database between two
versions.

Before trusting any mutable premise, `update` takes the installed app's
exclusive lifecycle gate. The gate lives at
`<OS data dir>/TFSApp/hub/locks/<identifier>.lifecycle.lock`, outside the
removable app-data directory, and its retained file handle lasts through the
prompt and every write. Once held, the registry is reloaded before the source,
action and data-directory guards are acted on. This prevents a second update
from removing the first update's `apps/<id>.previous` anchor.

1. Load the registry entry, resolve the supplied archive or re-resolve its
   recorded source (with `--ref` when appropriate), and validate it exactly
   as `install` does. `--ref` with an archive is refused.
2. Decide (`update_decision`) from the same `lifecycle_decision`
   `install`/`open`/`run` all read, applied to the registry's recorded
   `app_version` against the freshly resolved source's own:
   - a newer source → the update event;
   - an equal source → refused — there is nothing to update;
   - an older source → refused as a downgrade: update never goes backwards;
   - no record at all → refused; that moment belongs to `install`.
3. Guard the data directory exactly as `install` does (016's
   `check_data_dir_available`), then confirm.
4. Apply, in order: write a durable, outgoing-only transaction journal at
   `<data>/update-transaction.json`, snapshot `app.db` (+ `-wal`/`-shm`) into
   `<data>/.update-transaction/`, retain the outgoing tree at
   `apps/<id>.update-transaction`, copy the new tree in, empty the app's own
   cache/build directories
   (they are about to boot a container compiled from code that is no longer
   there) — `uploads/` is not a third: it is never emptied by an update, or by
   anything else the host does (`CONTRACT.md` §3) — run `install::prepare`
   under the update event (`pre-update` then
   `post-update`, then the same `cache:warmup`-and-stamp `install` itself
   runs — plan 024), then — only once every step above has succeeded — stamp
   the registry with the new one. Each mutation above — the snapshot's own
   bytes included — is synced before the journal write that records it, so a
   kill between the two never leaves a phase the journal claims durable
   without the file it describes. Only once the registry is stamped is the
   private retained tree promoted to the public one-generation
   `apps/<id>.previous` rollback anchor, its database snapshot and
   `rollback.json`; the journal is then discarded. That promotion
   (`finalise_anchor`) is itself replayable step by step — discard the old
   anchor and rename the tree, promote each database member, write
   `rollback.json` — so a kill partway through it resumes on the next repair
   instead of erroring or re-destroying what it already wrote.
5. Recovery reads the journal's phase, not the disk, for *what happened* — but
   for the one mutation a kill can land between and its own phase write, it
   reads the disk for whether that one step happened, never inferring a phase
   from it. Before the registry is stamped, a normal failure and a killed
   process both restore the same outgoing state: `repair --yes` restores the
   recorded outgoing tree, database, version record and registry entry, then
   removes the journal. It never chooses the partly-installed version, never
   resumes the update, and never rotates the public rollback anchor. Once the
   registry is stamped, the update *has* happened, and there is nothing left to
   revert — `apply` no longer attempts to; a failure past that point names
   `repair` directly. `repair --yes` there instead *finishes* the update:
   replaying the anchor promotion above until it is complete, so
   `rollback <id>` can then undo it.
   Either way, running `repair` a second time changes nothing. A malformed or
   future journal also refuses safely. Interrupted imports and rollbacks have
   recovery protocols of their own now — `repair <id>` puts an uncommitted
   import's replaced data back or finishes a committed one, and a resumed
   `rollback <id>` finishes a started rollback — and the same gate reads all
   three records, journal first, then import intent, then rollback marker,
   refusing on the first one it finds. And the refusal is not a race the hub
   leaves to luck: every maintenance and activity lease holder re-reads those
   records once it owns its lease — the journal and the intent exempt
   `repair`, the marker exempts only `rollback` — so a command that wins its
   lease after an `update` wrote its journal still refuses, naming `repair`.
   The dispatch-side check that answers before any command runs is only the
   early message; the one under the lease is authoritative.

**The rollback anchor is three halves, or none.** `rollback <id>` refuses
unless all three are present, naming whichever is missing:

| half | where |
| --- | --- |
| the retained source tree | `apps/<id>.previous` |
| the pre-update database snapshot | `<data>/data/app.db.pre-update` (+ `-wal`/`-shm` twins) |
| the outgoing registry state | `<data>/data/rollback.json` (`app_version`, `source_revision`, `source`, `created_at`) |

Rollback restores the outgoing version, tree, database, source revision and
source. The registry describes what is installed, as `update` already applies
to the tag, so the source comes back with the version it installed. After a
forge-to-archive update and rollback the recorded source is the forge again,
and a bare `update <id>` queries the forge again instead of requiring an
archive. `source` is a field of the third half, not a fourth: it is optional,
because an anchor written before it existed has none. Such a rollback keeps the
current source and says so, and the anchor stays complete.

The third half exists because the first two cannot answer what it does:
`source_revision` is hashed over the *source*, while the retained tree was
copied with `install`'s own excluded paths — recomputing it from the tree
would produce a different string that means nothing.

The anchor stays three halves, not four: `uploads/` is not snapshotted, so a
rollback can restore a database that no longer agrees with what an update
since left on disk there (decision 006). Copying a directory that can be
gigabytes on every update, to protect against a version that reorganises its
own files, was rejected as the wrong trade — that case is a `pre-update`
command's to handle, not the anchor's.

A rollback rescue-dumps the current database first, printing its timestamped
`app.db.rescue-<YYYYMMDDTHHMMSSZ>` path (with a numeric suffix on a collision),
then writes a durable `rollback-transaction.json` marker into the data
directory — after the rescue, before the first mutation — and only then
restores the snapshot as the live database, removes the current tree and
renames `.previous` back in its place, restores `data/config.json` and the
registry entry to the anchor's recorded version, and rewrites the desktop
entry. It then **consumes** the anchor — the snapshot and `rollback.json`
are discarded, and there is no new `.previous` to roll forward into. A
rollback is one step back; a `.previous` naming the version just left would
invite a "rollback forward" this plan does not define.

The marker makes the rest of the pipeline **resumable rather than
journaled**: every step from the restore on tolerates being run again, so a
kill partway through is finished by running `rollback <id>` again — with no
prompt, since the user confirmed the first run and the tree the new one
replaced is already deleted, so there is nothing to go back to — and never
rewound. Until the marker is gone every other command for the app, `repair`
included, refuses, naming `rollback <id>`. The one thing a resume cannot
recover is a tree lost from both ends — neither `apps/<id>` nor
`apps/<id>.previous` still a directory — which it reports as such instead
of guessing.

Anchor hygiene runs both directions, so a stale one never outlives the
install it belonged to: `remove <id>` takes `apps/<id>.previous` with it
alongside `apps/<id>` itself, and a fresh `install` discards any
snapshot/`rollback.json` it finds in the data directory it is about to write
into — a data directory a `remove` without `--purge` left behind can
otherwise carry a stale anchor into an unrelated later install, one that has
nothing of its own to roll back to.
