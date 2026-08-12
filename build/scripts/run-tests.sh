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

# gnome-keyring-daemon runs in the *foreground*, as a background child of the
# inner shell below rather than detached via `--daemonize` — its lifetime is
# therefore exactly this shell's, torn down by the trap on any exit (success,
# failure, or a future timeout), never left to a detached process that
# `dbus-run-session`'s own bus teardown might or might not reach. Before
# `cargo test` starts, the shell polls the private bus until
# `org.freedesktop.secrets` has an owner: D-Bus only *activates* a name's
# configured service when the name has no owner yet, so once this daemon
# holds it first, the tests can never trigger activation into a locked,
# unprompted daemon of their own (audit 004/P3 — the indefinite hang this
# guards against).
# The single quotes below are the point: this whole block is one script
# string handed to the inner `bash -c`, expanded by that shell, not this one.
# shellcheck disable=SC2016
dbus-run-session -- bash -c '
  set -euo pipefail

  gnome-keyring-daemon --foreground --unlock --components=secrets <<< "" &
  daemon_pid=$!
  trap "kill $daemon_pid 2>/dev/null" EXIT

  waited=0
  until dbus-send --session --print-reply --dest=org.freedesktop.DBus \
      /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner \
      string:org.freedesktop.secrets 2>/dev/null | grep -q "boolean true"; do
    if [ "$waited" -ge 100 ]; then
      echo "run-tests.sh: gnome-keyring-daemon (pid $daemon_pid) never took org.freedesktop.secrets on the private bus after 10s — see audit 004/P3." >&2
      exit 1
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

  # A generous ceiling: the suite itself runs in ~5s once compiled, but a
  # cold invocation compiles the whole workspace first, which alone can take
  # several minutes. 10 minutes leaves headroom for that while still making
  # sure a wedged run (audit 004/P3) fails loud instead of hanging a session
  # forever.
  status=0
  timeout 10m cargo test "$@" || status=$?
  if [ "$status" -eq 124 ]; then
    echo "run-tests.sh: cargo test did not finish within the 10 minute ceiling -- most likely the Secret Service wedge described in audit 004/P3 (a keyring crate call with no timeout of its own, blocked on a locked Secret Service). Check for a stray gnome-keyring-daemon on this session bus." >&2
  fi
  exit "$status"
' -- "$@"
