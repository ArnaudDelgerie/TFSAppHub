## Publishing a release

`publish path/to/project [--repo owner/repo | --local <dir>]` is `git.rs`/`gh.rs`'s pair
(`publish.rs`), and the mirror image of "Resolving a release" above: that
path downloads and verifies an archive a release already carries; this one
builds the archive and hands it to the one call that makes a release exist.
Gates, then archive, then checksums, then one `gh` call — the same order
`CONTRACT.md`'s "Publishing a release" states as an artefact, run backwards.

1. **Gates 1–8, all local, all before anything is built.** `git.rs` pins one
   clean commit that is reachable from the pushed upstream, then reads that
   commit's tree and blobs — never a later `HEAD` or worktree path. Its
   manifest supplies canonical `MAJOR.MINOR.PATCH` validation (1–2), its `CHANGELOG.md`
   supplies the verbatim `## <version>` notes (7), and only its
   `actions.secrets.ipc` can require the deliberately non-bypassable
   confirmation (8). The same snapshot resolves the repository — `--repo` if
   given, otherwise the upstream remote parsed as `owner/repo` (3–6). This is
   the one command where the hub runs `git` at all — decision 003's narrow
   exception to decision 002's "never shells out to `git`", scoped to the
   author's own machine.
2. **Gates 9–11, `gh.rs`'s `Gh`.** `gh` is installed (9) and authenticated
   (10); no release already carries the tag `v<app_version>`, draft included
   (11) — each its own refusal, checked before the archive is ever built.
3. **The archive**, only once every gate above has passed: the pinned Git tree
   supplies each path, mode and blob; the same exclusion rules `tree_hash`
   already applies on the install side filter entries before their blobs are
   requested. Rust packs them into `<project_name>-<app_version>.tar.gz` in a
   scratch directory under the hub's own root — removed on success or failure
   alike, exactly as install's own scratch is. A later editor save cannot alter
   either the archive or the manifest-derived tag and notes.
4. **`SHA256SUMS.txt`**, hashed over the archive just built, beside it in the
   same scratch directory.
5. **The announcement**, then one confirmation (this one does honour
   `--yes`) — the repository, the pinned commit and branch, the tag, the
   archive and its size and hash, and the notes extracted from that commit.
6. **One `gh release create --target <sha>` call**, the sha gate 3–6 already
   proved is on the forge — so the tag lands on the exact commit the archive
   was built from, never on a separately-pushed tag that could name a
   different one. A failed asset upload is inspected (`gh release view`) and,
   only when the release is missing an asset, deleted, so a retry is a clean
   retry rather than the version guard refusing the author's own wreckage.

**Two boundaries worth stating once.** The hub holds no forge credential
anywhere in this path — the one authenticated call is `gh`'s, already
installed and authenticated on the author's machine, and the hub never
prompts for, stores, or reads a token. And `publish` writes nothing into the
project: no build runs, no file is generated beside `tfsapp.config.json`, no
tag is pushed by the hub itself — `git.rs` only ever reads the project's
state, it never runs a command that changes it. `publish` is reachable from
no bridge route, no IPC command and no manifest key; an app never triggers
its own publish.

### Local publication

`--local <dir>` selects a directory that must already exist, and cannot be
combined with `--repo`. The local snapshot pins `HEAD` after checking the
project root and clean worktree; it does not inspect an upstream or remote.
The manifest and canonical version gates (1–2), changelog notes gate (7) and
secrets confirmation (8) still run. Forge reachability gates (5–6) and `gh`
gates (9–11) do not; instead, an existing `<name>-<version>` folder is
refused. No `gh` command is used.

The archive and sums are built in a hidden temporary directory inside
`<dir>`. `NOTES.md` holds the changelog section followed by the pinned commit
SHA. All three files are fsynced. After the announcement and confirmation,
the temporary directory is renamed to `<dir>/<name>-<version>` and `<dir>` is
fsynced. A refusal or decline removes the temporary directory. The project
tree is never written.
