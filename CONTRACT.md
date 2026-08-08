# The TFSApp contract

This document defines what **TFSAppHub** (the hub) and a **Symfony application**
promise each other. An app that honours it can be installed by the hub and
opened as a desktop window, with no fork of the hub and no per-app build step.

It is written from two sides and only those two:

- **What the app must provide** — a layout, a manifest, one HTTP route.
- **What the hub guarantees in return** — an environment, a place for the app's
  data, isolation from other apps, a lifecycle, and a set of native capabilities
  the app may declare.

Everything else has another home. *How* the hub does any of this — the installer,
the launch sequence, the process supervision, the bundled interpreter — is in
[`ARCHITECTURE.md`](ARCHITECTURE.md). What a *user* types is in
[`README.md`](README.md). A clause belongs here only if an app could be written
differently because of it; if it merely describes the hub's machinery, it is in
the wrong file.

The audience is an app author. You should be able to read this document alone
and know exactly what to build, without learning that FrankenPHP exists.

---

## Standing rules

Three rules govern every clause below. They are stated once, here, because they
are the reason the rest of the document can be read as a promise rather than as
a description of one program's behaviour.

**Warn on an unknown key, never refuse.** The hub reads the manifest and warns
about a top-level key it does not know, naming it — and then carries on. This is
what lets the schema grow additively, and what stops a hub older than an app
from being unable to run it. The corollary binds just as hard: a *known* key
with the wrong type is rejected, naming the file and the field. A quoted
`"async_worker": "true"` is not a forward-compatible extra, it is a manifest
that means the opposite of what its author believes.

**Fall back to the nearest honourable value, and say so.** A host that cannot
honour a value the app declared does not fail and does not silently ignore it:
it uses the closest thing it can, prints what it did, and makes the effective
state readable by the app at runtime. The app author declares the maximum once;
what actually happened is legible, at the terminal and in the injected
environment. A degradation nobody can observe is a bug, not a fallback.

**What is exposed is a capability, never a host.** The app is told what this
machine can do — whether the OS keyring answered, whether work continues once
the window closes — never what launched it. The intended use is informative:
warn the user, do not branch the domain logic. Branching on a *capability* is
fine and portable; branching on "am I running under X?" tests the wrong thing
and breaks the moment the app is deployed anywhere else.

That third rule has a practical edge worth stating separately, because it is
where app authors go wrong most often. Most of what the hub provides is
**ordinary Symfony infrastructure, not a hub-only capability**: a database, a
Messenger transport, a Mercure hub, a session directory. Every one of them has a
perfectly good non-hub equivalent, and the hub's role is only to provision one
and point the app at it. Code consuming them should read as it would in any
other deployment; when the hub is not there, the answer is *configuration* — a
`.env` for that environment, a real hub, a test double. The hub does also expose
things with no non-hub equivalent — the native bridge, the secret store — and
those genuinely have to be probed before use. **Probe the capabilities;
configure the infrastructure.**

---

## 1. What an app must provide

A TFSApp is a Symfony project directory. The hub is pointed at one — a local
path, or a git repository it clones — and that directory must contain:

```
path/to/project/
  bin/console            <- required
  public/index.php       <- Symfony front controller, the document root
  tfsapp.config.json     <- the manifest, §2
```

Plus one HTTP route, `GET /healthz` → `200`, covered in §4.

There is no directory to create and no file the hub writes into the project;
installing does not modify the source it was pointed at.

**Plus one obligation in the app's kernel.** Symfony decides where its cache,
its build artefacts and its logs go, and it decides it *inside the project* by
default — which is the installed snapshot, replaced on the next update. So the
app must honour three of §3's variables in its kernel:

```php
public function getCacheDir(): string { return $_SERVER['APP_CACHE_DIR'] ?? parent::getCacheDir(); }
public function getBuildDir(): string { return $_SERVER['APP_BUILD_DIR'] ?? parent::getBuildDir(); }
public function getLogDir(): string   { return $_SERVER['APP_LOG_DIR']   ?? parent::getLogDir(); }
```

