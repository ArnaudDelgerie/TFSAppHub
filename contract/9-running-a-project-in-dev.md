## 9. Running a project in dev

`tfsapp-hub dev path/to/project` runs the same app against its **live source**
— served in place, never snapshotted — for a developer who has not installed
it. This section says what an app author can rely on there, in the same terms
as the rest of this document: what the app sees, never how the hub does it.

### What differs, and it is a closed list

- `APP_ENV=dev` and `APP_DEBUG=1`, not `prod` and `0`.
- Every §3 path — `APP_CACHE_DIR`, `APP_BUILD_DIR`, `APP_LOG_DIR`,
  `APP_SESSION_DIR` — is rooted at the project's own `var/` instead of an OS
  data directory.
- `DATABASE_URL` points at `var/data/app.db` inside the project — **still
  SQLite, still the host's to set**, so §3's requirement that migrations run
  on SQLite binds in dev exactly as it does once installed.
- `APP_SECRET` is a fixed, throwaway constant, never the installation's
  generated-and-kept value (§5). It buys the dev loop — a fresh random secret
  on every relaunch would log the developer out each time — and nothing signed
  with it is meant to outlive the session.
- The keyring service is a namespace of its own, so a secret set from a dev
  session and one set from the installed app (§5, §7) never collide and never
  meet.
- The runtime identity is its own, `dev.<identifier>` — the window class, the
  GTK application id, the single-instance key, the cookie store, the data
  directory, the keyring namespace, all of it (§2). This is what lets the same
  project be open in dev and installed at once, in two windows, on two
  databases, neither able to see the other's.
- A dev session installs nothing, so it has no desktop entry. Its window falls
  back to the class-derived label §2 describes — `dev.<identifier>` rather
  than `product_name` — which is a cost paid by the one person who can be told
  why: the developer running it.

### What does not differ

§3's variable list is the same list, in the same names and the same order.
§4's HTTP contract holds exactly. §5's isolation holds — a dev session gets
its own data directory, its own cookie store, its own liveness lock, by the
same identifier-keyed rules an installed app gets; it is simply a different
identifier. §7's `actions` use the same available transports and ACL boundaries
in dev as when installed — including `picker`'s IPC-only boundary — and
`actions.update` already answers the same way in both: `unavailable` /
`local_source`, since a dev session has no release feed to compare against
regardless of which mode is asking.

### No install, so no install lifecycle

§6's `pre-install`, `post-install`, `pre-update` and `post-update` never run
for a dev session. They are events an app passes through once, on arrival or
on update, and a dev session is neither: the project was never installed, and
nothing will ever "update" it out from under a developer editing it live.
Whatever `pre-install` would have set up — typically
`doctrine:migrations:migrate` — is the developer's own to run, against the
injected `DATABASE_URL`, the same way any Symfony project's migrations run
outside the hub. `app_version`'s install/update bookkeeping (§2) plays no part
either: nothing compares it, nothing is refused for a downgrade, nothing is
recorded — an author is free to edit it while iterating.

### The footprint: `var/`, and nothing else

The hub serves the project's source **in place** and never modifies it — the
same guarantee §1 states for what installing does to a source tree, read from
the running-from-source side instead. The one directory the hub writes into is
`var/`, where the §3 paths above root the app's cache, build, log and session
directories (its kernel honours them, §1); nothing outside it is ever touched.

### The guardrail

The hub does not watch the project, does not compile anything, and does not
build any asset. It serves the source as it finds it on each request and
relaunches it on demand — nothing more. An app whose frontend needs a build
step builds it with its own tooling, exactly as §1 already requires of an
installed app: dev changes where the source lives, never who is responsible
for building it. When the answer to "can the hub watch and rebuild for me?"
comes up, it is no — the developer's own build tool already has a `--watch`.

