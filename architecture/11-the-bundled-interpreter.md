## The bundled interpreter

The hub ships one FrankenPHP and one `composer.phar`, fetched at build time by
`make resources`. That is not only a packaging step: the hub installs *and* runs
every app with them, so no system-wide PHP is required — and nothing works
without them.

**Resolved in packaged/dev/system order.** `platform::bundled_frankenphp` and
`php::bundled_composer` each return an ordered list of candidates: the
packaged AppImage's own resource dir first — read via
`tauri::utils::platform::resource_dir`, which needs no `AppHandle` and so
works from `install`, a headless CLI command that runs before any
`tauri::Builder` exists — then `<hub>/resources/`, where `make sidecar` and
`make composer` download them for a build run from source.
`core::sidecar::resolve_frankenphp_binary` walks that list and falls back to
a system-wide `/usr/bin/frankenphp` if neither resolves (`composer.phar` has
no such fallback — the hub never runs an app's Composer with anything but its
own). A candidate only counts when it is a real, non-empty file — `hub/build.rs`
writes 0-byte stubs at both paths so a fresh clone still compiles before
`make resources` has run, and a stub must never be mistaken for the download.

### The `PHP_BINARY` shim

`frankenphp php-cli` reports **no path for itself**: `PHP_BINARY` is empty.

That constant is the first thing Symfony's `PhpExecutableFinder` consults, and
it is how every PHP tool re-invokes "the interpreter running me" — Composer's
`@php`, Flex's auto-scripts, anything shelling out through `symfony/process`.
Empty, the finder walks on and picks up whatever `php` the *machine* has.
Measured, under the bundled 8.5.8:

```
> @php -r "echo PHP_VERSION, ' at ', PHP_BINARY;"
script ran under PHP 8.4.23 at /usr/bin/php8.4
```

No warning, no error — an app resolved against one interpreter quietly running a
piece of itself on another. On a machine with no PHP at all, the same call
simply fails, and that machine is the normal case for a desktop user.

A second, smaller problem came with it: `frankenphp php-cli` accepts a script or
`-r` and **no PHP CLI options**. `-d`, `-n`, `-v`, `-m` are each read as a
filename — which matters because Composer appends three `-d ini=value` options to
every `@php` it spawns.

Both are fixed by a `bin/php` shim under the hub's root that drops those options
and re-enters `frankenphp php-cli`, exported as `PHP_BINARY` and prepended to
`PATH` for every command the hub starts. It is rewritten on every use, for the
same reason the Caddyfile is.

