## 1. What an app must provide

A TFSApp is a Symfony project directory. The hub is pointed at one — a local
release archive, or a published release it downloads over HTTPS
(see "Publishing a release" below) — and that directory must contain:

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
app's kernel must honour three of §3's variables: `APP_CACHE_DIR`,
`APP_BUILD_DIR` and `APP_LOG_DIR`.

Symfony's own kernel already does: `MicroKernelTrait` reads all three on every
Symfony version the bundle supports (^7.4), so the `src/Kernel.php` that
`symfony/skeleton` generates needs no change, and is what the reference app
uses. The obligation only bites an app that overrides `getCacheDir()`,
`getBuildDir()` or `getLogDir()` itself: the override must keep honouring the
variable. What the contract does not tolerate is a kernel that ignores them:
the host would inject the three variables and the app would silently write
into a directory an update replaces.

**The manifest is read by the hub, never by PHP.** The app learns its own
identity from the environment (§3), not by parsing its own
`tfsapp.config.json` — so the same code runs unchanged under any deployment
that sets those variables. An app that reads the manifest at runtime has coupled
itself to being installed, which is exactly what §3 exists to avoid.

**Frontend assets ship built.** The host resolves PHP dependencies with its own
interpreter at install time; it does not run Node, and it never builds assets.
An app with a frontend build step builds its output itself and declares the
directories it produces under `build_outputs` in its manifest (§2); `publish`
copies them into the release archive as they stand on the author's machine.
An app that neither commits nor declares its output ships pages that arrive
without it. This is the same boundary the lifecycle draws in §6 — the developer
builds on their machine, the host installs and serves — and it is what keeps an
app's prerequisites down to "a Symfony project".

**What is in the release is what gets installed.** A release archive is built
from the tree that is committed, so whatever is committed to it is what the
hub installs. Keep the source you publish from clean of anything you would
not ship.

**The app's own source is never where its data lives.** The project directory is
read-only from the app's point of view: the database, cache, sessions, logs and
secrets all go to a per-app data directory the hub provides and names in the
environment (§3, §5). A project that writes into its own `var/` will find that
directory belongs to the installed snapshot, and that an update replaces it.

### Publishing a release

Everything above is what an app needs to ship as a release — the only thing
the hub installs. **To be installable** — `tfsapp-hub install
github:owner/repo` from a forge, or `tfsapp-hub install <archive.tar.gz>`
from a local release — it needs exactly that: a published release. An app
that never publishes one is not in breach of anything; it stays a project
the hub can only run in `dev`, for as long as its author wants.

A release consists of `<project_name>-<app_version>.tar.gz` and a matching
`SHA256SUMS.txt` beside it, on a forge or in a folder. The checksum is checked
before extraction, including for `install <archive.tar.gz>`; the archive alone
cannot be installed. On a forge, the release also has:

- a tag `v<app_version>` — `--ref` selects a release this way, never a branch
  or a commit;
- an asset named `<project_name>-<app_version>.tar.gz`, holding the permitted
  Git-tracked source tree at that commit: no `vendor/`, `var/`, `node_modules/`
  or `.git/`, plus the directories the manifest declares under `build_outputs`.
  When the hub publishes it, the archive's paths, bytes, executable and symlink
  modes, manifest and changelog all come from that one pushed commit — never a
  later working-tree state — so no step of the pipeline gets a remote special
  case. The declared `build_outputs` are the one stated exception: the author's
  bytes as they stand at publish time, gitignored in the project, embedded
  as-is and covered by the same checksum;
- a `SHA256SUMS.txt` beside it, one `<sha256>  <filename>` line naming the
  archive, checked before a single byte of it reaches the app root;
- a `## <version>` heading in `CHANGELOG.md` naming what changed.

All four are reachable by hand — `git archive` starts from the same tracked
tree, with the stated exclusions applied, the declared `build_outputs`
directories appended beside it, before `sha256sum` names the result.
`tfsapp-hub publish path/to/project` automates exactly this sequence and is the
convenience, never the requirement.

One of `publish`'s gates asks rather than checks: when the manifest turns on
`actions.secrets.ipc`, it says that the declared secrets become reachable
from the app's own JavaScript — an XSS in the app can read or overwrite them
(§7) — and waits for an explicit yes at a terminal. `--yes` does not answer
it, and a run with no terminal is refused: the release ships that setting to
everyone who installs it, so its author confirms it in person, every release.

For a release without a forge, `tfsapp-hub publish path/to/project --local <dir>`
writes `<dir>/<project_name>-<app_version>/` with the archive,
`SHA256SUMS.txt` and `NOTES.md`. The destination directory must exist. This
mode keeps the manifest, canonical version, clean Git tree and changelog
gates, and the `secrets.ipc` confirmation above; it does not require an upstream, remote, `gh` or
an unused forge tag. See decision 009 for the distinction. The release folder
must not already exist.

Forge publishing needs `git` installed and `gh` installed and authenticated
on the **author's own machine** — the one command where the hub runs `git`,
and only there (decision 003, "the hub publishes apps"). The project must be
committed and pushed; `publish` proves it rather than trusting an asserted
tag, and the release lands on the GitHub repository the branch's upstream
remote names — or on the one `--repo owner/repo` names instead. Either way the
release is tagged on that pushed commit, so the repository it lands on has to
hold it. Local publishing needs `git` and a clean committed tree, but no push
or forge authentication.
