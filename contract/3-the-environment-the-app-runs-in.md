## 3. The environment the app runs in

PHP never reads the manifest. Identity, paths, the port and the app's secrets
reach the app as **environment variables**, and this section is the canonical
list of them. Every process the hub starts on the app's behalf gets the same
list: the web server, the lifecycle commands at install time, a worker, a
console command. An app cannot tell them apart, and that is deliberate.

| Variable | Value | For |
| --- | --- | --- |
| `TFS_APP_IDENTIFIER` | `identifier` from the manifest, prefixed `dev.` in dev (§9) | the app's own identity |
| `TFS_APP_VERSION` | `app_version` from the manifest | the app's own version |
| `APP_ENV` | `prod` | Symfony |
| `APP_DEBUG` | `0` | Symfony |
| `APP_SECRET` | a per-app secret, generated once and kept — §5 | Symfony |
| `APP_PORT` | `app_port` if pinned, else a free loopback port for this launch | the web server |
| `APP_ORIGIN` | `http://127.0.0.1:<APP_PORT>` | Symfony, the web server |
| `APP_PUBLIC_DIR` | the app's `public/` — inside the installed snapshot, or inside the live project in dev (§9) | the web server's document root — replaced wholesale on update, rollback, or an install over an existing app; nothing durable survives here |
| `APP_CACHE_DIR` | a writable cache directory — see the lifetime note below | Symfony |
| `APP_BUILD_DIR` | a writable build directory — same lifetime note | Symfony |
| `APP_LOG_DIR` | a writable log directory | Symfony |
| `APP_SESSION_DIR` | a writable session directory, this app's own | Symfony |
| `APP_UPLOAD_DIR` | a writable directory for the app's own durable files — see the note below | the app |
| `DATABASE_URL` | `sqlite:///<app data>/data/app.db` | Doctrine, if the app uses it |
| `MESSENGER_TRANSPORT_DSN` | `doctrine://default` when `workers` is declared, `doctrine://default?queue_name=async` when only the legacy `async_worker: true` is, `sync://` otherwise | Symfony Messenger |
| `MERCURE_URL` | `<APP_ORIGIN>/.well-known/mercure` | Symfony, publishing |
| `MERCURE_PUBLIC_URL` | identical to `MERCURE_URL` — same origin, loopback | the browser, subscribing |
| `MERCURE_JWT_SECRET` | fresh random value every launch, never persisted | Symfony and the hub |
| `TFS_ASYNC_WORKER` | `"1"` / `"0"` — see "Capabilities are reported, not assumed" | the app, to tell its user |
| `TFS_WORKER_TRANSPORTS` | the transports actually consumed after fallbacks, in declaration order, deduplicated, comma-separated, empty when none | the app, to tell its user |
| `TFS_KEYRING_AVAILABLE` | `"1"` when the OS keyring answered, `"0"` when secrets fell back to a file — §5 | the app, to tell its user |
| `TFS_MEDIA_MICROPHONE` | `"1"` when `actions.media.microphone` is declared **and** the host actually granted it on this machine, `"0"` otherwise — see "Capabilities are reported, not assumed" | the app, to tell its user |
| `PHP_BINARY` | the interpreter actually running this app | any PHP tool spawning a PHP subprocess |
| `PATH` | prefixed so that `php` resolves to that same interpreter | the same |
| `TFS_BRIDGE_URL` | the loopback bridge's address — **present only when a bridge is running** (§7) | the app's HTTP client |
| `TFS_BRIDGE_TOKEN` | random hex, regenerated every launch, never persisted; present under the same condition | the app, as a bearer token |
| `TFS_USER_DESKTOP_DIR` | the resolved `desktop` user directory — **present only when `actions.paths.desktop` is declared and GLib resolves it** (§7) | the app |
| `TFS_USER_DOCUMENTS_DIR` | same, for `documents` | the app |
| `TFS_USER_DOWNLOADS_DIR` | same, for `downloads` | the app |
| `TFS_USER_MUSIC_DIR` | same, for `music` | the app |
| `TFS_USER_PICTURES_DIR` | same, for `pictures` | the app |
| `TFS_USER_PUBLIC_SHARE_DIR` | same, for `public_share` | the app |
| `TFS_USER_TEMPLATES_DIR` | same, for `templates` | the app |
| `TFS_USER_VIDEOS_DIR` | same, for `videos` | the app |

### The interpreter is discoverable

`PHP_BINARY` is how every PHP tool re-invokes "the interpreter running me" —
Composer's `@php`, Symfony Flex's auto-scripts, anything shelling out through
`symfony/process`. The hub guarantees it names the interpreter that is actually
serving the app, and that the same interpreter is first on `PATH`.

This is a guarantee rather than a detail because the failure mode is silent: an
app whose dependencies were resolved against one PHP version, quietly running a
piece of itself on whatever `php` the machine happens to have — or failing
outright on a machine with no PHP at all, which is the normal case for a
desktop user. An app may rely on `PHP_BINARY` being right.

### `APP_CACHE_DIR` and `APP_BUILD_DIR` may be emptied at any launch

The host may invalidate both at any launch, and the app **must store nothing
durable there**. Whether a given host reuses them across launches, and when it
decides not to, is its own affair — a compiled container is a derived artefact
and the contract will not promise its survival.

