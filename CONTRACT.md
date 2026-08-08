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
