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

At install time, `identifier` may not be exactly `hub`, `TFSApp`, or
`applications`. Those values name, respectively, the hub's own root, the shared
vendor directory, and the XDG desktop-entry directory; the hub refuses them
rather than letting an app overlap its infrastructure. This is a rule about
known **values**: unknown top-level keys still only warn and never refuse.

The hub-local `id`, whether derived from `project_name` or supplied with
`--as`, may not end in `.previous`. `update` deletes and recreates
`apps/<id>.previous` as its rollback anchor, so an app installed under that
suffix would be destroyed by another app's update.

### Optional fields

| Field | Type | Meaning |
| --- | --- | --- |
| `app_port` | integer or null | Pins the app's loopback port instead of taking a fresh free one each launch. Absent (the default) is dynamic, and dynamic is the right answer unless something outside the app must know the port in advance. |
| `icon_path` | string | Project-root-relative path to one square source PNG (1024×1024 RGBA recommended, ≥512 wanted) used as the app's launcher, switcher and window icon. Absent keeps a placeholder. |
| `splash_path` | string | Project-root-relative path to one self-contained HTML file (inline CSS and JS only, no external assets) shown while the app cold-starts, served read-only over a scheme scoped to the app's own snapshot. Missing or unreadable falls back to the host's own cold-start page — see §8. |
| `splash_bg` / `splash_text` | string, `#rgb` or `#rrggbb` | Recolour the cold-start page's background and text without authoring one. Either or both; an unset one keeps the default. |
| `commands` | object | Lifecycle commands the hub runs around an install or an update — §6. |
| `run` | object | Named `bin/console` aliases a user can run directly. |
| `actions` | object | Which native capabilities the app's own code may reach, and over which transport — §7. `actions.picker` and `actions.open_files` are IPC-only: `{ "ipc": true }`. `open_files` has one further default-off option, `directories` — see "Declaring file associations" below. `actions.media` and `actions.paths` name a device or a resource rather than a transport and need not use either: `{ "microphone": true }`, `{ "downloads": true }` — §7. |
| `file_associations` | object | The MIME types this app declares it can open — see "Declaring file associations" below. |
| `workers` | array of objects | Declares one or more background consumers, each an ordered list of Messenger transports plus an optional copy count — see "Declaring off-window work" below. |
| `async_worker` | boolean | Sugar for a single worker consuming `async`; refused together with `workers` — see "Declaring off-window work" below. |
| `build_outputs` | array of strings | Project-relative directories the author's own frontend build step produces, gitignored in the project. `publish` alone reads this key — `install`, `update` and `dev` ignore it — and refuses a declared path that escapes the project, sits under an excluded component, is absent or empty, holds a tracked file, is not gitignored, or holds anything but regular files and directories, before embedding it in the release archive as it stands — see §1's "Publishing a release". |

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

Every surface fed by `identifier` reads `dev.<identifier>` instead, for a dev
session — see §9.

**An installed app has a desktop entry, and this is the normal case.**
Installing writes `Name=` from `product_name` and `Icon=` from `icon_path` at
full size — the same fields and the same one-field-per-surface rule as the
table above — to a per-user location with no root required. The user may
decline it at install (`--no-desktop-entry`), and `remove` reverses it. An app
must not ship a `.desktop` file of its own: the hub writes and owns the one
that names it.

One consequence surprises people the first time and is not a defect, and it is
now the *exception's* behaviour rather than the rule's: without a desktop
entry — a dev session (§9), or an install that declined one — a desktop
environment has no `Name=` to read and falls back to a label derived from the
window class, which is the identifier. So a window with no entry behind it can
appear in the switcher as `dev.local.labelboard` rather than as *LabelBoard*.
The fix is the desktop entry, never the window class: the class has to be the
identifier or nothing can match a window to its own app.

### `app_version` is semver, and it is load-bearing

`app_version` must be canonical `MAJOR.MINOR.PATCH` — no leading zeros, no
suffix, no `v`. This is not a stylistic preference. It is the value the hub
compares to decide whether putting a given source on this machine is an
install, an update, or a downgrade to refuse, and a value that cannot be
compared makes that decision unanswerable.

