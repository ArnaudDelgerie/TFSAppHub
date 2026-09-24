# Architecture

How TFSAppHub is built, and why it is built that way. The companion document is
[`CONTRACT.md`](CONTRACT.md), which states what the hub and an app promise each
other; nothing here is a promise to an app, and anything an app could depend on
belongs there instead.

---

## The problem

A Symfony application can be a perfectly good desktop application. Making it one
means shipping a PHP interpreter, a web server and a browser engine alongside it,
and that is where the cost lives: those three are native, they are large, and
they are compiled against the machine that built them.

The obvious answer — package each app as its own self-contained binary — makes
that cost recur. Every app pays a build pipeline, every app author needs a Rust
toolchain and GTK development headers, and every release re-answers the question
of which glibc and which WebKitGTK it links against, on whichever machine
happened to run the build.

The hub inverts it. **The fragile native half is built once. Everything added
afterwards is PHP.**

## The model

One binary installs, updates and runs N Symfony applications from their source,
using its own bundled FrankenPHP as the interpreter.

Three properties follow, and they are the whole argument:

**`composer install` *is* the compatibility manifest.** It runs at install time
with the very interpreter that will later serve the app. There is no linking, no
ABI surface, and no baseline to get wrong — the question "will this app run on
this machine?" is answered by Composer's own platform requirements against a PHP
the hub controls.

**Per-app update is `git fetch`.** An app is source at a pinned ref. Updating it
is re-resolving that ref and replaying the lifecycle, not rebuilding and
redistributing a 150 MB binary.

**N binaries collapse to 1.** The disk win comes from sharing the *binary*, never
from sharing the process — see "One process per open app" below.

The cost is honest and worth stating: the hub is a single point of failure, in a
way N independent binaries were not. A hub that will not start takes every
installed app with it. That is accepted, and its mitigation is deliberately
deferred rather than pretended away.

---

## Table of contents

- [The two crates](architecture/01-the-two-crates.md) — the `core`/`hub` split, and why.
- [Where things live](architecture/02-where-things-live.md) — the OS data dir layout, one directory per installed app.
- [Runtime identity: one binary, N applications](architecture/03-runtime-identity.md) — distinct app id, `WM_CLASS`, cookie store, per launch.
- [Installing](architecture/04-installing.md) — the install pipeline: snapshotting, dependencies, lifecycle commands.
- [Updating](architecture/05-updating.md) — the update/rollback pipeline, the pre-update snapshot.
- [Exporting and importing](architecture/06-exporting-and-importing.md) — the curated archive, rescue dumps.
- [Removing and purging](architecture/07-removing-and-purging.md) — what `remove` leaves, what `purge` deletes.
- [Publishing a release](architecture/08-publishing-a-release.md) — the publish pipeline, its gates, and `gh`.
- [Opening an app](architecture/09-opening-an-app.md) — the launch sequence, the sidecar, the splash, close guards, teardown.
- [Running a declared command](architecture/10-running-a-declared-command.md) — `run`, its locks, its concurrency.
- [The bundled interpreter](architecture/11-the-bundled-interpreter.md) — FrankenPHP, `PHP_BINARY`, revalidation.
- [The registry, and what a hub update does to installed apps](architecture/12-the-registry.md) — `registry.json`, hub self-update, revalidation.
- [The CLI grammar](architecture/13-the-cli-grammar.md) — the `--flags`-on-hub / bare-word-on-app dispatcher.
- [Four measurements that shaped the code](architecture/14-four-measurements.md) — the numbers behind the design.
- [Packaging](architecture/15-packaging.md) — building the AppImage.
- [Source layout](architecture/16-source-layout.md) — the repo's file map.

---

## Open questions

The queued design work lives in `.project/plan/000-index.md` rather than here, so
that one list stays authoritative. The structural decisions that outrank
everything else are in `.project/decision/`.

The one worth naming here, because it shapes what is above rather than extending
it: the hub is a single point of failure for every installed app, and nothing in
this architecture mitigates that yet.
