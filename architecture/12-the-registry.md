## The registry, and what a hub update does to installed apps

`registry.json` holds one entry per installed app plus a little about the hub
that last wrote it. Three properties it owes its callers:

- **A missing file is an empty registry, not an error.** "Nothing installed" is
  the overwhelmingly common state on a fresh machine.
- **Writes are atomic and serialised** — temp file in the same directory,
  `fsync`, rename, under an exclusive lock. A reader always sees one complete
  generation, which is why reading takes no lock.
- **Unknown fields survive a read-modify-write**, at both the top level and per
  entry. The same additive-growth rule the manifest's unknown-key warning
  serves, applied to the hub's own state: a user who tries a newer hub and steps
  back must not lose what it recorded.

There is deliberately no format-version field; the hub version already recorded
answers the same question without inviting a migration framework.

The registry is keyed on `id`, the hub-local handle; the data directory, the
keyring namespace, the WebKit data directory and the `.desktop` entry are all
keyed on `identifier`, the app's own. `install` enforces that mapping being
one-to-one (`check_identifier_free`, plan 016): a second `id` for an
`identifier` another entry already carries is refused before anything is
written, since the two installs would otherwise share all four.

Each entry's source kind can be `release`, `local-path` or `local-archive`.
For `local-archive`, `location` is the canonical path used at install or the
last update, for information only. There is no ref, reference kind or index;
`source_revision` hashes the extracted source tree. The archive may have
since moved, so `update <id>` requires a new archive path explicitly.

### The platform fingerprint

A hub self-update replaces PHP *underneath apps that are already installed*,
whose `composer.lock` was resolved against the old one. The one-app-per-binary
route never had this problem: one binary, one PHP, one app, updated together or
not at all.

So each app records the fingerprint it was installed against — PHP's
`major.minor`, because that is the granularity a lock's platform requirements are
written at, plus the loaded-extension list, because a missing extension breaks a
lock just as surely as a version bump. Both come from one `php-cli` call at
runtime; nothing is baked.

After a self-update, apps whose fingerprint differs are marked for revalidation:
`composer install` against the **existing lock**, lazily, on that app's next
use. The parent detects and announces the pending work, then returns with the
child pid; the child runs it behind the already-painted splash after it owns
the launch locks. Never `composer update` — re-resolving a dependency tree
during what the user experiences as a *hub* update is the worst possible
moment for a surprise.

**It is not a security check and not a version pin.** It authenticates nothing
and blocks no launch on its own. An app whose fingerprint differs is
revalidated, never refused.

### Self-update, reconciliation and rollback

`--update` (`hub_update.rs`) cannot compare fingerprints itself: after it
swaps the binary, the process still answering is the *old* one, running the
*old* FrankenPHP — only a later invocation, of the *new* binary, can ask that
question honestly. Reconciliation (`reconcile.rs`) is therefore deferred and
keyed on state already recorded: `registry.json` carries the `hub_version` of
whichever hub last wrote it, and every `Level::App` command (the same table
the CLI grammar's own dispatch reads) compares that string against its own
version before doing anything else. Equal — the overwhelmingly common case —
costs one string comparison and nothing more. Different, it probes PHP once,
marks every app whose fingerprint moved `needs-revalidation`, leaves an
already-`broken` one alone, and re-stamps. This survives a hub replaced by
any means other than `--update` too — a hand download, a distribution
package, a `--rollback` — where a hook on `--update`'s own success would
silently miss every one of them.

Revalidation itself (`revalidate.rs`) is what a marked app's next `open` runs
behind its splash — described above. The lock timing is intentionally only the
launch-facing seam here; audit 005 owns lock semantics themselves. None of
§6's lifecycle commands run alongside it: the app's own version has not moved,
so no event fires.

Two files must end up holding the new binary, not one: the stable copy under
`<hub root>/bin/tfsapp-hub`, which every generated `.desktop` entry's `Exec=`
names, and `$APPIMAGE`, wherever the user's own download happens to sit. The
stable copy is swapped first — every launcher on the machine silently
running the old hub, with nothing on screen to say so, is the worse of the
two possible partial failures, so it is the one `--update` protects against
finishing last.

The rollback anchor is the stable copy **renamed aside**
(`bin/tfsapp-hub.previous`), not a re-download: the hub already keeps a full
copy of what is running on the hub root's own filesystem, so renaming it
before the verified download replaces it is atomic, zero-copy and needs no
network of its own. A `registry.json.previous` snapshot is taken beside it,
under the registry lock, *before* the binary swap — a snapshot taken after
could already describe a state the new hub had begun to change. One
generation, like the per-app anchor `update <id>` leaves: a second
`--update` before a `--rollback` overwrites it rather than keeping two.

`--update` first swaps the stable copy. If placing the verified download at
`bin/tfsapp-hub` then fails, it immediately tries to rename
`tfsapp-hub.previous` back and removes the registry snapshot; a successful
undo leaves nothing changed. If that undo fails too, `bin/tfsapp-hub` is
missing and every generated launcher is down until repaired, but the complete
anchor remains: `tfsapp-hub --rollback` repairs that state.

`--rollback` (`hub_rollback.rs`) touches no network at all. Its precondition
— both anchor halves present — is checked before anything else, including the
confirmation prompt, so a half-written anchor from an interrupted `--update`
reads as "nothing to roll back to" rather than a network problem. It restores
the previous binary first. Only then does one registry operation take the
exclusive registry lock, read the live entries, compare them with the snapshot,
and write the selected result before releasing that lock. It is deliberately
not held through the confirmation prompt or binary swap, so ordinary app
commands never wait on a person or unrelated filesystem work. The registry
restore is a merge: when the entries read under that lock match the snapshot it
copies the snapshot byte-for-byte; when they differ, those locked live entries
follow the installed trees while only `hub_version` and the platform stamp
return from the snapshot. Thus app installs, removals and updates made since
the hub update remain coherent, and the report names the entries whose recorded
state was kept. This supersedes plan 020's historical claim that a rollback
always restores the whole registry exactly.

Once the registry is safely back, `$APPIMAGE` is restored — skipped when it
already names the stable copy, and noted rather than failed when the user's
own download no longer exists — and the anchor is consumed. A second
`--rollback` therefore finds no anchor rather than re-downloading an
already-restored version. If restoring or merging the registry fails after
the binary rename, the previous binary is nevertheless back and every
generated launcher already runs it; the registry was not restored and
`registry.json.previous` remains at its path for manual restoration. Retrying
`--rollback` is not a recovery path because its binary anchor half has already
been consumed.
