## Running a declared command

`run <id> <alias>` executes one of an app's own declared `bin/console`
commands in the foreground, from `hub/src/run.rs` — a transplant of the
station's own module, with one thing added in front of it: which app.
Everything it stands on already lived in `core::process` (the locks, the
signal machinery, the process-group teardown), because the station had
already built and measured it; the hub's own addition is the app-resolution
step, and the fact that it has two apps' worth of this state to keep apart —
`runs/` is keyed on the app's own `identifier`, the same key the two locks
above already use, so two different apps running commands at once costs
nothing.

### A lifecycle gate beside the three activity locks

| Lock | Answers | Read by |
| --- | --- | --- |
| `hub/locks/<identifier>.lifecycle.lock` | Is any installed lifecycle activity or maintenance operation retained for this identifier? | installed `open`, foreground `run`, and every installed maintenance pipeline |
| `serving.lock` | Will handing this launch's argv to that process get you a window right now? | a launch's own hand-off probe |
| `sidecar.pid.lock` (the liveness lock) | Does a process still own this data dir at all? | a launch's reap, and `run`'s own rule-3 concurrency probe |
| `runs/` | Is each entry's launcher live, or did its child outlive it? | `run`'s lifted rule 2, launch's rule-3-in-reverse refusal, and data-dir writers |

The lifecycle gate is a non-blocking `flock` over a hub-root file, deliberately
separate from the app data tree: `install` can reach it before that tree exists
and `purge` keeps it while removing the tree. Activity holders use a shared
lease; maintenance holders use an exclusive lease and record their operation
name only after acquiring it. The record improves a busy error but is not
authority — a stale record never blocks a free flock. A window keeps its shared
handle in Tauri-managed state until process teardown, a foreground `run` keeps
it through `child.wait()`, and maintenance keeps its exclusive handle through
confirmation, Composer/hooks and every write. The descriptors are close-on-exec
so a Composer or hook child cannot accidentally retain the gate. Different
identifiers map to different files and proceed independently.

Rule 3's "is a window live" probe deliberately reads the **liveness** lock,
not the serving one: a process mid-teardown still owns the data dir, and a
`bin/console` command opening the app's SQLite while its server is being
killed is the same hazard as one opening it next to a live server. A launch's
own refusal reads `runs/` the other way round — only once it has already
decided to launch (past the serving/liveness probe above), never against a
sibling it is about to hand off to, since a `concurrent` alias legitimately
running beside an already-open window must not be blocked by a second window
opening beside it.

Between that probe and the refusal sits one more question: has this launch
anything to run at all? The scan is resolved against the manifest and
combined with the version guard's own verdict (`crate::run::launch_verdict`)
— a launch with nothing to run opens beside every `concurrent` command, one
with a lifecycle event to perform or a non-`concurrent` holder refuses either
way. See `.project/decision/005-concurrency-belongs-to-the-alias.md` for why.

`runs/` separates the two questions per entry: each entry's flock answers
whether its launcher still lives, while its durable record — `<alias>`, then
`<alias>\n<pid>` once the child is spawned — answers what it was running when
the flock has already gone. Every guard consults each entry's flock and, when
it is free, identity-proves that recorded pid through `TFS_APP_IDENTIFIER`; a
live match is an orphaned active command, while a dead or mismatched pid is
stale — the same ternary plan 035 gave the single file, now applied per
entry. That lets refusals name the alias, and lets `run --stop`/`--replace`
know what to signal without ever killing a command as a side effect of `open`
or a data-dir writer.

A `run` launcher creates and locks `runs/<launcher pid>.lock`, writes the
alias, and only then scans the other entries for rule 2. It excludes its own
path from the verdict. Whoever takes a lock second sees the first launcher's
entry, so two non-`concurrent` newcomers cannot both start; if their scans
cross, both may refuse. After spawning, the launcher appends `\n<child pid>`
to its alias record without truncating it. A scan never unlinks an unlocked
entry while the pid in its *file name* is alive, even if its record is empty
or stale: that entry may have been created just before its flock was taken.
An identity-proven active orphan is still reported regardless of the
file-name pid.

### Forwarding a signal past `Child::wait()`'s own retry

`std::process::Child::wait()` silently retries when the underlying wait is
interrupted by a caught signal, which means a thread blocked in it never
learns that `SIGINT`/`SIGTERM` arrived — a handler alone cannot forward what
the thread actually waiting never sees. The fix is the standard self-pipe
trick: the signal handler itself only writes one byte to a pipe (the one
thing sound to do inside a handler), and a *separate* thread blocks reading
that pipe and reacts once a byte arrives, leaving the thread in `wait()`
alone. `core::process::install_signal_forwarding` sets the pipe and the
handlers up once; `spawn_signal_forwarder` is what reacts to a caught signal,
terminating the child through it. Plan 047 deleted the coexistence watchdog
that used to share this mechanism for a second reason (a window's owner
dropping) — a `run` command's lifetime belongs to whoever started it, not to
a window that happened to be open at the time
(`.project/decision/005-concurrency-belongs-to-the-alias.md`, point 4).

### Hub-side, and never `tauri`

`run.rs` depends on nothing from `tauri` — not the crate, not a running
`Builder`, not GTK. It runs to completion and exits before any window could
exist, exactly like `install`/`list`/`remove`. That is not a style
preference: a `run` command is an interactive terminal subcommand with the
terminal's own stdio inherited, and the app it runs beside may or may not
have a window open at all — nothing about it belongs behind the point where
Tauri claims the process.
