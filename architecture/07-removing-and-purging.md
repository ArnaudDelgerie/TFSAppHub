## Removing and purging

`remove <id> [--purge]` and `purge [<identifier>] [--yes]` are `remove.rs`,
two entry points sharing one execution engine (plan 023). `remove` acts on a
hub-local `id` still in the registry; `purge` acts on an `identifier` with no
registry entry left — the state a plain `remove` deliberately creates by
keeping data behind. Two different keys because there is no `id` left once an
app is removed: reusing `remove`'s own grammar for it would mean typing an
`id` that no longer exists.

**One `RemovalPlan`, two constructors.** `plan` builds the installed subject
— `id`, `app_dir`, everything `remove <id> [--purge]` has always touched,
plus the keyring accounts a live manifest declares. `plan_orphan` builds the
orphan subject — no `id`, no `app_dir`, only what is keyed on `identifier`
alone. Both feed `announce`/`execute`, which branch on `Option<id>`/
`Option<app_dir>` rather than duplicate the zone-by-zone deletion logic; the
desktop entry is the one zone with a real behavioural difference between the
two, not just an absent field — see below.

**The desktop entry: a marker check that has to work with no `id`.**
`desktop::remove` deletes `<identifier>.desktop` only if it carries the exact
`X-TFSApp-Id=<id>` line the hub wrote for that `id`; the orphan case has no
`id` left to check against, so `desktop::remove_any` accepts *any*
`X-TFSApp-Id=` line at all — the file is named after the identifier, so a
hub-written entry at that path is this identifier's to delete regardless of
which `id` wrote it. A hand-written or foreign file, carrying no marker, is
left alone by both.

**Four zones are keyed on `identifier`, and that is the ceiling of what
`purge` can reach.** `TFSApp/<identifier>/` (the data dir), `<OS data
dir>/<identifier>/` (WebKit's own website data, a sibling of `TFSApp/`, not
nested under it), the OS keyring accounts under service `<identifier>`, and
`<applications dir>/<identifier>.desktop`. Nothing code-side: `apps/<id>/`
and `apps/<id>.previous` are keyed on `id`, which no longer exists for an
orphan — an orphaned `apps/` tree is `install::DirectoryInTheWay`'s subject,
never `purge`'s.

**The enumeration behind bare `purge` reads `TFSApp/` only, never the OS data
dir.** `TFSApp/<identifier>/` is protected by its own parent — whatever
string arrives, the path stays under the vendor directory
(`paths::safe_segment`) — but the WebKit sibling is not: it is `<OS data
dir>/<identifier>/`, a neighbour of `applications/`, `gnome-shell/`,
`keyrings/` and `Trash/`, so a mistyped identifier there names a real system
directory. The existence of `TFSApp/<identifier>/` is what *proves* an
identifier is the hub's; `purge <identifier>`'s refusal 2 (nothing under
`TFSApp/`) is the same guard read a second time, at deletion rather than
listing time, so the WebKit sibling is only ever reached for an identifier
already proven this way.

**A symlinked data directory is refused, not followed.** Both the
enumeration and `purge_identifier` read `TFSApp/<identifier>` with
`fs::symlink_metadata`, never `metadata` — the call that proves a link
rather than following it. `fs::remove_dir_all` is already hardened against a
symlink escape (it descends with `openat`/`O_NOFOLLOW`), but an obscure I/O
error is not the same as a refusal that names the link, names its target,
and deletes nothing. The check and the deletion are two different instants,
so a link swapped in between them is a known, accepted non-goal (a TOCTOU
race) — an attacker able to write into the user's own `~/.local/share`
gains nothing from the trick they didn't already have.

**The busy guard is `lifecycle::data_dir_holder`, and `remove --purge` now
goes through it too.** Plan 022 built the shared probe for `export`/`import`
— `is_owner_live` on `sidecar.pid`, then `probe_run_lock` on `runs/`.
`remove --purge` used to probe only `sidecar.pid`, so an active `run`
command did not stop a purge from deleting the database out from under it;
routing it through the shared probe (plan 023 step 2) closed that gap
without a second definition of "who holds this data directory". `purge
<identifier>`'s own refusal 4 reads the same probe.

**The keyring cannot be enumerated, so `remove` leaves a note for `purge` to
read.** The `keyring` crate is point-to-point — an entry is read, written or
deleted by `(service, account)`, both known in advance — there is no API to
list a service's accounts. By the time `purge <identifier>` runs, the
manifest that declared `actions.secrets.keys` is gone with the snapshot. The
fix: a plain `remove` (never `--purge`, whose directory is going away)
writes `data/keyring.json` — `{ "accounts": [...] }`, atomically, at the last
moment it still has the manifest — into the data directory it retains.
`purge` reads it back, unions it with the two hub-owned accounts
(`APP_SECRET_ACCOUNT`, `PROBE_ACCOUNT`) and dedups. A data directory with no
note — an older hub's leftover, or a packaged station app's data, neither of
which ever writes one — falls back to the two hub accounts alone and says
so: `RemovalPlan.keyring_note_found` is what gates that caveat, dropped the
moment a note answers it. The limit is named, not silently under-purged.

**A keyring delete that fails is reported, not folded into "already clean".**
Each account's delete runs under the runtime store's deadline
(`secrets::with_deadline`, plan 068). A keyring that refuses or does not answer
is printed `FAILED (<cause>)` through the same `report()` line as a directory
that cannot be removed; "already clean" means only that no entry was there.
