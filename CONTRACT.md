# The TFSApp contract

This document defines what **TFSAppHub** (the hub) and a **Symfony application**
promise each other. An app that honours it can be installed by the hub and
opened as a desktop window, with no fork of the hub and no per-app build step.
It is written for **app authors**: contributors to the hub itself start from
[`ARCHITECTURE.md`](ARCHITECTURE.md), and users from the [README](README.md).

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

## Table of contents

- [§1 — What an app must provide](contract/1-what-an-app-must-provide.md) — the layout, the manifest, the one HTTP route, and publishing a release.
- [§2 — `tfsapp.config.json`](contract/2-tfsapp-config-json.md) — the manifest's required and optional fields, identity, semver, workers, `run`, file associations.
- [§3 — The environment the app runs in](contract/3-the-environment-the-app-runs-in.md) — the environment variables every app process receives.
- [§4 — The HTTP contract](contract/4-the-http-contract.md) — headers, `/healthz`, the CSP, Mercure authorization, graceful shutdown.
- [§5 — The app's own state, and what it is isolated from](contract/5-the-apps-own-state.md) — the data directory, `APP_SECRET`, sessions, isolation, export/import.
- [§6 — Lifecycle](contract/6-lifecycle.md) — the install/update lifecycle commands, the no-overlap guarantee, `run`.
- [§7 — Native capabilities: `actions`](contract/7-native-capabilities-actions.md) — `secrets`, `update`, `picker`, `open_files`, `close_guard`, and the bridge wire contract.
- [§8 — What a host cannot honour, and how you find out](contract/8-what-a-host-cannot-honour.md) — what falls back, and how the app is told.
- [§9 — Running a project in dev](contract/9-running-a-project-in-dev.md) — `tfsapp-hub dev`: live source, its own identity, its own data.

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
