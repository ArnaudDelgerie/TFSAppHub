## The CLI grammar

> **`--flags` act on the hub. Bare words act on an app.**

One sentence, no ambiguous case. Two rules keep the dispatcher small:

- **It parses and routes; it never works.** Argv becomes a command and nothing
  else — no filesystem, no registry, no process — which is what makes every form
  in the grammar testable on a machine with nothing installed.
- **The whole surface is declared from the start**, including the parts no plan
  has implemented. A recognised-but-unavailable command answers "not implemented
  yet", never "unknown command" — a wrong message there sends someone hunting
  for a typo that is not present.

One carve-out is worth naming because it is easy to get wrong: after a `run`
alias, **every word belongs to the app's own command**. `run demo console --help`
asks the app's console for its help, not the hub for its own. The property, not
the flag, is what the test pins — forwarded arguments are the app's — because
every other host-level flag sits in the same trap the day an app declares an
alias that takes one.

A second carve-out, the only operand separator in the grammar: `open <id> -- <path>...` hands local paths to the app's declared receiver (§7) — regular files for any receiver, plus directories for one that opted into `actions.open_files.directories`. Everything
after the `--` is the caller's own operands — verbatim, never a flag, spaces
and Unicode and option-looking names included — and a separator with nothing
after it is the no-file form, because a declaring app's desktop entry ends in
`-- %F` and a bare menu launch expands `%F` to zero arguments.

`update <id> [<archive.tar.gz>] [--ref <tag>] [--yes]` accepts an
optional second positional only when it ends in `.tar.gz`. The archive wins
over the recorded source; `--ref` with it is refused by source resolution.
`publish <project> [--repo owner/repo | --local <dir>] [--yes]` makes those two
targets mutually exclusive at parse time.