Extending `ArnaudDelgerie\TFSAppBundle\Kernel\TFSAppKernel` is the supported way
to get exactly that, and what the reference app does. Writing the three overrides
by hand satisfies the requirement just as well — the contract binds the
behaviour, not the class. What it does not tolerate is neither: the host would
inject the three variables and the app would silently ignore them, writing into
a directory an update replaces.

**The manifest is read by the hub, never by PHP.** The app learns its own
identity from the environment (§3), not by parsing its own
`tfsapp.config.json` — so the same code runs unchanged under any deployment
that sets those variables. An app that reads the manifest at runtime has coupled
itself to being installed, which is exactly what §3 exists to avoid.

**Frontend assets ship built.** The host resolves PHP dependencies with its own
interpreter at install time; it does not run Node, and it never builds assets.
An app with a frontend build step commits its output, or its pages arrive
without it. This is the same boundary the lifecycle draws in §6 — the developer
builds on their machine, the host installs and serves — and it is what keeps an
app's prerequisites down to "a Symfony project".

**What is in the directory is what gets installed.** Installing from a local
path snapshots the tree as it stands, so a `.env.local` full of development
overrides is snapshotted with it. Keep the source you install from clean of
anything you would not commit.

**The app's own source is never where its data lives.** The project directory is
read-only from the app's point of view: the database, cache, sessions, logs and
secrets all go to a per-app data directory the hub provides and names in the
environment (§3, §5). A project that writes into its own `var/` will find that
directory belongs to the installed snapshot, and that an update replaces it.

---

## 2. `tfsapp.config.json`

A JSON file at the project root. It is the single source of truth for the app's
identity and for what it declares.

### Required fields

| Field | Type | Meaning |
| --- | --- | --- |
| `product_name` | string | The human-readable name. Feeds the window title and the desktop entry's `Name=`. |
| `identifier` | string | Reverse-domain technical identity, e.g. `dev.local.myapp`. Everything the operating system keys per app derives from it — see "One identity, several surfaces" below. |
| `project_name` | string | Machine-friendly slug for the project. |
| `app_version` | string | The app's own release version. Must be canonical semver — see "`app_version` is semver, and it is load-bearing" below. |

An app that declares none of these has no identity, and every downstream
decision — where its data lives, which keyring namespace is its own, what the
window manager thinks it is, whether a given source is an install or an update —
hangs off one of them. All four are refused if missing or empty.

`project_name` is the odd one out and it is worth being explicit: on the hub it
is **not** an identity key. The handle you type on the command line is assigned
by the hub at install time and recorded in its registry; it is derived from the
project but does not have to equal this field. `project_name` remains required
because a project without a slug has nothing to derive from, not because
anything downstream is keyed on it.

### Optional fields

| Field | Type | Meaning |
| --- | --- | --- |
| `app_port` | integer or null | Pins the app's loopback port instead of taking a fresh free one each launch. Absent (the default) is dynamic, and dynamic is the right answer unless something outside the app must know the port in advance. |
| `icon_path` | string | Project-root-relative path to one square source PNG (1024×1024 RGBA recommended, ≥512 wanted) used as the app's launcher, switcher and window icon. Absent keeps a placeholder. |
| `splash_path` | string | Project-root-relative path to one self-contained HTML file (inline CSS and JS only, no external assets) shown while the app cold-starts. **Not honoured today** — see §8. |
| `splash_bg` / `splash_text` | string, `#rgb` or `#rrggbb` | Recolour the cold-start page's background and text without authoring one. Either or both; an unset one keeps the default. |
| `commands` | object | Lifecycle commands the hub runs around an install or an update — §6. |
| `run` | object | Named `bin/console` aliases a user can run directly. |
| `actions` | object | Which native capabilities the app's own code may reach, and over which transport — §7. |
| `async_worker` | boolean | Declares that the app has work to consume off the request cycle — see "Declaring off-window work" below. |

A minimal manifest is four lines:

```json
{
  "product_name": "LabelBoard",
  "identifier": "dev.local.labelboard",
  "project_name": "labelboard",
  "app_version": "1.4.0"
}
```

### One identity, several surfaces

