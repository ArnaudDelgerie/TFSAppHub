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
app's own `data/config.json`, which survives updates. A dev session has no
`data/config.json` to hold one (§9): the guard still runs and still stops on a
real conflict, but the only fix there is to free the port or drop `app_port`
from the manifest. An app that does not actually need a fixed port should
leave `app_port` out and take a dynamic one.

### The cold-start page

`splash_path`'s file sits inside the installed snapshot, outside `public/`,
and it is wanted *before* the app's own server exists — so it is reachable
neither over HTTP nor from the window's own origin at launch time. The host
closes that gap with a read-only custom URI scheme, registered once per
process and scoped to that process's own resolved snapshot root: every
request path is canonicalised and confirmed inside the root before anything
is opened, so one app's scheme can never reach another's tree. When
`splash_path` is declared and the resolved file exists and is readable, the
splash window loads it over that scheme; when it is absent, missing or
unreadable, the host shows its own cold-start page in the app's declared
`splash_bg` / `splash_text` colours, with its `product_name`, and warns at
launch that it fell back and why.

The scheme's own responses carry a `Content-Security-Policy` distinct from
§4's default — `default-src 'none'` with `style-src`/`script-src
'unsafe-inline'` and `img-src data:` admitting exactly what a self-contained
splash file is documented to use, and `base-uri`/`form-action`/
`frame-ancestors` pinned to `'none'`. A page reaching past its own inline
markup fails visibly rather than silently reaching further into the snapshot
or the network.

The scheme's reach is bounded by navigation policy, not by unregistering it:
Tauri has no runtime unregistration, and one hub process serves one app for
its whole life, so the handler staying registered is not itself a new
isolation risk. §4's navigation policy treats the scheme as internal only
while the app's own origin is not yet known — the splash window, before
hand-over — and once the backend origin is published, a navigation to the
scheme is refused like any other non-`http(s)` target. The running app gains
no standing second way to read its own splash snapshot after hand-over.

So `splash_bg`, `splash_text` and `splash_path` all work; the two colour
fields recolour the host's own fallback page specifically — an app that
supplies its own `splash_path` page styles it itself, inline, since that page
replaces the fallback rather than layering on top of it.

### The renderer can die

The host's own WebKitGTK is shared across every window (above), and it can
still crash, hit its own memory limit, or otherwise lose the process
rendering a window's page — a decoder, a GPU driver, WebKit itself. When that
happens outside a termination the host asked for itself, the window shows the
host's own page in the app's declared `splash_bg` / `splash_text` with its
`product_name`, the same colours and fallback the cold-start page above uses,
with a Reload button. `hub.log` records the reason.

The host never reloads on its own: whatever crashed the process could crash
again on the same input, so the user's click is the only way back, and there
is no retry count or crash-loop heuristic hiding behind it. The app is never
told — no environment variable, no event, no query parameter marks the next
page load as a recovery — it arrives exactly like any other load of the same
URL.

### Off-window work, and secret storage

Both are covered where they belong — §3's `TFS_ASYNC_WORKER` and
`TFS_KEYRING_AVAILABLE` — and both are listed here because they are the same
rule: the app declares the maximum, the machine delivers what it can, and the
environment says which.

### A backend that cannot install the microphone grant

The same rule again, for `actions.media.microphone` (§7): a backend that
cannot install the WebKit permission handler — today that would mean a
non-GTK backend, since the Linux host always can — reports `TFS_MEDIA_MICROPHONE=0`
rather than failing the launch, and a grant the launch could not confirm
within its 2-second wait counts as not installed, so the variable reports
`0` for that too. A missing or broken capture stack on an
otherwise-granted machine is not this host's to catch at all: it surfaces
through `getUserMedia()` failing at the point of use, exactly as it would on
any ordinary web page.

### A declared user directory GLib cannot resolve

The same rule again, for `actions.paths` (§7, decision 008): GLib answers from
`~/.config/user-dirs.dirs` and its own compiled-in defaults, not from a
`stat()` call, so a declared member it cannot resolve reports as an absent
`TFS_USER_<NAME>_DIR` variable (§3) rather than a guessed path built from
`$HOME`. The reverse case — a path GLib resolves that no longer exists, or is
not writable — is not caught here either: whether that check ever happens is
left open (decision 008), and today the variable simply reports GLib's answer
as-is.

