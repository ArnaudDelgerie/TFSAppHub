## Updating

`update <id> [--ref <tag>] [--force]` re-resolves an app's already recorded
source and, when it is newer, replaces the installed tree with it —
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
4. `Apply`, in order: write a durable, outgoing-only transaction journal at
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
   the registry with the new one and rewrite the desktop entry. Only then is
   the private retained tree promoted to the public one-generation
   `apps/<id>.previous` rollback anchor, its database snapshot and its
   outgoing registry record; the journal is then discarded.
5. A normal failure attempts that same outgoing restoration. A kill or crash
   leaves the journal deliberately: it is a recovery decision, not something
   the next command silently guesses. Every ordinary command for that app
   refuses and names `tfsapp-hub repair <id>`; `repair --yes` restores only
   the recorded outgoing tree, database, version record and registry entry,
   then removes the journal. It never chooses the partly-installed version,
   never resumes the update, and never rotates the public rollback anchor.
   A malformed or future journal also refuses safely. Interrupted imports and
   rollbacks have no such recovery protocol.

`ResyncOnly` has no lifecycle event or rollback-anchor rotation. It uses the
same journal and private retained tree, but no database snapshot; repair still
chooses the outgoing tree and registry entry. The existing public rollback
anchor is untouched until the re-sync is fully committed.

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

The anchor stays three halves, not four: `uploads/` is not snapshotted, so a
rollback can restore a database that no longer agrees with what an update
since left on disk there (decision 006). Copying a directory that can be
gigabytes on every update, to protect against a version that reorganises its
own files, was rejected as the wrong trade — that case is a `pre-update`
command's to handle, not the anchor's.

A rollback rescue-dumps the current database first, printing its timestamped
`app.db.rescue-<YYYYMMDDTHHMMSSZ>` path (with a numeric suffix on a collision),
then restores the snapshot as the live database, deletes the current tree and
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

