## 5. The app's own state, and what it is isolated from

### One data directory per app, keyed by its identifier

```
<OS data dir>/TFSApp/<identifier>/
  data/
    app.db          the SQLite database DATABASE_URL points at
    app.secret      APP_SECRET in plaintext — only on a machine with no keyring
    secrets.json    the app's declared secrets (§7), same condition
    config.json     which version last wrote all of this, plus the hand-edited
                    keys the host never writes on its own: port_override (§8)
                    and revoked (§7)
  cache/            APP_CACHE_DIR — may be emptied at any launch (§3)
  build/            APP_BUILD_DIR — same
  log/              APP_LOG_DIR — persists, rotated
  sessions/         APP_SESSION_DIR — persists
  uploads/          APP_UPLOAD_DIR — the app's own durable files, never emptied (§3)
```

`log/` is `APP_LOG_DIR`, and it is not the app's alone: the host writes its own
rotated logs into the same directory — `sidecar.log` (FrankenPHP's own
structured output, and nothing else), one `worker-<n>.log` per declared
worker slot (that consumer's `messenger:consume` output plus the host's own
supervision of it, `n` its 1-based slot number), `commands.log` (declared
lifecycle commands, §6) and `hub.log` (`open`'s own routine output, for a
launch with no terminal to read it — plan 015). An app that names one of its
own files `sidecar.log`, `commands.log`, `hub.log` or `worker-<n>.log` loses
it to a rotation it never asked for; pick something else.

This is the installed layout. A dev session's equivalent lives under the
project's own `var/` instead of an OS data directory, keyed by `dev.<identifier>`
rather than `identifier` — see §9, which is also where the isolation guarantees
below are shown to hold for it exactly as they do here.

The path derives from `identifier` **alone**. Not from where the host binary
lives, not from the handle you type, not from the source the app was installed
from. That is a guarantee and not an implementation accident: it means the app's
data belongs to the app rather than to whatever put it on this machine, and
reinstalling from a different source, or arriving by a different route, opens
onto the same database, the same secret and the same sessions.

The identifier directory is `0700` and `app.secret`, where it exists at all, is
`0600` — enforced on every launch rather than only at creation, so an older
installation is tightened on its next run. `app.db` is the app's to create; the
`0700` directory is what keeps it away from other local users.

### `APP_SECRET` persists, and where it lives depends on the machine

`APP_SECRET` is resolved once and reused on every subsequent launch, so signed
values — CSRF tokens, remember-me cookies, signed URIs — keep validating across
restarts. It lives in the OS keyring when one is reachable, probed with a
throwaway round-trip before any real secret is touched, and falls back to a
plaintext `0600` file when the probe fails. `TFS_KEYRING_AVAILABLE` (§3) reports
which happened, for this launch. None of this runs for a dev session, whose
`APP_SECRET` is a fixed constant instead — see §9.

Two consequences an app author should know rather than discover:

**The fallback file is deliberately not encrypted.** With no keyring reachable
there is nowhere secure to keep an encryption key, so encrypting it would be
theatre. Degraded but functional beat silently losing the secret on every
restart.

**A backend flip resets the secret.** If a launch cannot reach the keyring that
held it — or the reverse, after a launch that had fallen back — the other
backend generates a fresh one, and everything previously signed stops validating
once. This is rare, non-destructive (the user logs in again) and accepted rather
than papered over.

A keyring that does not answer the startup probe within 5 s counts as
unreachable, the same as a failed probe. And declared secrets (§7) are visible
only from the backend that stored them: a launch that fell back to the file
does not see what the keyring holds, and the reverse.

A pre-existing plaintext secret is migrated into the keyring on the first launch
that can reach one, and the file is deleted in the same launch once the write is
confirmed. Keeping it "just in case" would defeat the keyring.

### Sessions are per-app, if the app opts in

`APP_SESSION_DIR` is this app's own, and it persists. Using it is the app's
choice:

```yaml
framework:
    session:
        save_path: '%env(APP_SESSION_DIR)%'
```

The host provides the directory and the variable; it never patches the app's
configuration. The same is true of `APP_CACHE_DIR`, `APP_BUILD_DIR` and
`APP_LOG_DIR`.

### Cookies do not leak between apps

HTTP cookies are keyed by host only — the port is ignored — so two apps on
`127.0.0.1` would trample each other's session cookie if their webviews shared
one cookie store. They do not: each app's webview storage is keyed by its
`identifier`, so app A's cookies never reach app B's backend. With per-app
session directories as a second, server-side layer, even a cookie that did
arrive would find its session file in a directory it cannot name.

This is verified end to end rather than asserted: two apps differing only in
their identity fields, opened side by side, logging in to one leaves the other
anonymous, in both directions, with separate cookie files and separate session
files on disk.

### One live app per identifier, whoever launched it

Two servers writing one SQLite file is the failure mode this rules out. The
guarantee is stated in terms of the app, not of the program that started it:
**at most one live instance of a given `identifier` on a machine at a time**.
A second launch does not get a second server — it surfaces the app that is
already running.

That holds through the handover, too, not only on either side of it: a launch
arriving while the live instance is shutting down does not surface it — a
process mid-teardown is not "already running" in the sense this guarantee
means — and it does not race it for the data directory either. It waits,
bounded, for the shutdown to finish, then starts a new one. An app author
reads the whole guarantee as one sentence: there is never a second writer, and
a relaunch is never handed a backend that is already on its way out.

Stating it that way matters because "whoever launched it" is not hypothetical.
An app installed here and the same app arriving by another route resolve to the
same identifier, hence to the same data directory, hence to the same locks; the
hand-off between two *different binaries* was measured, and it holds. An app may
rely on there never being a second writer on its database.