What the app is promised is the opposite side: both directories are writable,
both exist before any of the app's own code runs, and warming them is what the
install lifecycle is for (§6). Anything that must survive a launch goes to the
database, or to the app's data directory (§5).

*(The hub warms both once, at install and at update, and reuses them across an
installed app's launches as long as a stamp it keeps under `data/` — the app's
version, the snapshot path, and the PHP fingerprint the container was compiled
against — still matches; it empties them at a launch only when one of those
three has moved, which an update, a rollback or a hub self-update can each
cause. A dev session never wipes at all, so its container survives a relaunch
instead of paying for a full rebuild on every reload (§9). That difference is
a choice about safety and cost, not a property of this clause, and it is
`ARCHITECTURE.md`'s to explain.)*

### `APP_UPLOAD_DIR` is never emptied, at any launch, under any circumstance

The counterpart to the clause above: `APP_UPLOAD_DIR` is the one directory in
the data dir the app owns outright, and the host never invalidates it — not at
an ordinary launch, not when the cache stamp above mismatches, not on an
update or a rollback. A file an app writes there stays there until the app
itself removes it.

It is a sibling of `data/`, not a child of it (§5's layout), so nothing that
touches the database — the rollback anchor, the rescue-dump machinery — reaches
it either, with one accepted exception the rollback clause states on its own
(§5). `export`/`import` carry it alongside the database (§5); `public/`'s row
above is where a file put in the wrong place goes to be destroyed instead.

### The database is SQLite, and that is a constraint on the app

`DATABASE_URL` is the host's to set, in every mode, and it always names a SQLite
file. An app that declares its own in `.env` is not overridden so much as unheard:
a process environment variable wins over Symfony's dotenv files, by design and in
both directions of the argument.

So the app's schema, its queries and — above all — its **migrations must run on
SQLite**. That last one is the reason this is stated as a requirement rather than
left to be inferred from the DSN, because its failure mode is the worst shape a
failure can take: `doctrine:migrations:diff` emits DDL for the platform it was
generated against, so a migration produced on MySQL or Postgres is syntactically
fine, passes review, passes the author's own test suite, and then fails **at
install time on the end user's machine** — inside a `pre-install` hook, on a
terminal that user did not ask for.

Develop against SQLite and the whole class of problem is gone by construction,
which is why the host declines to offer anything else.

### The database is real, and it is migrated before the app opens

`DATABASE_URL` points at a SQLite file in the app's own data directory, which
survives updates and is what the rollback anchor is taken from. It is empty on a
fresh install: creating the schema is the app's own job, declared as a
`pre-install` lifecycle command (§6), and the hub runs it at install time —
before any window exists, on a terminal where a failure is legible.

A dev session never installs, so nothing runs `pre-install` for it and nothing
creates its schema automatically — the developer's own console does, against
the injected `DATABASE_URL`, exactly as below (§9).

An app author running the same project outside the hub gets whatever their own
`.env` names, which is a different file and correctly so. Point the console at
the injected value when you want the hub's database:

```console
$ DATABASE_URL="sqlite:///$(pwd)/var/data/app.db" \
    php bin/console doctrine:migrations:migrate --no-interaction
```

### Capabilities are reported, not assumed

Three variables exist purely so an app can tell its user something true about
this machine, and they are the standing rules' third clause in practice.

`TFS_KEYRING_AVAILABLE` says whether the secret store is backed by the OS
keyring or has fallen back to a file (§5). It reports what the probe actually
picked, not whether a keyring is installed: a keyring that is present but locked
has already fallen back, and an app is entitled to warn on that basis.

`TFS_ASYNC_WORKER` says whether at least one worker is consuming any of the
app's transports. Today the answer is exact and its scope is the window's
lifetime: declaring `workers` (or the legacy `async_worker`) gets one or more
workers for as long as the app is open, and closing the window ends all of
them. `TFS_WORKER_TRANSPORTS` names which transports specifically, so an app
with several declarations can tell a still-running one apart from one that
gave up (§6's supervisor). The rule that binds any future change to either
variable: they mirror **what is actually running**, never what the manifest
asked for. The day a host can be asked to narrow the declaration — or to
extend it past the window — the narrowing has to reach the app here, or an app
ships a task due at 8 a.m., is silently narrowed, and neither its author nor
its user ever learns it does not fire.

`TFS_MEDIA_MICROPHONE` says whether the app's own page is granted the
`getUserMedia({audio: true})` request in the first place (§7) — not whether
that call will actually succeed. It reports the grant, never the
hardware: a machine with no microphone at all still reads `"1"` once the
manifest declares it and the host installed the grant, because whether a
device answers is exactly what `getUserMedia()` itself already fails on, the
way it would on any web page. The AND that produces it is the same shape as
the other two — the manifest's own declaration on one side, what this launch
actually managed on the other — so a host that cannot install the grant
reports `"0"` rather than failing the launch (§8). The same AND makes a
locally revoked microphone read `"0"`: a microphone declared in the manifest
but switched off in this installation's `data/config.json` (§7) counts as
not granted, so the variable reports `"0"` although the manifest declares
it — the launch side refusing, not the declaration side lying.

Each `TFS_USER_<NAME>_DIR` variable above (§7, decision 008) follows the same
rule from the other direction: it is present only when its member is declared
**and** GLib's own `~/.config/user-dirs.dirs` resolution answers a path for
it. A declared member GLib cannot resolve reports as an absent variable —
never an empty string and never a guessed fallback path (§8).

