## 1. What an app must provide

A TFSApp is a Symfony project directory. The hub is pointed at one — a local
path, or a published release it downloads over HTTPS (see "Publishing a
release" below) — and that directory must contain:

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

### Publishing a release

Everything above is what an app needs to be installed from a local path or a
clone. **To be installable remotely** —
`tfsapp-hub install github:owner/repo` — it needs one more thing: a published
release. An app that never publishes one is not in breach of anything; it
stays local-only, installed from a directory or a clone, for as long as its
author wants.

A release is, on the app's own forge repository:

- a tag `v<app_version>` — `--ref` selects a release this way, never a branch
  or a commit;
- an asset named `<project_name>-<app_version>.tar.gz`, holding the permitted
  Git-tracked source tree at that commit: no `vendor/`, `var/`, `node_modules/`
  or `.git/`, built frontend assets committed like any other tracked file. When
  the hub publishes it, the archive's paths, bytes, executable and symlink
  modes, manifest and changelog all come from that one pushed commit — never a
  later working-tree state — so no step of the pipeline gets a remote special
  case;
- a `SHA256SUMS.txt` beside it, one `<sha256>  <filename>` line naming the
  archive, checked before a single byte of it reaches the app root;
- a `## <version>` heading in `CHANGELOG.md` naming what changed.

All four are reachable by hand — `git archive` starts from the same tracked
tree, with the stated exclusions applied before `sha256sum` names the result.
`tfsapp-hub publish path/to/project` automates exactly this sequence and is the
convenience, never the requirement.

The convenience needs two things a hand-built release does not: `gh` **and**
`git`, both installed and authenticated on the **author's own machine** — the
one command where the hub runs `git`, and only there (decision 003, "the hub
publishes apps"). The project must be committed and pushed; `publish` proves
it rather than trusting an asserted tag, and the release lands on whichever
repository that pushed commit's own remote names.