### Secrets are namespaced, not isolated — read this before storing one

Each app's secrets go into the OS keyring under its own `identifier` as the
service name. **That keeps two apps from colliding. It does not keep them from
reading each other.**

The Secret Service authorises per login session: any process running as this
user can list and read any service's entries. This is not something the hub
introduces and not something it can remove — the same read succeeds between any
two applications on the desktop that use the same keyring. Key prefixing would
not help, because a reader lists entries rather than guessing their names.

Where the host *is* strict is in what it hands to the app's own code: a webview
reaches its secret store through the window it belongs to and can never name
another app's. There is no "give me app X's secret" call, and there will not be
one. What no host can prevent is an app asking the keyring directly, exactly as
any program on the machine can.

The only real fix is sandboxing the processes, which is a change of distribution
format and is not on the roadmap. Read per-`identifier` storage as tidiness, not
as secrecy, and do not store in it something whose disclosure to another
application on the same machine would be a breach.

### An installation moves between machines — the database is what travels

`tfsapp-hub export <id> <path>` and `tfsapp-hub import <id> <path>` carry one
installed app's data to another machine, or to a fresh install on the same
one. The guarantee names exactly two things: **the database travels, and so
does `uploads/`; nothing else in the data directory does.**

`cache/`, `build/`, `log/`, `sessions/` do not travel — each is either
machine-specific or gets regenerated on the destination's next launch anyway.
Neither does `secrets.json`, the plaintext keyring fallback: it holds
`APP_SECRET` and every `actions.secrets` value in the clear, and shipping it
in a file people copy around would turn a convenience into a disclosure. An
app must not treat anything outside its own database and `uploads/` as
portable state — a value stashed in `cache/` or read back from a file it wrote
beside the database is not carried by an export, whatever survives a
reinstall on the same machine.

`uploads/` travels the same way the database's raw bytes do: `export` walks it
recursively and writes every regular file it finds under the archive's own
`uploads/` prefix; a symlink, socket or fifo is skipped and named on stderr
rather than followed or embedded. `import` replaces rather than merges — an
archive carrying no `uploads/` entries at all, which is every archive written
before this guarantee existed, simply leaves the destination's `uploads/`
empty, its previous contents rescued aside exactly as below.

**An accepted import clears the destination's `cache/` and `build/` before
replacing anything.** The compiled container is *derived* from the database,
and the stamp that vouches for it compares the app's version, the installed
path and the PHP platform — never the database itself. An archive restored
over the same version therefore changes nothing the stamp can see, and without
this clause the destination would keep serving settings cached from the
previous database. Import discards the stamp and empties both directories
before any forward-migration command runs, at equal versions just as much as
at older ones. The `log/` and `sessions/` halves of the "do not travel" list
above stay exactly where they were: not travelling and being cleared are two
different promises, and only the derived pair is cleared.

**The rollback anchor stays asymmetric.** It snapshots `app.db` and nothing
else, so rolling back to the previous version restores a database that may no
longer agree with what is on disk in `uploads/` — a row pointing at a file a
newer version renamed or removed, for instance. Snapshotting a directory that
can be gigabytes on every update was rejected as the wrong trade; an app that
reorganises its files across a version bump handles that itself, in a
`pre-update` command.

Two consequences an app author should know rather than discover. **The
destination's `APP_SECRET` is never part of the transfer** — it keeps
whatever secret the destination already had, or resolves one fresh exactly as
any first use does (see above). Every session and remember-me token in the
imported database stops validating there, the same one-time reset a backend
flip already causes, and the app should treat a login prompt right after an
import as expected rather than a bug to chase. **`actions.secrets` values do
not travel either** — they live in the OS keyring under the destination's own
identifier (§7), untouched by an import, and must be re-provisioned there by
whatever provisioned them the first time.

**Export never overwrites either name it might encounter.** It writes beside
the requested target as the complete filename plus `.tmp` — for example,
`backup.tar.gz.tmp` — then renames that file into place only on success. An
existing target is refused, and so is an existing temporary file: the latter
is treated as a leftover from a failed export and must be removed by hand,
never silently clobbered. A failed export best-effort removes only the temp it
created itself.

**An import stages first, and touches nothing live until the archive is
whole.** The archive's database and `uploads/` are extracted into a staging
directory inside the data directory; a failed extraction leaves the
installation exactly as it was.

**A forced import keeps what it replaces, by rename.** Each database file
being replaced — `app.db`, and `app.db-wal`/`app.db-shm` when present — is
moved aside as `<name>.rescue-<YYYYMMDDTHHMMSSZ>` (with a numeric suffix for a
same-second collision), and a non-empty `uploads/` as
`uploads.rescue-<YYYYMMDDTHHMMSSZ>`; only then do the staged files take their
places. The rescue names are reserved before anything moves, so consecutive
forced imports never overwrite an earlier rescue, and the replaced data's WAL
never survives beside an archive that carries only `app.db`. A successful
import prints the database's rescue path and, when one was taken,
`uploads/`'s.

**An archive older than the installed app is migrated forward inside the
import**: the installed version's `pre-update` then `post-update` (§6) run
over the imported database, and the version record ends on the installed
version. An archive newer than the installed app is refused.

**A failed import changes nothing.** If the switch, the version record or the
forward migration fails, the import moves everything back — database,
`uploads/`, version record — and says that nothing was changed. If the hub is
stopped partway instead, every command for that app refuses until
`tfsapp-hub repair <id>` puts the replaced data back, or finishes an import
that had already committed (§6).

