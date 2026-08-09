# Changelog

## Unreleased

Closing an app's window now tears its backend down in about a third of a
second, where it took 6.3 s — and tears it down *gracefully*, which it never
once did. Two independent causes, both of them invisible from the outside: a
process that had exited but not yet been reaped was counted as still running,
so the `SIGTERM`-then-`SIGKILL` escalation spent its full budget and then
killed a corpse on every single close; and the window was only hidden, so its
webview held a Mercure stream open against Caddy's graceful shutdown, which
waited for a client that was never going to let go. Teardown now destroys its
windows before signalling the server, and the escalation has become what it was
always meant to be — an exception path for something genuinely stuck.

For app authors, one thing is newly guaranteed and worth reading in CONTRACT.md
§4: PHP's shutdown functions actually run when the app closes, and a request
still in flight gets two seconds to finish before its connection is closed
under it. Work that needs longer than a request belongs in `async_worker` or in
a `run` command, as it always did — the difference is that this is now a stated
bound rather than an accident.

## 0.1.0

First packaged release: `make build` produces a repaired, self-contained
`TFSAppHub_<version>_amd64.AppImage` — its bundled FrankenPHP runs instead of
segfaulting, no symlink in it points outside itself, it carries no frozen
`libwayland-*`, and a `.versions.txt` beside it names the frozen WebKit/GTK
stack and the glibc floor it requires. Copied alone to a machine with no Rust,
no PHP and no Tauri toolchain, that one file installs a Symfony app, shows it
in the shell's grid under its own name and icon, opens it into a working
window, and removes it again. `docker compose run --rm build` (under `build/`)
rebuilds the same artifact against an older base for anyone the default
release does not run on.
