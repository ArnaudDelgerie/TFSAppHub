## 7. Native capabilities: `actions`

`actions` answers exactly one question: **what may the app's own code reach?**
Not what the user may do — the commands a person types are the host's, and they
run because that person typed them, which is a different act with different
consent. This section is about the doors application code can knock on.

There are four capability groups today. `secrets`, `update` and `close_guard`
may declare either of the two transports; `picker` is deliberately IPC-only:

```json
{
  "actions": {
    "secrets":     { "ipc": false, "bridge": true, "keys": ["openai", "anthropic"] },
    "update":      { "ipc": true,  "bridge": true },
    "picker":      { "ipc": true },
    "close_guard": { "ipc": true,  "bridge": true }
  }
}
```

| Transport | Reaches |
| --- | --- |
| `ipc` | the webview's own JS, through the host's IPC channel |
| `bridge` | PHP, through a loopback HTTP server — see "The bridge wire contract" below |

Granularity is **group × available transport, all or nothing per group**; there
is no per-command configuration. `picker` has only the IPC transport, so a
`bridge` member there is refused rather than held as a future permission.
Absent means off, at every level: no `actions`, an absent group, and an absent
transport all mean the same thing. A group declared on none of its available
transports is completely unreachable — no server thread, no environment
variables, no grant — and the app is unaffected either way.

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
{"status": "unavailable", "reason": "local_source"}
```

`update_available` compares semver: a published version older than *or equal to*
the running one is `false`, and so is a running version newer than anything
published. The result says **what changed** and nothing about how to apply it —
there is no downloadable asset in it, because an app pointed at a download it
cannot apply is worse off than one told nothing. If a host ever offers "apply it
now", that is a separate action, not a field in a query's answer.

`reason` is a stable machine-readable token rather than a sentence, so an app can
hide its "check for updates" button on one and show a network error on another.
The host publishes the token; the sentence a person reads belongs to the app.

| token | when |
| --- | --- |
| `local_source` | installed from a local directory, or a `dev` session — no release feed exists |
| `no_answer_yet` | a release-installed app whose cache holds nothing usable: the first launch after install, or every refresh attempted so far has failed |

Offline, rate-limited, a malformed release and a repository that 404s all
collapse into `no_answer_yet` — an app cannot act differently on any of them,
so they share one token, and the detail goes to the host's own log rather than
into a vocabulary apps would have to branch on.

**The check is pull, never push.** No automatic, background, periodic or startup
check, and no notification badge. The app decides when to ask and how to render
the answer; the host never blocks startup or raises a dialog about a version
nobody asked about. A host refreshing its cache in the background, on its own
schedule, is not a push: nothing is delivered to the app or its user unrequested
— the app still has to ask, and the answer it gets is only ever a reply.

**The check never opens a socket in the app's request path.** Whatever a host
does to learn the answer, it does before the app asks, not while the app waits
— so polling this on a timer costs nothing and cannot fail for a network
reason. A stale answer is served rather than withheld: an update check is never
urgent, and a host running behind is not a reason to answer less than it knows.

**A check that cannot be made is a result, not a failure.** Both transports
answer successfully with `status: "unavailable"` rather than throwing, which is
what lets an app call this on a timer without wrapping it in exception handling.

**The hub answers `ok` for a release-installed app once it has resolved that
release's feed at least once**, and `unavailable` / `no_answer_yet` until then
— never by dialling out while the app waits, only from what a background
refresh already learned. A local install, or a `dev` session, always answers
`unavailable` / `local_source`: the "latest version" of a directory a developer
edits by hand is whatever they last typed there, and the question has no other
meaning. (`host_resolves_updates_itself`, an earlier reason token, is retired;
it will not reappear.)

### `picker`

Shows one native chooser owned by the calling app window. It is for selecting a
directory, retaining the local path an integration itself understands, or
letting a person choose where a file the app is about to write should go. It is
not the normal way to upload a file: for an upload, prefer the browser's
`<input type="file">`, which provides a `File` object and owns the upload flow.

`picker` has one transport and one exact manifest shape:

```json
{"picker": {"ipc": true}}
```

There is no `bridge` transport, HTTP route, bridge environment variable, or PHP
permission for this group. PHP processes, workers, lifecycle commands and a
page on the splash origin cannot open a chooser. A manifest that names
`actions.picker.bridge` is invalid and says that this group has no bridge
transport.

**`pick_path`** — "which existing file or directory?" The webview calls
Tauri's raw IPC as either `invoke("pick_path", { kind: "file" })` or
`invoke("pick_path", { kind: "directory" })`. `kind` is required; any other
value is invalid input and returns an IPC error. A selection resolves to its
absolute path as a string, and cancellation resolves successfully to `null`.

`kind: "file"` also accepts `filters`, optional and in the same shape as
`save_path`'s:

```js
invoke("pick_path", { kind: "file", filters: [{ name: "Markdown", extensions: ["md"] }] })
invoke("pick_path", { kind: "file", filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg"] }] })
```

Omitted, `null` and an empty list all mean no application filter — the
chooser shows every file. With entries, the chooser displays only files
matching one of them, in the order given and with the first one active, each
`extensions` entry written without a leading dot — the same native semantics
as `save_path`. Filters affect what the chooser displays; the selected file
and its contents remain the app's own to validate. For
`kind: "directory"`, well-formed filters are ignored rather than refused:
folders are not selected by extension, and a caller may share one chooser
options object between both kinds. A wrongly typed filter field is still
invalid input and returns an IPC error for either kind.

**`save_path`** — "where should this new file go?" The webview calls
`invoke("save_path", { filters, fileName, directory })`, all three arguments
optional:

```js
invoke("save_path", {
  filters:   [{ name: "Markdown", extensions: ["md"] }, { name: "Text", extensions: ["txt"] }],
  fileName:  "notes.md",
  directory: "/home/…/Documents"
})
```

`filters` opens the dialog with one filter per entry, in the order given, the
first one active; each `extensions` entry is written without a leading dot.
`fileName` pre-fills the suggested name. `directory` sets the starting
directory and must be an absolute path — a relative one is invalid input and
returns an IPC error, like any other invalid input in this group.
`invoke("save_path", {})` opens a bare dialog. The result shape is exactly
`pick_path`'s: the chosen absolute path as a string, or `null` on
cancellation, including GTK's own "replace?" confirmation when the typed name
already exists.

The hub never writes. The path `save_path` returns may name a file that does
not exist yet — the app's own PHP creates it, no filesystem scope is granted,
and the hub cannot tell whether a path handed to a route came from this
dialog, so that route writing to it is the app's responsibility like any
other route. The path is also returned exactly as typed: the filter list is a
view filter, not an enforced extension, and the hub does not append one — an
app that appends an extension itself is writing to a name GTK's own dialog
never asked "replace?" about.

The dialog is the person's consent to that one selection. It does **not** grant
the app filesystem scope, read or write the selected file or directory, copy
it, upload it, retain an OS file descriptor, or persist anything. A returned
path remains privacy-relevant information: an app that sends it to its PHP
backend or saves it is responsible for that choice. Treat a saved path as
machine-local configuration, never as exportable application state or a
promise that it will survive another computer, an OS migration, a missing
mount, or a permission change.

### `close_guard`

Lets the app mark work that a person should be warned about before a window
closes, from either side of itself: the webview for a document's unsaved
changes, PHP for app-wide background work. Closing a guarded window shows one
native confirmation — host-authored, `Cancel` as the safe default — before the
window is hidden or the backend stopped; cancelling keeps the running app
exactly as it was. A close that nothing guards closes the way it always did,
and an app that declares nothing keeps its current UX entirely.

**Two namespaces, one rule each.** `ipc` is the webview's, for guards that
belong to one document in the calling window; `bridge` is PHP's, for guards
that belong to this running app instance. Neither transport can reach the
other's namespace, and no request can name another window, another document
or another app — the webview's window is read from Tauri's caller, and the
bridge holds one process's state. Both switches default to off and neither
implies the other; declaring only `close_guard.bridge` starts the bridge (and
injects `TFS_BRIDGE_URL`/`TFS_BRIDGE_TOKEN`) exactly as declaring
`secrets.bridge` does.

**Guards are identifiers, not messages.** An ID is an app-chosen non-empty
string of at most 128 bytes of UTF-8 — `editor:<document-id>`,
`export:<job-id>`. Registration is idempotent per owner and ID; removing an
absent ID succeeds harmlessly; distinct simultaneous jobs need distinct IDs.
The hub caps each namespace — 16 frontend guards per window, 16 backend
guards per app instance — and exhaustion is an explicit error, never a silent
eviction of another task's protection. There is no global clear operation,
and nothing is persisted or exported: a guard lives in this launch's memory
and nowhere else.

**A frontend guard belongs to a document, and the document proves itself.**
Every committed load of a window's main frame gives the page a fresh opaque
context, which it fetches once at startup and presents back with every
register/remove call:

```js
// once per document, at startup:
const { context } = await invoke("close_guard_context");
// the document becomes dirty:
await invoke("close_guard_register", { context, id: `editor:${docId}` });
// saved, or the dirty state discarded:
await invoke("close_guard_remove", { context, id: `editor:${docId}` });
```

A call presenting anything but the calling window's *current* context is
refused with `stale_document` — it belongs to the document a reload or
navigation replaced, and it cannot register or remove anything for the
successor. The context is generated by the host, never the page, so it cannot
be guessed in advance either. The previous document's guards are erased when
its successor first fetches the context — never by a load-finished callback,
so a page that registers during its own startup can never have its guards
erased by its own load. A cancelled or blocked navigation and a same-document
navigation (`pushState`) change nothing: no commit, no new context, guards
kept. A failed load or a dead renderer is handled conservatively — the old
guards stay until the window is destroyed, because a warning a person can
dismiss costs nothing and a silently-erased guard is data loss. A window's
guards die with the window, whatever incarnation set them; a window label
reused later is a new owner with a new context.

**A backend guard is conservative by design.** PHP registers before starting
vulnerable work and removes its own guard in `finally`:

```php
// Before the work starts, not after: a guard registered late cannot
// warn about the close that already happened.
$bridge = $this->bridgeClient();      // TFS_BRIDGE_URL + TFS_BRIDGE_TOKEN
$bridge->post('/close-guard/register', ['json' => ['id' => $id]]);
try {
    $this->run($job);
} finally {
    $bridge->post('/close-guard/remove', ['json' => ['id' => $id]]);
}
```

Two simultaneous jobs are two IDs — include the job's own id in the string
(`export:{$job->getId()}`, not `export`) so the two lifetimes stay
independent, and so finishing one cannot remove the other's protection.
Guards have **no automatic expiry**: a long job must not lose protection
because it cannot send a heartbeat, and a worker being replaced never clears
a guard — only the code that registered it (in `finally`) or this process's
exit does. A task that crashes may leave its warning behind for the rest of
the launch; the person can always choose to close anyway, and the next launch
starts clean.

**What the confirmation does, and does not.** Closing a window consults that
window's own frontend guards, plus the backend guards only when this close
would stop the shared backend — a clean secondary window never prompts
because work is running in another window. The native dialog names the stakes
by category — unsaved changes, background work, or both — with no
app-supplied content. Cancel, dismiss or a failed dialog never grants
permission; the guards stay and the window stays. Confirmation is permission
for this one close attempt, not a persistent bypass and not an instruction to
save: it closes exactly the window that was closing, and only the last
window's confirmation stops the backend. While a decision is pending the
hub shows one dialog at a time, and rechecks what is at stake before applying
an answer — a guard that appeared while the dialog was open, or a close that
became the backend-stopping one, gets a fresh warning rather than a stale
approval.

**A close commits as one atomic transition, and the commitment reserves its
window.** The final guard check and the commitment are the same step: a
registration racing a close is either in the decision's past — covered by the
warning, or the cause of the fresh one — or in its future, where `closing`
refuses it. It is never accepted and then ignored by a close that
revalidates nothing, on either transport. Once a close is committed, its
window stops counting as a survivor for every other close until its
destruction is actually observed — so two nearly-simultaneous closes cannot
both conclude that the other window preserves the backend — and that window's
document can no longer register or remove guards: the person already approved
closing it, so a guard that appeared now could never earn its own
confirmation. A window whose close is committed and whose destruction failed
while it stayed usable goes back to being an ordinary window; a committed
whole-app shutdown releases nothing.

**A decision belongs to the document it was decided on.** The confirmation
and the close it commits are bound to the window's document incarnation, not
just its label and the guard IDs the warning named. A page replaced while
its dialog stands — a reload, a programmatically scheduled navigation — makes
the answer apply to nothing, whatever guard IDs the successor reuses or
registers: the successor stays open with its own guards, no second dialog is
stacked while the first stands, and a new explicit close request starts a
decision of its own. The same binding holds at the moment the approved close
is applied: a destruction queued for one document is never redirected onto
its replacement, and a window label reused after a destruction is a new owner
that inherits neither the queued close nor a pending decision. An
acknowledgement protects subsequent close decisions; it cannot reverse a
close that already committed. Once shutdown has committed, every register or
remove — from either transport — answers `closing` explicitly, so a job
starting during teardown learns the truth instead of installing a guard
nobody will ever see.

**Unavailable is a result, not an exception.** A hub older than this group
refuses the `invoke` outright, and a missing or refused bridge route is
unavailable the same way: app code that probes before use and degrades when
the answer is no. Declaring the group and getting no error is the only
success; treat anything else — a refused permission, a 404, no
`TFS_BRIDGE_URL` at all — as "no guard installed", never as one:

```js
// The document wants protection, but a hub without the group refuses the
// invoke before any handler runs — degrade, never break:
try {
    const { context } = await invoke("close_guard_context");
    await invoke("close_guard_register", { context, id: `editor:${docId}` });
} catch {
    // No guard installed; the app closes the way it always did.
}
```

```php
// PHP's probe is the environment itself: no TFS_BRIDGE_URL means the
// launch declared no bridge group, so the job runs unguarded rather than
// treating a missing guard as an installed one.
if ($bridgeUrl && $bridgeToken) {
    $bridge->post('/close-guard/register', ['json' => ['id' => $id]]);
    // ...and only a 200 means the guard exists.
}
```

The IPC commands and their exact errors: `close_guard_context` answers
`{"context": "…"}` for the calling window's current document;
`close_guard_register` and `close_guard_remove` take `{ context, id }` and
answer `null` on success. Their error codes, shared with the bridge routes
where the transport has them: `stale_document` (frontend only), `invalid_id`,
`too_many_guards`, `closing` — shutdown has committed, or this window's close
already has, so the namespace is closed — and `unavailable` when no state backs
the calling window at all.

### The bridge wire contract

PHP cannot see the webview's IPC channel. The bridge is the transport that gives
the app's PHP side the capabilities whose groups declare it, and it starts as
soon as *any* such group declares `bridge: true`. It does not expose IPC-only
groups such as `picker`.

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
| `POST /close-guard/register` | `{"id": "…"}` | `200 {"ok": true}` |
| `POST /close-guard/remove` | `{"id": "…"}` | `200 {"ok": true}` |

**Errors, in the order they are checked:** `401` unauthorized (before routing) →
`404 not_found` (the group is off, or the path is unknown — deliberately the same
answer either way) → `413 payload_too_large` (the whole body exceeds a transport
cap, before any parsing) → `400 invalid_body` → `403 key_not_declared` (reserved,
or not in `keys` — before the store is touched) → `413 value_too_large` on
`/secrets/set` → the route's success shape. The close-guard routes share the
front of that order and add their own tail after `invalid_body`:
`400 {"error": "invalid_id"}`, `429 {"error": "too_many_guards"}`,
`503 {"error": "closing"}` — the last meaning shutdown has committed and the
guard namespace is closed for the rest of this process's life.

The bridge never logs the token, a secret value, or the update check's body, on
success or on failure.