The hub **refuses at the transitions**: installing or updating an app whose
`app_version` is not canonical semver fails, naming the file, the value, and
what the value is for. `tfsapp-hub publish` enforces the same rule on the
author's own machine, before anything is built or uploaded — the station's
`build/scripts/release.sh` used to be where this was checked; `publish` is
where it is checked now. It does **not** re-check when opening an app that is
already installed — an app that passed the gate on its way in should not become
unopenable later, and re-refusing it would make the hub reject something it
itself accepted. A dev session never installs, so neither check ever runs
against one — see §9.

Bump it when the app changes in a way that its data has to follow. The hub runs
the update lifecycle (§6) on the strength of this field alone; a source whose
code moved but whose version did not is, as far as the contract is concerned,
the same version of the app.

### Declaring off-window work

`workers` declares one or more background consumers, each an ordered,
non-empty list of Messenger transport names plus an optional `count` (a
number of identical copies, default `1`, capped at `4`). Declaring at least
one worker gets the app real Messenger transports it can dispatch to; declaring
none gets `sync://`, where handlers run inline. Either way the app's own code
is the same code, which is the point.

```json
{
  "workers": [
    { "transports": ["courant", "planifie", "fond"] },
    { "transports": ["urgent"] }
  ]
}
```

**A transport list's order is its priority, and there is no separate priority
key.** `messenger:consume a b c` is Symfony's own mechanism — a worker
rescans from the first transport after handling a single envelope — so a long
queued run never starves short interactive work on the same consumer. An app
that wants several transports to interleave in a given order declares them in
that order on one worker; several *workers* are for latency, letting one
transport's handlers run while another's are busy, never for throughput on one
transport and never for routing — the manifest declares transports and their
order, the app's own `framework.messenger.routing` decides what lands where.

`count` is copies of one declaration, default `1`. `DATABASE_URL` is always
SQLite, which serializes writers regardless, so extra copies pay off only for
handlers that spend time outside SQLite. A `count` above `4` falls back to
`4`, and a declaration naming a `scheduler_*` transport falls back to `count:
1` regardless of what was asked — a Scheduler transport consumed twice fires
every task twice — both with a printed reason, per the standing
fall-back-and-say-so rule. Naming the same transport twice, within one
declaration or across several, is refused rather than falling back: two
consumers on one transport is what `count` spells.

`async_worker: true` is kept as sugar for exactly one declaration consuming
`async`, with the DSN it always had — see §3 — so an already-installed app
migrates to `workers` at its own pace rather than under it. A manifest
spelling both keys is refused: two spellings of one thing, where guessing
which wins is worse than asking the author to pick one.

Two things about this declaration are deliberate and will not change even as
its shape does.

**Declaration is not consent.** The manifest says *this app has off-window
work*. Whether that work is allowed to continue once the window is closed — a
process living on someone's machine after they have closed the application — is
the user's decision and is asked for outside the manifest. A manifest never
grants itself a permanent process. Today nothing outlives the window, so the
question does not yet arise; when it does, it arrives as a consent step and not
as a manifest key.

**The app is told what the hub actually set out to run, not what it asked
for.** §3 injects the worker state as of launch, and the question it answers
is the one a user
message needs — *does my scheduled work continue once I close the window?* — not
"is something consuming right now". An app whose 8 a.m. task will not fire
because the window will be closed is entitled to say so, and can only say it if
the environment tells the truth about the machine rather than echoing the
manifest back.

The list above is plan 045's landed replacement for a boolean that hardcoded
one transport, an app using Symfony Scheduler having always needed
`scheduler_<name>` and often several transports at once. `async_worker`
remains, unchanged in meaning, as the one-line spelling for the common case.
The two paragraphs above are the ones a future change to this shape may not
touch.

### Declaring file associations

`file_associations` is how an app asks to appear in the desktop environment's
"Open with" menu for files of a given type:

```json
{
  "file_associations": { "mime_types": ["text/markdown", "application/json"] },
  "actions": { "open_files": { "ipc": true } }
}
```

The rules, each of which the hub enforces at parse time:

- **A declaration is a pair, never a lone key.** A nonempty `mime_types`
  without the `actions.open_files` receiver capability (§7) is refused: an app
  advertised in a menu whose selections it can never acknowledge would be a
  promise the manifest cannot keep. The reverse is fine — a receiver with no
  declared types can still be handed files through `tfsapp-hub open <id> --`
  (§7) and simply stays out of the file manager's menus.
