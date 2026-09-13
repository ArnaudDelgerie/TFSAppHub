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

### The host serves the app's document root, and nothing of its own

The web server's document root is `APP_PUBLIC_DIR` (§3), and that is the whole
of what the host puts on the wire. There is no second mount, no reserved path
prefix, no static serving of `APP_UPLOAD_DIR` or anything else outside
`public/` — reading a durable file back is a route the app writes, behind the
app's own authorization, exactly like any other route. `Content-Disposition:
attachment` is the default worth reaching for on that route: `default-src
'self'` above permits everything the app serves, so an uploaded file handed
back inline is same-origin content with the same reach as the app's own
scripts. See `.project/decision/006-durable-files-live-in-the-data-directory.md`
for why this was decided rather than defaulted.

What the window does with such a response, today: the file is saved straight
into the OS download directory, under the name the response gives it —
de-duplicated if a file of that name is already there — with no Save-As
prompt, and each download leaves a line in `hub.log`. This is provisional, not
a guarantee: a prompt is wanted and not yet built, so an app must not rely on
either the silent save or the destination.

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

### A request in flight gets two seconds when the app closes

Closing the app stops its server gracefully: requests already being handled are
allowed to finish, and **two seconds** is how long they get. Past that the
connection is closed under them, and the process exits regardless.

The bound exists because the alternative is unbounded. A graceful stop waits for
its connections to drain, and a Mercure subscription is a stream that never
drains — so without a limit, one subscriber anywhere on the machine could hold
an app open for as long as it liked. The host's own escalation would eventually
kill it, but by then nothing has shut down cleanly: PHP's shutdown functions
never run and buffered writes are lost, which is the outcome the bound exists to
avoid, not to cause.

Two seconds is not a budget to design against. It is generous for a loopback
request against a local SQLite database and deliberately short of the host's own
kill deadline, so the ordinary close is clean. **An app that needs to finish
something longer than a request must not do it in a request**: that is what §2's
`workers` and §6's `run` are for. Work still running in a stream or a
long-poll when the user closes the window is work that will be cut off.

