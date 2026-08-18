## Four measurements that shaped the code

Recorded because each one explains why a piece of code looks the way it does,
and each was silent enough that it would otherwise be "simplified" away.

**A 512×512 window icon never reaches the window property.** GDK writes it as
`2 + width × height` words and refuses past its selection size limit, which caps
at 262144; a 512×512 RGBA icon needs 262146 and misses by two words. It does not
warn and does not error — the property is simply never set and the window comes
up with a generic icon. Hence the downscale-and-say-so in `identity.rs`. The
full-size file stays the one every other surface uses.

**`--help` and `--version` were read past the point where argv stops being the
host's.** See the carve-out above; the fix is that `run`'s tail is claimed before
the host's own flag parser sees it.

**`PHP_BINARY` is empty under `frankenphp php-cli`.** See the shim above.

**`/proc/<pid>` outlives the process, so "is it still running" answered yes for
a corpse.** Nothing in this process reaps a child while the code that signalled
it is still waiting on it, so a child that obeyed its `SIGTERM` in 260 ms kept
its `/proc` entry for the whole three-second budget and was then `SIGKILL`ed
after the fact. Every teardown ended that way; both budgets were spent in full,
every time, and the six seconds were read as "FrankenPHP and the worker are slow
to stop" for two plans. `process_exists` reads `/proc/<pid>/stat`'s state field
for that reason, and the escalation went from being the rule to being an
exception path.

All four were found here rather than reasoned about, and the reason is
structural: the hub is the first thing to run this code path **twice in one
process image, from argv rather than from a build**, which makes it the first
place a silent default is visible as a difference.

