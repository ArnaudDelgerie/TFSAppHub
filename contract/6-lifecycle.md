## 6. Lifecycle

An app declares, under `commands`, what has to run when it arrives on a machine
and when a newer version of it does. Both events belong to `install` and
`update`; a dev session goes through neither, and none of this section runs for
one — see §9.

```json
{
  "commands": {
    "pre-install":  ["doctrine:migrations:migrate --no-interaction"],
    "post-install": ["app:seed-defaults"],
    "pre-update":   ["doctrine:migrations:migrate --no-interaction"],
    "post-update":  []
  }
}
```

### The guarantee

**Installed-app lifecycle operations never overlap for one identifier.** The
hub holds a shared activity lease from an installed `open` child's preflight
until its window process exits, and throughout a foreground `run` command
(including its `--replace` transition; `run --stop` holds it while it resolves
and signals the run). It holds an exclusive maintenance lease from stateful
preflight through confirmation and the final write for post-manifest
`install`, `update`, `rollback`, `export`, `import`, `remove`, and `purge`.
A competing operation fails immediately, naming the maintenance operation when
one owns the lease; it never waits or begins a partial mutation. Different
identifiers have independent leases. The existing window and `runs/` probes
remain compatibility guards for older hub processes, not substitutes for this
no-overlap guarantee. A lease released by process death does not repair an
interrupted mutation; crash recovery is deliberately outside this contract.

**The commands of an event run once per install or update, in declared order,
before the user first sees the app, with §3's environment.** Every one of them
runs as `bin/console <args>` through the interpreter that serves the app, with
the app's own directory as the working directory. The first non-zero exit stops
the rest of the list and fails the whole event, naming the event, the command,
its exit status and where its output was captured.

*When* an event happens is the host's to decide, because only a host knows what
its own moments are. What the app is promised is the ordering and the
environment, not a wall-clock relationship to a window opening.

**A `post-` command may not assume its own app is reachable over HTTP.** This is
the one thing worth reading twice, because it is the clause most likely to be
assumed the other way. A `post-install` that curls its own routes, or that
expects a booted HTTP kernel to answer it, is outside the contract. Everything a
lifecycle command needs it must reach directly — the database, the filesystem,
the container — exactly as any console command does.

### The four events

| Key | Means |
| --- | --- |
| `pre-install` | This app is arriving on this machine for the first time. |
| `post-install` | Same occasion, after the `pre-` list. |
| `pre-update` | A newer version of this app is replacing an older one, over the same data. |
| `post-update` | Same occasion, after the `pre-` list. |

Which event a moment is gets decided by comparing the app's `app_version`
against the version recorded in its data directory. No record means install; a
newer version means update; an equal one means neither, and nothing runs. A
recorded version *newer* than the app being run is a downgrade: it has no
lifecycle event, and it is refused rather than guessed at — running old code
against data a newer version wrote is how a database gets corrupted quietly.