- **Syntax is checked, existence is not.** Each entry must be a `type/subtype`
  pair, each side 1–127 characters from letters, digits and `!#$&^_.+-` — the
  restricted alphabet that excludes `;` (the desktop entry's field separator),
  `/`, whitespace and control characters. A type the host's MIME database has
  never heard of is still a valid declaration: this is a statement of what the
  *app* can open, not a claim about the host.
- **Directories are opted into, twice.** A local directory is not a file, and
  an app that has not asked for directories keeps the file-only behaviour it
  has always had. The opt-in is
  `"actions": { "open_files": { "ipc": true, "directories": true } }` — it is
  what lets the hub deliver a directory path through *any* launch path,
  including `open <id> -- <path>...`. Putting `inode/directory` in
  `mime_types` is the separate, advertising half: it is what makes the file
  manager offer the app for a directory. A manifest that declares
  `inode/directory` without the receiver and its `directories` option is
  invalid — a menu entry whose selections would be refused at the launch
  boundary. And `directories: true` without `ipc: true` is invalid too, for
  the pair rule's own reason: directories are delivered to the receiver, and
  this group has no other transport.
- **A declaration is not a permission.** MIME types are never part of app
  identity (the "One identity, several surfaces" table above), never content
  sniffing, and never a filesystem authorization: which files a running app
  may actually read remains the app's own backend's business, on every path
  alike — a request the hub delivers names paths; it grants nothing. A
  delivered directory path grants no access beyond what the app's backend
  already has: the hub neither enumerates nor copies its contents.

The hub renders a declaring app's generated desktop entry (see §2's "An
installed app has a desktop entry") with `MimeType=` from this list and an
`Exec=` line ending in `-- %F`, so the desktop environment passes the selected
paths as separate local-file arguments — the same entry shape for
`inode/directory` as for any file type. Which app the desktop environment
*prefers* for a type is the user's own default-application setting — outside
the manifest, outside this contract.

### `run`

Named `bin/console` aliases a user runs directly and interactively —
`tfsapp-hub run <id> <alias> [args...]` — as opposed to `commands`' four
launch-time hooks (§6), which the host runs unattended around an install or
an update:

```json
{
  "run": {
    "mcp-serve": { "command": "app:run:mcp-serve", "concurrent": true },
    "cleanup":   { "command": "app:run:cleanup" }
  }
}
```

| Key | Type | Meaning |
| --- | --- | --- |
| `command` | string, required | A `bin/console` argument string, split on whitespace and passed as `argv` directly — same no-shell-interpretation rule as `commands` (§6). |
| `concurrent` | boolean, optional | Default `false`. Whether this alias tolerates siblings — other instances of itself, other active `run` commands, an already-open window — in any arrival order, rather than requiring to run alone. Whether an alias tolerates siblings is the app author's to declare: only the author knows whether two instances may overlap, and the hub takes the declaration at face value, exactly as it takes every other manifest promise. See "Running a declared command" (§6) for the full gating. |

Each top-level key is the alias name a user types. `tfsapp-hub run <id>` with
no alias lists them back, naming each one's command and whether it is
`concurrent`. Running one is covered in §6.

### Keys this contract does not define

Unknown top-level keys are warned about and ignored (see "Standing rules"). One
key escapes that warning without being part of this contract: `releases_repo`,
which named the forge repository an app's release lived on for the archived
per-app packaging route, and may still sit in manifests written against it.
The hub accepts it silently and does nothing with it — deliberately, not for
lack of a use: `tfsapp-hub update <id>` resolves what its own registry
recorded at install or the last update, while `update <id> <archive.tar.gz>`
uses an explicit user-supplied archive. Neither reads a location out of the
source it is about to replace. A manifest that could redirect its own future updates would let
one good release permanently steer every machine that ever installed it. Only
what the hub itself wrote or the user explicitly supplies may steer a fetch; an app asks about its own updates
through §7, and how an update is *applied* belongs to the host.

`tfsapp-hub publish` reads nothing more out of it than `update` does: the
repository a release lands on comes from `--repo` or from the project's own
git remote, never from the manifest, so a stale `releases_repo` — naming a
downloads repository this route retired — cannot steer a publish either. The
key is accepted and read by nothing, on every path alike; an author who finds
one in an old manifest can take it as inert, not as a setting to update.