Several things the operating system shows the user are fed from this file, and
each surface is fed by exactly **one** field. Stating which is not pedantry: a
name that reaches two surfaces through two different routes is a name that
eventually differs between them, and a user meets that as "the window says one
thing and the dock says another".

| Surface | Fed by |
| --- | --- |
| Window title | `product_name` |
| Desktop entry `Name=` | `product_name` |
| Window class (`WM_CLASS`), GTK application id, D-Bus name, single-instance key | `identifier` |
| Data directory, keyring namespace, cookie store | `identifier` |
| The handle you type on the command line | assigned at install, recorded in the hub's registry |

One consequence surprises people the first time and is not a defect. Until an
app has a desktop entry, a desktop environment has no `Name=` to read and falls
back to a label derived from the window class — which is the identifier. So an
app with no entry installed can appear in the window switcher as
`dev.local.labelboard` rather than as *LabelBoard*. The fix is the desktop
entry, never the window class: the class has to be the identifier or nothing can
match a window to its own app.

### `app_version` is semver, and it is load-bearing

`app_version` must be canonical `MAJOR.MINOR.PATCH` — no leading zeros, no
suffix, no `v`. This is not a stylistic preference. It is the value the hub
compares to decide whether putting a given source on this machine is an
install, an update, or a downgrade to refuse, and a value that cannot be
compared makes that decision unanswerable.

The hub **refuses at the transitions**: installing or updating an app whose
`app_version` is not canonical semver fails, naming the file, the value, and
what the value is for. It does **not** re-check when opening an app that is
already installed — an app that passed the gate on its way in should not become
unopenable later, and re-refusing it would make the hub reject something it
itself accepted.

Bump it when the app changes in a way that its data has to follow. The hub runs
the update lifecycle (§6) on the strength of this field alone; a source whose
code moved but whose version did not is, as far as the contract is concerned,
the same version of the app.

### Declaring off-window work

`async_worker` declares that this app has work that does not belong on the
request cycle — a Messenger transport to consume. Declaring it gets the app a
real transport it can dispatch to, and a worker consuming it; not declaring it
gets `sync://`, where handlers run inline. Either way the app's own code is the
same code, which is the point.

Two things about this key are deliberate and will not change even as its shape
does.

**Declaration is not consent.** The manifest says *this app has off-window
work*. Whether that work is allowed to continue once the window is closed — a
process living on someone's machine after they have closed the application — is
the user's decision and is asked for outside the manifest. A manifest never
grants itself a permanent process. Today nothing outlives the window, so the
question does not yet arise; when it does, it arrives as a consent step and not
as a manifest key.

**The app is told what is actually running, not what it asked for.** §3 injects
the effective worker state, and the question it answers is the one a user
message needs — *does my scheduled work continue once I close the window?* — not
"is something consuming right now". An app whose 8 a.m. task will not fire
because the window will be closed is entitled to say so, and can only say it if
the environment tells the truth about the machine rather than echoing the
manifest back.

The boolean is the shape accepted today, and it is known to be too narrow: it
hardcodes one transport, while an app using Symfony Scheduler consumes
`scheduler_<name>` and often several transports at once. The replacement is a
list, and it is owned by the background-worker plan rather than by this
document. What that plan may not change is the two paragraphs above.

### Keys this contract does not define

Unknown top-level keys are warned about and ignored (see "Standing rules"). One
key escapes that warning without being part of this contract: `releases_repo`,
which meant something to the archived per-app packaging route and may still sit
in manifests written against it. The hub accepts it silently and does nothing
with it. It has no release feed to point at; an app asks about its own updates
through §7, and how an update is *applied* belongs to the host.

---

## 3. The environment the app runs in

PHP never reads the manifest. Identity, paths, the port and the app's secrets
reach the app as **environment variables**, and this section is the canonical
list of them. Every process the hub starts on the app's behalf gets the same
list: the web server, the lifecycle commands at install time, a worker, a
console command. An app cannot tell them apart, and that is deliberate.