*(A host replacing itself is neither of these — the app's `app_version` has
not moved, so no event fires, ever. What a host may still do on its own
behalf, without an event, is re-resolve `composer.lock` against a PHP that
has changed underneath the app; that is a platform concern of §3's, not a
lifecycle one, and `ARCHITECTURE.md`'s to explain.)*

**The record is never written speculatively.** It is updated only after the
event's last command has succeeded, so a failure leaves the previous record — or
none at all, on a first install — in place, and the next attempt replays the
*whole* event rather than resuming from the command that failed. There is no
partial state to reason about.

**Both events run, on the moments the host decides.** Which event a moment is
gets decided against the data directory's own record — the one a plain
`remove` (without `--purge`) deliberately leaves behind. No record means an
install, and `tfsapp-hub install` executes `pre-install` then `post-install`,
in order, with the full environment and no server running, on a terminal
where a failure is legible. A record equal to the app's own version means
neither event, and install runs no lifecycle command over it — the ordinary
reinstall-after-`remove` path, where Composer and the version record are the
only things that still run. A record *older* than the app being installed is
the update event, and `install` refuses to run it in `install`'s own moment —
naming `tfsapp-hub update <id>` as the command that owns it instead. A record
*newer* than the one being installed is a downgrade, and `install` refuses it
exactly as a launch does, naming both versions and the directory: running old
code against data a newer version wrote is how a database gets corrupted
quietly, whichever host notices it first.

**What a plain `remove` keeps is not stranded.** `tfsapp-hub purge
<identifier>` is the guarantee behind the data it leaves: the data directory,
the same identifier's WebKit website data, and its OS keyring accounts are
all deletable later, by identifier alone, once no app is registered under
it — the retained-data path this section describes always ends somewhere,
never in a directory nothing in the product can clear.

**Neither form of `remove` runs while its data directory is held.** A live
window or active `run` command makes both `remove <id>` and `remove <id>
--purge` refuse, so neither can delete the installed tree or its database
under a process that is using it.

`tfsapp-hub update <id>` owns the update event, and with it the one guarantee
this section states on its own behalf rather than an app author's: **an
update must never leave the app's database between two versions.** It does
so by snapshotting the database before `pre-update` runs and reverting the
snapshot, the code and the registry together the moment any step fails — an
app either finishes the update it declared, or is left exactly where it
started, never partway through. If an on-disk undo itself fails, the hub still
attempts every remaining undo and names every failed path instead of claiming
the installation was put back.

A successful update leaves behind a **rollback anchor** — the outgoing
source tree and the pre-update database, kept rather than discarded —
until `tfsapp-hub rollback <id>` consumes it: putting the previous version's
code and database back together, in one generation, and setting the database
being left behind aside as a named rescue dump rather than deleting it. A
rollback is one step back; it leaves nothing behind to roll back a second
time.

**An interrupted update requires an explicit repair.** If the hub is stopped
after it has started an update or a forced re-sync, it retains a recovery
journal in the app data directory. Until the user runs `tfsapp-hub repair
<id>` (and confirms it, or passes `--yes`), `open`, `run`, `update`,
`rollback`, `export`, `import` and removal refuse for that app. What repair
does depends on how far the interrupted attempt got, at the one point that
divides its two halves cleanly: whether the registry already records the new
version. Before that point, the update has not really happened yet, and
repair restores the pre-attempt tree, database, version record and registry
entry — it does not resume the update or choose the partially installed
version. From that point on, the update *has* happened — the registry already
names it — and repair does not undo a commit that already landed; it finishes
promoting the outgoing tree and database into the ordinary rollback anchor
instead, the same one `rollback <id>` (see below) consumes, so a repaired
update can still be rolled back afterwards. Either way this remains one
generation only. The promise does not extend to interrupted imports or
rollbacks.

That install-time placement is better than it had to be. A migration and a cache
warm-up run once, while someone is watching a terminal that can print an error,
instead of inside a launch where every failure has to become a dialog.

### There are no build hooks

Two further keys existed on the archived per-app packaging route,
`pre-build` and `post-build`, running arbitrary shell on the developer's machine
during a build. They do not exist here and will not be added.

The hub has no per-app build step to hook into — that is the point of installing
from source with a bundled interpreter — and the boundary is deliberate:
**the developer builds on their machine; the host installs, serves and
restarts.** A host that ran an app's build commands would be a build tool with a
window attached, and every asset pipeline in the world already has a `--watch`.

Nothing is lost by it. Assets belong in the app's repository, built and
committed by whatever built them; `composer install` runs at install time with
the very interpreter that will later serve the app. If that interpreter later
changes, the next open re-runs it against the existing lock behind the app's
splash, after its launch locks are held. Those are the only build-like steps
the contract needs.

### Command strings are argv, never a shell

Each entry is a `bin/console` argument string, split on whitespace and passed as
`argv` directly. There is **no shell interpretation** — no pipes, no
redirection, no variable expansion, no `&&`. Two consequences, both intended:
a manifest cannot become a shell-injection vector, and only `bin/console`
commands can be declared, never arbitrary executables.

This is stricter than it would need to be if these commands only ever ran on
their author's machine. They do not: they run on a user's machine, from a
manifest that user did not write.

### Running a declared command

`run` aliases (§2) are not part of the install/update lifecycle above — they
are commands a user runs directly, interactively, in the foreground:
`tfsapp-hub run <id> <alias> [args...]`, alongside `run --stop <id>` and
`run --replace <id> <alias> [args...]` as the way to release one without a
manual `kill`. Four rules govern it.

**Rule 1 — the app layer must be up to date.** A `run` command refuses unless
the app's data directory records the version it is actually running — the
same check a launch makes. A missing record, an older one, or a newer one (a
downgrade) each refuse, naming the way out: open the app once, run
`tfsapp-hub update <id>`, or resolve the downgrade by hand. `run --stop` is
exempt — recovering an installation stuck on a stale app layer is one of its
own jobs, and stopping whatever holds the command's lock never touches this
record.

**Rule 2 — an app runs as many commands at once as its aliases permit.**
`<data dir>/runs/` (§5) holds one entry per launcher, named after the
launcher's own pid, each exclusively flocked by its owner and carrying the
alias and child pid it started. A flock held proves a live launcher; a flock
free but the recorded pid alive and identity-proven is an orphaned command
whose launcher died, refused exactly like a held flock; otherwise the entry
is stale and inert. Whether a newcomer alias may start is decided against
every entry the scan finds active: a `concurrent` newcomer (§2) stacks with
itself and with other `concurrent` entries, and is refused only by a
non-`concurrent` one; a non-`concurrent` newcomer refuses beside anything
active at all. A launcher records and locks its own entry before scanning,
then excludes that entry from its verdict. Two newcomers started at the same
instant may both refuse, but cannot both start when an alias is not
`concurrent`; rerunning after the refusal is the way out. Two apps running
commands at once costs nothing either way —
`runs/` is keyed on the app's own `identifier`, a different directory per
app. `run --stop <id>` stops every active command for that app; `run --stop
<id> <alias>` narrows it to that alias's instances. `run --stop`/
`run --replace` with no id list every active command across
every installed app. See `.project/decision/005-concurrency-belongs-to-the-alias.md`
for why the exclusivity moved from the command to the app layer's own
mutation.

**Rule 3 — the window's refusal is narrowed to its motive.** A `run` command
does not by itself keep a window from opening: a launch refuses over active
commands only when a non-`concurrent` one is live, or when the launch has a
lifecycle event to perform (an install or a pending update) — the launch path
is the only one that can show progress. A launch with nothing to run opens
beside every `concurrent` command, naming nothing; either refusal names the
blocking alias, its pid, and `run --stop <id>`. A `concurrent` command started
beside a window survives that window's closing — its lifetime belongs to
whoever started it, not to the window that happened to be open at the time.

**Self-contained-command boundary.** A `run` command gets everything the
server gets from §3 — `DATABASE_URL`, `APP_SECRET`, the writable directories
— re-resolved fresh for each invocation. What it does not get is the live
app's own HTTP endpoint: its own `APP_PORT` need not be a running window's,
so a command that needs to reach the app over HTTP is outside the contract.

**What `--stop`/`--replace` do not promise.** The `run` *command's own
process* is never force-killed, only what it started — `run --stop` against
one wedged somewhere other than waiting on its own child reports that the
lock did not release, rather than guessing at what to kill next, and a
multi-target `--stop` reports every target's own outcome rather than one
combined verdict, so one wedged command does not hide the ones that stopped.
A lock record with no pid yet — the narrow window between it being taken and
the command actually starting — is reported as such, not assumed to be
either free or wedged. `run --replace <id> <alias>` refuses outright on a
`concurrent` alias: there is nothing for it to replace when instances stack,
and the message points at starting another instance instead.
