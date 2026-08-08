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

That is the whole requirement. There is no TFSApp base class to extend, no
directory to create, no file the hub writes into the project. Installing does
not modify the source it was pointed at.

**The manifest is read by the hub, never by PHP.** The app learns its own
identity from the environment (§3), not by parsing its own
`tfsapp.config.json` — so the same code runs unchanged under any deployment
that sets those variables. An app that reads the manifest at runtime has coupled
itself to being installed, which is exactly what §3 exists to avoid.

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