| Variable | Value | For |
| --- | --- | --- |
| `TFS_APP_IDENTIFIER` | `identifier` from the manifest | the app's own identity |
| `TFS_APP_VERSION` | `app_version` from the manifest | the app's own version |
| `APP_ENV` | `prod` | Symfony |
| `APP_DEBUG` | `0` | Symfony |
| `APP_SECRET` | a per-app secret, generated once and kept — §5 | Symfony |
| `APP_PORT` | `app_port` if pinned, else a free loopback port for this launch | the web server |
| `APP_ORIGIN` | `http://127.0.0.1:<APP_PORT>` | Symfony, the web server |
| `APP_PUBLIC_DIR` | the app's `public/` inside the installed snapshot | the web server's document root |
| `APP_CACHE_DIR` | a writable cache directory — see the lifetime note below | Symfony |
| `APP_BUILD_DIR` | a writable build directory — same lifetime note | Symfony |
| `APP_LOG_DIR` | a writable log directory | Symfony |
| `APP_SESSION_DIR` | a writable session directory, this app's own | Symfony |
| `DATABASE_URL` | `sqlite:///<app data>/data/app.db` | Doctrine, if the app uses it |
| `MESSENGER_TRANSPORT_DSN` | a `doctrine://` transport when `async_worker` is declared, `sync://` otherwise | Symfony Messenger |
| `MERCURE_URL` | `<APP_ORIGIN>/.well-known/mercure` | Symfony, publishing |
| `MERCURE_PUBLIC_URL` | identical to `MERCURE_URL` — same origin, loopback | the browser, subscribing |
| `MERCURE_JWT_SECRET` | fresh random value every launch, never persisted | Symfony and the hub |
| `TFS_ASYNC_WORKER` | `"1"` / `"0"` — see "Capabilities are reported, not assumed" | the app, to tell its user |
| `TFS_KEYRING_AVAILABLE` | `"1"` when the OS keyring answered, `"0"` when secrets fell back to a file — §5 | the app, to tell its user |
| `PHP_BINARY` | the interpreter actually running this app | any PHP tool spawning a PHP subprocess |
| `PATH` | prefixed so that `php` resolves to that same interpreter | the same |
| `TFS_BRIDGE_URL` | the loopback bridge's address — **present only when a bridge is running** (§7) | the app's HTTP client |
| `TFS_BRIDGE_TOKEN` | random hex, regenerated every launch, never persisted; present under the same condition | the app, as a bearer token |

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

