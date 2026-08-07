#!/usr/bin/env bash
set -euo pipefail

# Run the workspace's `cargo test` inside a private D-Bus session backed by an
# ephemeral gnome-keyring-daemon, so the suite never touches the developer's
# real login keyring. The keyring-backed modules arrive with plan 007 and some
# of their tests deliberately exercise a *real* Secret Service; this wrapper is
# what gives them one to talk to without any risk to the host. It is in place
# from plan 001 on purpose — a test that reaches the login keyring once has
# already done the damage, so the isolation must never be the thing added
# afterwards.
#
# All arguments are passed through to `cargo test`, so e.g.
# `build/scripts/run-tests.sh --test keyring_health` still works.

if ! command -v dbus-run-session >/dev/null; then
  echo "run-tests.sh: 'dbus-run-session' not found on PATH — install 'dbus' (see README Prerequisites). It stands up the private bus the ephemeral Secret Service answers on for the tests; it is not a runtime dependency of the packaged app." >&2
  exit 1
fi

if ! command -v gnome-keyring-daemon >/dev/null; then
  echo "run-tests.sh: 'gnome-keyring-daemon' not found on PATH — install 'gnome-keyring' (see README Prerequisites). It provides an ephemeral, throwaway Secret Service for the tests to run against; it is not a runtime dependency of the packaged app and is unrelated to the GNOME desktop." >&2
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

XDG_DATA_HOME="$(mktemp -d)"
XDG_RUNTIME_DIR="$(mktemp -d)"
chmod 700 "$XDG_RUNTIME_DIR"

cleanup() {
  rm -rf "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"
}
trap cleanup EXIT

export XDG_DATA_HOME
export XDG_RUNTIME_DIR

# The workspace root, so `cargo test` covers both `core/` and `hub/`.
cd "$ROOT_DIR"

# gnome-keyring-daemon registers org.freedesktop.secrets on the private bus
# dbus-run-session hands it — that is all the `keyring` crate needs to find it,
# no exported control-socket vars required. `dbus-run-session` tears the whole
# private bus (and everything D-Bus-activated on it, including this daemon)
# down when the inner command exits, so nothing outlives this script.
dbus-run-session -- bash -c '
  set -euo pipefail
  gnome-keyring-daemon --daemonize --unlock --components=secrets <<< ""
  cargo test "$@"
' -- "$@"
