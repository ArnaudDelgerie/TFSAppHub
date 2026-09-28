#!/usr/bin/env bash
set -euo pipefail

# Run the explicit production-keyring smoke tests inside a private D-Bus
# session backed by an ephemeral gnome-keyring-daemon, so they never touch
# the developer's real login keyring. Its repository-owned bus configuration
# has no activation service directories: the daemon below is the sole
# explicit Secret Service provider, and the wrapper verifies its PID before
# Cargo can use it.
#
# All arguments are passed through to `cargo test`; Make supplies the ignored
# production-backend tests that this harness exists to run. The verified
# daemon PID is exported as TFS_TEST_SECRET_SERVICE_PID before Cargo starts,
# so the frozen-service test can SIGSTOP exactly that daemon — the only one
# the test is ever allowed to touch.

if ! command -v dbus-run-session >/dev/null; then
  echo "make keyring-integration: 'dbus-run-session' not found on PATH — install 'dbus' (see README Prerequisites). It stands up the private bus the ephemeral Secret Service answers on for this check; it is not a runtime dependency of the packaged app." >&2
  exit 1
fi

if ! command -v gnome-keyring-daemon >/dev/null; then
  echo "make keyring-integration: 'gnome-keyring-daemon' not found on PATH — install 'gnome-keyring' (see README Prerequisites). It provides an ephemeral, throwaway Secret Service for this check; it is not a runtime dependency of the packaged app and is unrelated to the GNOME desktop." >&2
  exit 1
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DBUS_CONFIG_FILE="$ROOT_DIR/build/scripts/test-session.conf"

XDG_DATA_HOME="$(mktemp -d)"
XDG_RUNTIME_DIR="$(mktemp -d)"
COMPOSER_ROOT="$XDG_DATA_HOME/composer"
# Composer's XDG data directory is $XDG_DATA_HOME/composer. Its separate
# per-user home and cache are explicit too, so a test run cannot read or write
# the developer's global Composer configuration or cache.
COMPOSER_HOME="$COMPOSER_ROOT/home"
COMPOSER_CACHE_DIR="$COMPOSER_ROOT/cache"
mkdir -p "$COMPOSER_HOME" "$COMPOSER_CACHE_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

cleanup_diagnostic() {
  local path="$1"
  local operation="$2"
  local status="$3"

  printf 'run-tests.sh: cleanup failed: operation=%s path=%s status=%s\n' \
    "$operation" "$path" "$status" >&2

  if command -v findmnt >/dev/null; then
    printf 'run-tests.sh: mount information for %s:\n' "$path" >&2
    findmnt -T "$path" -o TARGET,SOURCE,FSTYPE,OPTIONS 2>&1 || true
  fi

  if command -v fuser >/dev/null; then
    printf 'run-tests.sh: processes using %s:\n' "$path" >&2
    fuser -v "$path" 2>&1 || true
  fi
}

cleanup_path() {
  local path="$1"
  local status=0

  if timeout 10s rm -rf -- "$path"; then
    return 0
  else
    status=$?
  fi

  cleanup_diagnostic "$path" "timeout 10s rm -rf --" "$status"
  return "$status"
}

cleanup_paths() {
  local cleanup_status=0
  local path

  # gvfsd-fuse mounts a FUSE filesystem at $XDG_RUNTIME_DIR/gvfs on demand;
  # its own teardown races this trap when the private bus goes away, so
  # `rm -rf` can hit it while still mounted ("Device or resource busy" — the
  # gvfs failure diagnosed in audit 004/P3, there attributed to the
  # XDG_DATA_HOME dir rather than this one). Force-unmount it first,
  # quietly and best-effort: it is often already gone by the time we get
  # here, and it is never ours to leave mounted either way.
  timeout 5s fusermount3 -uz "$XDG_RUNTIME_DIR/gvfs" 2>/dev/null || true

  # This is deliberately a narrow, test-only seam: it retains only paths the
  # wrapper recorded as its own, so the failure diagnostic is testable without
  # relying on an intermittent external leak.
  if [ "${RUN_TESTS_TEST_CLEANUP_FAILURE:-}" = "1" ]; then
    cleanup_diagnostic "$COMPOSER_ROOT" "test-only cleanup failure seam" "forced"
    cleanup_status=1
  else
    cleanup_path "$COMPOSER_ROOT" || cleanup_status=1
    cleanup_path "$XDG_DATA_HOME" || cleanup_status=1
    cleanup_path "$XDG_RUNTIME_DIR" || cleanup_status=1
  fi

  for path in "$COMPOSER_ROOT" "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"; do
    if [ -e "$path" ]; then
      cleanup_diagnostic "$path" "post-cleanup existence check" "still exists"
      cleanup_status=1
    fi
  done

  return "$cleanup_status"
}

cleanup() {
  local test_status=$?
  local cleanup_status=0

  trap - EXIT
  cleanup_paths || cleanup_status=$?

  if [ "$cleanup_status" -ne 0 ]; then
    if [ "$test_status" -eq 0 ]; then
      echo "run-tests.sh: tests passed but cleanup failed; exiting with cleanup status 70." >&2
      exit 70
    fi
    echo "run-tests.sh: cleanup failed after test status $test_status; preserving the original test failure status." >&2
  fi

  exit "$test_status"
}
trap cleanup EXIT