*(The hub empties them on every launch today. That is a choice about safety, not
a property of this clause, and it is `ARCHITECTURE.md`'s to explain.)*

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

An app author running the same project outside the hub gets whatever their own
`.env` names, which is a different file and correctly so. Point the console at
the injected value when you want the hub's database:

```console
$ DATABASE_URL="sqlite:///$(pwd)/var/data/app.db" \
    php bin/console doctrine:migrations:migrate --no-interaction
```

### Capabilities are reported, not assumed

Two variables exist purely so an app can tell its user something true about this
machine, and they are the standing rules' third clause in practice.

`TFS_KEYRING_AVAILABLE` says whether the secret store is backed by the OS
keyring or has fallen back to a file (§5). It reports what the probe actually
picked, not whether a keyring is installed: a keyring that is present but locked
has already fallen back, and an app is entitled to warn on that basis.

`TFS_ASYNC_WORKER` says whether a worker is consuming the app's transports.
Today the answer is exact and its scope is the window's lifetime: declaring
`async_worker` gets a worker for as long as the app is open, and closing the
window ends it. The rule that binds any future change: this variable mirrors
**what is actually running**, never what the manifest asked for. The day a host
can be asked to narrow the declaration — or to extend it past the window — the
narrowing has to reach the app here, or an app ships a task due at 8 a.m., is
silently narrowed, and neither its author nor its user ever learns it does not
fire.

---

## 4. The HTTP contract

The app is served over plain HTTP on `127.0.0.1` and nothing else. No HTTPS: a
loopback origin is treated as a secure context by the webview, so the browser
APIs that require one are available.

### `GET /healthz` → `200`

The one route the app must expose. It is what says "I am ready to be shown": the
host polls it every 250 ms for up to 60 seconds, and the user reaches the app
itself only once it answers `200`. Until then they are looking at a cold-start
page (§8). If the app's server process dies before answering, the wait aborts
immediately rather than burning the timeout, and the failure is reported with
the cause named.

An app that never answers is an app that never opens. Keep the route cheap and
free of any session requirement — it is a liveness probe, not a diagnostic.

### Response headers the app is given, and can override

Every response carries these unless the app sets them itself:

```
Cache-Control: no-store          (except /assets/*, see below)
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
X-Frame-Options: DENY
Content-Security-Policy: default-src 'self'; connect-src 'self' ipc: http://ipc.localhost;
                         img-src 'self' data:; style-src 'self' 'unsafe-inline';
                         script-src 'self' 'unsafe-inline'; object-src 'none';
                         base-uri 'self'; frame-ancestors 'none'; form-action 'self'
```

Each of the last four is a **default, not a mandate**: a response that already
carries one of those field names keeps its own, and the host's version does not
appear at all. One header, the app's. There is no second policy layered on top
and none silently overriding it.

`/assets/*` is exempted from `no-store` and cached aggressively instead, because
AssetMapper and Encore both serve content-hashed filenames there — a change is a
different URL, so caching is always safe. Serving non-hashed content under
`/assets/` is the one way to get this wrong.

### Nothing the page loads may come from off-origin

`default-src 'self'` is the rule behind the policy above: every script,
stylesheet, font and image a page pulls has to be served by the app itself. A
`<script src="https://cdn.example.com/…">` is refused by the browser with
nothing but a console message — no dialog, no error page, just a feature that
silently is not there. Assets go through AssetMapper or the app's own build
output.

The case every scaffolded project starts with is Symfony's FrankenPHP hot-reload
block: the `symfony/webapp` recipe ends `templates/base.html.twig` with a meta
tag and two CDN `<script>` tags. Remove it. Both are blocked by the policy above,
and there is no watcher for them to talk to even without it.

**Containment, not XSS prevention.** `'unsafe-inline'` stays in `script-src` and
`style-src`, because AssetMapper renders its importmap inline and the profiler
toolbar is inline throughout — removing it would break real apps for a benefit
`connect-src`, `form-action` and `object-src` already deliver. An injected script
can still run; it cannot reach the network, read another origin, or navigate the
top frame. An app wanting nonce- or hash-based hardening emits its own policy,
per the override rule above.

### Same-host requests are not authenticated by the loopback

Any local process can reach `http://127.0.0.1:<port>`. Host matching blocks DNS
rebinding — a remote page cannot redirect a request onto this port under a
different apparent host — but nothing stops a request forged by another local
page or process. The hub does not shield against this; it is the app's to
answer, and Symfony's CSRF protection on state-changing routes is the standard
answer.

### Mercure subscribers must be authorized

A Mercure hub is mounted at `/.well-known/mercure` on the app's own origin,
always, whether or not the app declares a worker. Because loopback is not
user-restricted, **every subscription requires a valid subscriber JWT** — there
is no anonymous fallback, and a request carrying none gets `401`.

How the JWT arrives is the app's call; all three of the protocol's transports
are accepted:

| Transport | Accepted |
| --- | --- |
| `Authorization: Bearer <jwt>` header | yes |
| `mercureAuthorization` cookie | yes |
| `?authorization=<jwt>` query parameter | yes |

The cookie is the recommended default, for two reasons that both follow from the
setup rather than from taste: app and hub share an origin by construction here,
which is exactly the condition a cookie needs, and the browser's native
`EventSource` cannot set request headers at all. The query parameter works but
puts a bearer token into URLs, hence into logs and history — a last resort.

Choosing the cookie means minting it per topic and passing `withCredentials`,
which defaults to `false` per spec:

```php
$authorization->setCookie($request, ['https://example.com/some-topic']);
```

```js
new EventSource(url, {withCredentials: true});
```

Nothing checks that an app did this. One that does not gets a hub that silently
never delivers that topic: no error, no dialog, an `EventSource` that simply
never receives anything. As defence in depth, prefer unguessable per-session
topic names over predictable ones like `/user/1` — not a substitute for the JWT,
but it raises the cost of a blind guess.

One portability note, since it bites outside the hub rather than inside it:
`setCookie()` derives the cookie's domain from the hub's public URL and the
current request's host, and throws when the two share no second-level domain.
Here they always match. A stock Flex project elsewhere still holds the recipe's
`https://example.com/.well-known/mercure` placeholder and throws on every
request that mints a cookie. The fix is configuration — give each environment a
`MERCURE_PUBLIC_URL` that matches how it is served — not a runtime check for
whether a host is present.

---

## 5. The app's own state, and what it is isolated from

### One data directory per app, keyed by its identifier

```
<OS data dir>/TFSApp/<identifier>/
  data/
    app.db          the SQLite database DATABASE_URL points at
    app.secret      APP_SECRET in plaintext — only on a machine with no keyring
    secrets.json    the app's declared secrets (§7), same condition
    config.json     which version last wrote all of this
  cache/            APP_CACHE_DIR — may be emptied at any launch (§3)
  build/            APP_BUILD_DIR — same
  log/              APP_LOG_DIR — persists, rotated
  sessions/         APP_SESSION_DIR — persists
```

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
which happened, for this launch.

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

Stating it that way matters because "whoever launched it" is not hypothetical.
An app installed here and the same app arriving by another route resolve to the
same identifier, hence to the same data directory, hence to the same lock; the
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

---

## 6. Lifecycle

An app declares, under `commands`, what has to run when it arrives on a machine
and when a newer version of it does:

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

**The record is never written speculatively.** It is updated only after the
event's last command has succeeded, so a failure leaves the previous record — or
none at all, on a first install — in place, and the next attempt replays the
*whole* event rather than resuming from the command that failed. There is no
partial state to reason about.

**Today the hub runs the install event and refuses the update.** `install`
executes `pre-install` then `post-install`, in order, with the full environment
and no server running, on a terminal where a failure is legible. Per-app update
is a queued plan; until it lands, `pre-update` and `post-update` may be declared
but never run, and an installed snapshot found to be newer than its own record is
refused with the command that resolves it named. When update does land, it owns
one guarantee this section deliberately does not state on its behalf: an update
must never leave the app's database between two versions.

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
the very interpreter that will later serve the app, which is the only build-like
step the contract needs.

### Command strings are argv, never a shell

Each entry is a `bin/console` argument string, split on whitespace and passed as
`argv` directly. There is **no shell interpretation** — no pipes, no
redirection, no variable expansion, no `&&`. Two consequences, both intended:
a manifest cannot become a shell-injection vector, and only `bin/console`
commands can be declared, never arbitrary executables.

This is stricter than it would need to be if these commands only ever ran on
their author's machine. They do not: they run on a user's machine, from a
manifest that user did not write.

---

## 7. Native capabilities: `actions`

`actions` answers exactly one question: **what may the app's own code reach?**
Not what the user may do — the commands a person types are the host's, and they
run because that person typed them, which is a different act with different
consent. This section is about the doors application code can knock on.

There are two of them today, and each is declared per **transport**:

```json
{
  "actions": {
    "secrets": { "ipc": false, "bridge": true, "keys": ["openai", "anthropic"] },
    "update":  { "ipc": true,  "bridge": true }
  }
}
```

| Transport | Reaches |
| --- | --- |
| `ipc` | the webview's own JS, through the host's IPC channel |
| `bridge` | PHP, through a loopback HTTP server — see "The bridge wire contract" below |

Granularity is **group × transport, all or nothing per group**; there is no
per-command configuration. Absent means off, at every level: no `actions`, an
absent group, and an absent transport all mean the same thing. A group declared
on neither transport is completely unreachable — no server thread, no
environment variables, no grant — and the app is unaffected either way.

### `secrets`

Read and write the app's own secrets in the OS keyring, from either transport,
against the same store. Read §5 first: that store is a namespace, not a
boundary.

**`keys` is a manifest, not a permission grant.** It is required and non-empty
the moment either transport is on. What it buys is typo-catching (a `get`/`set`
mismatch fails loudly instead of returning nothing), enumerability — the only
reliable way to render a settings screen — and a cap on how many secrets one app
can ever touch. It does not widen anything: an app cannot name another app's
entries whatever it lists, because the store is resolved from the window that
asked and never from a string the app passes.

The list is enumerable over both transports, in declared order, each key with
whether it currently holds a value, so a settings screen can render itself
without re-declaring the list in the app's own code.

**Values are capped at 8 KiB**, checked before the store is touched. That leaves
real headroom for something like a PEM private key without turning the OS keyring
into a data store.

**Reserved keys are refused even when declared.** `app-secret` — the value that
signs CSRF tokens, signed URIs and remember-me cookies (§5) — and the
keyring-availability probe's own account can never be reached over either
transport, whatever `keys` says.

**Neither transport is safer; the choice is a real trade-off.**

| | protects | exposes |
| --- | --- | --- |
| IPC | confidentiality in transit — the value never touches the PHP process | integrity: an XSS in the app can overwrite a declared key |
| Bridge | integrity — a bearer token only PHP holds | confidentiality to PHP and anything that logs it: request body, profiler, server logs |

An app building an API-key entry form has a legitimate reason to want `ipc`.
This document deliberately does not steer everyone to the bridge.

### `update`

Lets the app ask its host whether a newer version of itself exists. The
*question* is the portable part and the valuable one; how an update is found and
applied belongs to the host.

```json
{"status": "ok", "current": "1.1.0", "latest": "1.2.0",
 "update_available": true,
 "release_url": "https://…", "notes": "…"}
```

```json
{"status": "unavailable", "reason": "host_resolves_updates_itself"}
```

`update_available` compares semver: a published version older than *or equal to*
the running one is `false`, and so is a running version newer than anything
published. The result says **what changed** and nothing about how to apply it —
there is no downloadable asset in it, because an app pointed at a download it
cannot apply is worse off than one told nothing. If a host ever offers "apply it
now", that is a separate action, not a field in a query's answer.

`reason` is a stable machine-readable token rather than a sentence, so an app can
hide its "check for updates" button on one and show a network error on another.

**The check is pull, never push.** No automatic, background, periodic or startup
check, and no notification badge. The app decides when to ask and how to render
the answer; the host never blocks startup or raises a dialog about a version
nobody asked about.

**A check that cannot be made is a result, not a failure.** Both transports
answer successfully with `status: "unavailable"` rather than throwing, which is
what lets an app call this on a timer without wrapping it in exception handling.

**Today the hub always answers `unavailable`,** with the reason above: it
resolves updates itself, through a command, and has no release feed to point an
app at. The real answer needs the per-app update plan; until then the honest
`unavailable` is served over both transports, so an app writes its handling once
and never learns that anything is missing.

### The bridge wire contract

PHP cannot see the webview's IPC channel. The bridge is the transport that gives
the app's PHP side the same capabilities, and it starts as soon as *any* group
declares `bridge: true`.

**Transport.** Plain HTTP on `127.0.0.1`, on a free port chosen at launch
(`TFS_BRIDGE_URL`), with a random bearer token regenerated every launch and
never persisted (`TFS_BRIDGE_TOKEN`). Thread per request, so a slow handler
cannot block a concurrent call. **Every route requires
`Authorization: Bearer <token>`, including `/healthz`** — a missing or wrong
token gets `401 {"error": "unauthorized"}` before anything else runs.

**Absence must degrade, never throw.** No `TFS_BRIDGE_URL` and no
`TFS_BRIDGE_TOKEN` means no bridge — because no group declared one, or because
this PHP process was not started by a host at all. The app's own client code has
to treat that as "unavailable", not as an exception.

**Routes are gated per group, not per bridge.** Two groups share one server, so
a group whose own `bridge` is `false` gets its routes refused with a plain
`404 {"error": "not_found"}` — indistinguishable from an unrecognised path —
even while the server runs because the *other* group wanted it. `/healthz` is
the exception: it is the bridge's own liveness route, belongs to no group, and
always answers once authorised.

| Route | Body | Success |
| --- | --- | --- |
| `GET /healthz` | — | `200 {"status": "ok"}` |
| `GET /secrets/keys` | — | `200 {"keys": [{"key": "…", "set": bool}, …]}` |
| `POST /secrets/has` | `{"key": "…"}` | `200 {"has": bool}` |
| `POST /secrets/get` | `{"key": "…"}` | `200 {"value": "…"}`, or `404 {"error": "not_found"}` when never set |
| `POST /secrets/set` | `{"key": "…", "value": "…"}` | `200 {"ok": true}` |
| `POST /secrets/delete` | `{"key": "…"}` | `200 {"ok": bool}` — whether a value existed |
| `GET /update/check` | — | `200` with the result shape above, never an error for a network condition |

**Errors, in the order they are checked:** `401` unauthorized (before routing) →
`404 not_found` (the group is off, or the path is unknown — deliberately the same
answer either way) → `413 payload_too_large` (the whole body exceeds a transport
cap, before any parsing) → `400 invalid_body` → `403 key_not_declared` (reserved,
or not in `keys` — before the store is touched) → `413 value_too_large` on
`/secrets/set` → the route's success shape.

The bridge never logs the token, a secret value, or the update check's body, on
success or on failure.

---

## 8. What a host cannot honour, and how you find out

The second standing rule in practice. Everything below is a case where an app
declares something the machine — or this host, today — cannot deliver exactly.
None of them fails the app; all of them are visible.

### The renderer belongs to the host

An app's frontend runs against the WebKitGTK the host ships, not one frozen per
app at build time. Two consequences worth designing around: every installed app
on a machine gets the *same* renderer, so a feature that works in one works in
all; and that renderer moves when the host is updated, not when the app is.
Target the host's, not the browser you develop against.

### An icon larger than a window property can carry

`icon_path` should be a large square PNG. Some surfaces cannot take it at full
size — a window icon property has a hard ceiling, and a source above it is
silently dropped by the toolkit, leaving a generic icon and no error. The host
downscales for that surface, and says which size it used. The full-size file
stays the one used everywhere that can take it.

Do not pre-shrink the source to dodge this. It costs quality on every other
surface to fix one.

### A pinned port that is already taken

`app_port` pins a port for a reason, so a host that quietly took a different one
would break whatever the pin was for. Startup stops instead, and names the
conflict.

The resolution is per-installation rather than per-app, because the manifest is
the same on every machine and the conflict is not: it is `port_override` in the
app's own `data/config.json`, which survives updates. An app that does not
actually need a fixed port should leave `app_port` out and take a dynamic one.

### The cold-start page

`splash_path` is **not honoured today.** The file sits inside the installed
snapshot, outside `public/`, and it is wanted *before* the app's own server
exists — so it is reachable neither over HTTP nor from the window's own origin.
Rather than fail, the host shows its own cold-start page in the app's declared
`splash_bg` / `splash_text` colours, with its `product_name`, and warns at
launch that it did so.

So `splash_bg` and `splash_text` work; `splash_path` is parsed, reported and
ignored. Closing that gap is a queued plan, not a change of contract.

### Off-window work, and secret storage

Both are covered where they belong — §3's `TFS_ASYNC_WORKER` and
`TFS_KEYRING_AVAILABLE` — and both are listed here because they are the same
rule: the app declares the maximum, the machine delivers what it can, and the
environment says which.

### Declared but not yet runnable

`run` aliases are parsed and validated, and the command that runs one is a
queued plan. An app may declare them; nothing runs them yet. This is stated
rather than left silent so that an alias which appears to do nothing is
recognisable as an unbuilt feature and not as a broken declaration.

---

## Things this document deliberately does not cover

**A navigation off the app's origin leaves the app.** A link to another site
opens in the user's own browser instead of replacing the application inside its
own frame; `javascript:` URLs are refused outright. This is worth knowing when
writing links, but it is the host's navigation policy rather than a promise to
the app — [`ARCHITECTURE.md`](ARCHITECTURE.md) has the rules.

**A launch that fails always says so.** No silent exit, no window that never
appears. What the message says and where it appears is the host's.

**How any of this is implemented** — the installer, the snapshot, the registry,
the process supervision and teardown, the bundled interpreter, the packaging —
is in [`ARCHITECTURE.md`](ARCHITECTURE.md).

**What a user types** — installing, opening, listing, removing — is in
[`README.md`](README.md). The commands a person runs are not configured by the
manifest and are not part of this contract, with one exception: the `run`
aliases above, because the commands they run are the app's own.