export XDG_DATA_HOME
export XDG_RUNTIME_DIR
export COMPOSER_HOME
export COMPOSER_CACHE_DIR

if [ "${RUN_TESTS_RECORD_PATHS:-}" = "1" ]; then
  echo "run-tests.sh: harness-owned paths: XDG_DATA_HOME=$XDG_DATA_HOME XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR COMPOSER_ROOT=$COMPOSER_ROOT COMPOSER_HOME=$COMPOSER_HOME COMPOSER_CACHE_DIR=$COMPOSER_CACHE_DIR" >&2
fi

# The workspace root, so `cargo test` covers both `core/` and `hub/`.
cd "$ROOT_DIR"

# gnome-keyring-daemon runs in the *foreground*, as a background child of the
# inner shell below rather than detached via `--daemonize` — its lifetime is
# therefore exactly this shell's, torn down by the trap on any exit (success,
# failure, or a future timeout), never left to a detached process that
# `dbus-run-session`'s own bus teardown might or might not reach. Before
# `cargo test` starts, the shell polls the private bus until
# `org.freedesktop.secrets` belongs to that exact child. `NameHasOwner` alone
# only says *someone* owns the name, which previously let an activation-started,
# locked daemon through. The activation-free bus makes this daemon the only
# possible provider; the unique-name-to-PID check below proves it before Cargo
# can use the service.
# The single quotes below are the point: this whole block is one script
# string handed to the inner `bash -c`, expanded by that shell, not this one.
# shellcheck disable=SC2016
dbus-run-session --config-file="$DBUS_CONFIG_FILE" -- bash -c '
  set -euo pipefail

  startup_failure() {
    echo "run-tests.sh: Secret Service startup failed: $1" >&2
    exit 1
  }

  # `unix:runtime=yes` must resolve to the wrapper-owned directory, never to
  # a socket in /tmp or on the developer session. Validate the listener before
  # starting the provider, then expose it on request for external cleanup
  # assertions.
  bus_socket="${DBUS_SESSION_BUS_ADDRESS#unix:path=}"
  bus_socket="${bus_socket%%,*}"
  if [ -z "$bus_socket" ] || [[ "$bus_socket" != "$XDG_RUNTIME_DIR/"* ]] || [ ! -S "$bus_socket" ]; then
    startup_failure "private D-Bus socket is not a socket below XDG_RUNTIME_DIR: ${DBUS_SESSION_BUS_ADDRESS:-unset}"
  fi
  if [ "${RUN_TESTS_RECORD_PATHS:-}" = "1" ]; then
    echo "run-tests.sh: Secret Service bus: socket=$bus_socket" >&2
  fi

  gnome-keyring-daemon --foreground --unlock --components=secrets <<< "" &
  daemon_pid=$!

  cleanup_daemon() {
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  }
  trap cleanup_daemon EXIT

  waited=0
  while :; do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
      startup_failure "gnome-keyring-daemon (pid $daemon_pid) exited before owning org.freedesktop.secrets"
    fi

    owner_name="$(dbus-send --session --print-reply --dest=org.freedesktop.DBus \
      /org/freedesktop/DBus org.freedesktop.DBus.GetNameOwner \
      string:org.freedesktop.secrets 2>/dev/null | awk '\''/string/ { gsub(/"/, "", $2); print $2; exit }'\'' || true)"

    if [ -n "$owner_name" ]; then
      owner_pid="$(dbus-send --session --print-reply --dest=org.freedesktop.DBus \
        /org/freedesktop/DBus org.freedesktop.DBus.GetConnectionUnixProcessID \
        string:"$owner_name" 2>/dev/null | awk '\''/uint32/ { print $2; exit }'\'')"

      if ! [[ "$owner_pid" =~ ^[0-9]+$ ]]; then
        startup_failure "GetConnectionUnixProcessID returned an unparsable PID for $owner_name"
      fi

      # Narrow test-only seam for the failed-proof path. It cannot affect a
      # normal run and demonstrates that Cargo never starts after a mismatch.
      if [ "${RUN_TESTS_TEST_OWNER_PROOF_FAILURE:-}" = "1" ]; then
        owner_pid=$((daemon_pid + 1))
      fi

      if [ "$owner_pid" != "$daemon_pid" ]; then
        startup_failure "org.freedesktop.secrets owner PID $owner_pid does not match harness daemon PID $daemon_pid"
      fi

      if [ "${RUN_TESTS_RECORD_PATHS:-}" = "1" ]; then
        echo "run-tests.sh: Secret Service ownership: daemon_pid=$daemon_pid owner_pid=$owner_pid" >&2
      fi
      break
    fi

    if [ "$waited" -ge 100 ]; then
      startup_failure "gnome-keyring-daemon (pid $daemon_pid) did not own org.freedesktop.secrets after 10s"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done

  # The frozen-service test SIGSTOPs exactly this daemon — the PID verified
  # above, never a PID guessed from the process table, so the developer
  # session gnome-keyring-daemon cannot be touched by it.
  export TFS_TEST_SECRET_SERVICE_PID="$daemon_pid"

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
