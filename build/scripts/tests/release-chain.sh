#!/usr/bin/env bash
set -euo pipefail

# Isolated run of the hub's own release chain — build-hub.sh and release.sh
# against a fake `cargo`, a fake `gh` and a stub fix-appimage-bundle.sh, each
# case in its own throwaway git repository. `make check`'s fifth gate; nothing
# here touches the network, the real releases repo, or this checkout's own
# working tree.
#
# Every case builds a fresh tree under `mktemp -d` (removed on exit):
#   - a git repository holding copies of the real release.sh and
#     build-hub.sh — copies, not symlinks, since both derive ROOT_DIR from
#     their own location — a stub fix-appimage-bundle.sh that writes
#     `<stem>.versions.txt` for each argument and logs its arguments, a
#     hub/Cargo.toml with the version under test, non-empty
#     hub/resources/frankenphp and hub/resources/composer.phar, a
#     CHANGELOG.md, a build/releases-repo, and a .gitignore for target/ and
#     hub/resources/;
#   - a bare repository as `origin`, pushed with `-u`. The remote's URL is a
#     real GitHub one (https://github.com/owner/TFSAppHub.git) made to reach
#     the bare repository through `url.<bare>.insteadOf`, so release.sh's
#     repo-name parsing is tested without a network;
#   - a fake-bin directory first in PATH: `cargo` logs every call and, on
#     `tauri build`, writes the version's AppImage under the workspace-shared
#     bundle directory (its cwd is hub/, the workspace root one level up);
#     `gh` satisfies `auth status` and `repo view`, answers `release view`
#     with "no such release", and on `release create` logs its arguments and
#     copies the notes file. Every call lands in the case's log.
#
# Git is isolated from the host: no global or system config, author and
# committer through the environment. Answers to `read` prompts are fed on
# stdin, never left to a terminal. One line per case on stdout, assertion
# failures on stderr, non-zero exit if any case failed.

export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME="Release Chain Test"
export GIT_AUTHOR_EMAIL="release-chain@example.invalid"
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TMP_ROOTS=()
cleanup() {
  local d
  for d in "${TMP_ROOTS[@]}"; do rm -rf "$d"; done
}
trap cleanup EXIT

FAILED=0
CASE_STATUS=0

ok() { echo "ok: $1"; }
fail() { echo "FAIL: $1"; FAILED=$((FAILED + 1)); }

assert_match() { # assert_match <description> <ERE> <file>
  local desc="$1" pattern="$2" file="$3"
  if grep -qE -- "$pattern" "$file" 2>/dev/null; then return 0; fi
  echo "  assertion failed: $desc (no match for '$pattern' in $file)" >&2
  CASE_STATUS=1
}
assert_no_match() { # assert_no_match <description> <ERE> <file>
  local desc="$1" pattern="$2" file="$3"
  if grep -qE -- "$pattern" "$file" 2>/dev/null; then
    echo "  assertion failed: $desc ('$pattern' matched in $file)" >&2
    CASE_STATUS=1
  fi
}
assert_exists() { # assert_exists <description> <path>
  local desc="$1" path="$2"
  if [[ -e "$path" ]]; then return 0; fi
  echo "  assertion failed: $desc ($path does not exist)" >&2
  CASE_STATUS=1
}
assert_missing() { # assert_missing <description> <path>
  local desc="$1" path="$2"
  if [[ ! -e "$path" ]]; then return 0; fi
  echo "  assertion failed: $desc ($path exists)" >&2
  CASE_STATUS=1
}
assert_equals() { # assert_equals <description> <actual> <expected>
  local desc="$1" actual="$2" expected="$3"
  if [[ "$actual" == "$expected" ]]; then return 0; fi
  echo "  assertion failed: $desc (got '$actual', expected '$expected')" >&2
  CASE_STATUS=1
}

case_result() { # case_result <name> — one line per case on stdout
  if [[ "$CASE_STATUS" -eq 0 ]]; then
    ok "$1"
  else
    fail "$1"
    local f
    for f in "$CASE_DIR/run.out" "$CASE_DIR/run.err"; do
      if [[ -f "$f" && -s "$f" ]]; then
        echo "  --- $(basename "$f") of the failed run:" >&2
        sed 's/^/  /' "$f" >&2
      fi
    done
  fi
  CASE_STATUS=0
}

# new_tree <version> <heading-line> — a fresh, clean, pushed tree for one
# case, plus its fake-bin and call log. Sets TREE, ORIGIN, FAKEBIN, LOG.
new_tree() {
  local version="$1" heading="$2"
  CASE_DIR="$(mktemp -d)"
  TMP_ROOTS+=("$CASE_DIR")
  TREE="$CASE_DIR/tree"
  ORIGIN="$CASE_DIR/origin.git"
  FAKEBIN="$CASE_DIR/fake-bin"
  LOG="$CASE_DIR/calls.log"
  : >"$LOG"

  mkdir -p "$TREE/build/scripts" "$TREE/hub/resources" "$FAKEBIN"

  cp "$SCRIPTS_DIR/release.sh" "$TREE/build/scripts/release.sh"
  cp "$SCRIPTS_DIR/build-hub.sh" "$TREE/build/scripts/build-hub.sh"

  cat >"$TREE/build/scripts/fix-appimage-bundle.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "fix-appimage-bundle $*" >>"$FAKE_BIN_LOG"
for appimage in "$@"; do
  printf 'glibc_floor=stub\n' >"${appimage%.AppImage}.versions.txt"
done
EOF
  chmod +x "$TREE/build/scripts/release.sh" "$TREE/build/scripts/build-hub.sh" \
    "$TREE/build/scripts/fix-appimage-bundle.sh"

  cat >"$TREE/hub/Cargo.toml" <<EOF
[package]
name = "tfsapp-hub"
version = "$version"
EOF

  printf 'stub sidecar\n' >"$TREE/hub/resources/frankenphp"
  printf 'stub composer\n' >"$TREE/hub/resources/composer.phar"

  cat >"$TREE/CHANGELOG.md" <<EOF
# Changelog

## Unreleased

Nothing yet.

## $heading

Notes body for $version.
EOF

  printf 'owner/TFSAppHub-releases\n' >"$TREE/build/releases-repo"
  printf 'target/\nhub/resources/\n' >"$TREE/.gitignore"

  cat >"$FAKEBIN/cargo" <<'EOF'
#!/usr/bin/env bash
set -u
echo "cargo $*" >>"$FAKE_BIN_LOG"
if [[ "${1:-}" == "tauri" && "${2:-}" == "build" ]]; then
  version="$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "$PWD/Cargo.toml")"
  out="$PWD/../target/release/bundle/appimage"
  mkdir -p "$out"
  printf 'stub image\n' >"$out/TFSAppHub_${version}_amd64.AppImage"
fi
exit 0
EOF

  cat >"$FAKEBIN/gh" <<'EOF'
#!/usr/bin/env bash
set -u
echo "gh $*" >>"$FAKE_BIN_LOG"
cmd="${1:-}" sub="${2:-}"
if [[ "$cmd" == "auth" && "$sub" == "status" ]]; then exit 0; fi
if [[ "$cmd" == "repo" && "$sub" == "view" ]]; then exit 0; fi
if [[ "$cmd" == "release" ]]; then
  if [[ "$sub" == "view" ]]; then
    echo "release not found" >&2
    exit 1
  fi
  if [[ "$sub" == "create" ]]; then
    notes="" prev=""
    for arg in "$@"; do
      if [[ "$prev" == "--notes-file" ]]; then notes="$arg"; fi
      prev="$arg"
    done
    [[ -z "$notes" ]] || cp "$notes" "$FAKE_BIN_LOG.notes"
    exit 0
  fi
fi
echo "fake gh: unexpected invocation: $*" >&2
exit 1
EOF
  chmod +x "$FAKEBIN/cargo" "$FAKEBIN/gh"

  git init -q -b main "$TREE"
  git init -q --bare "$ORIGIN"
  git -C "$TREE" remote add origin https://github.com/owner/TFSAppHub.git
  git -C "$TREE" config "url.$ORIGIN.insteadOf" https://github.com/owner/TFSAppHub.git
  git -C "$TREE" add -A
  git -C "$TREE" commit -q -m "throwaway tree"
  git -C "$TREE" push -q -u origin main
}

plant_appimage() { # plant_appimage <version> <content> — a pre-existing build
  local dir="$TREE/target/release/bundle/appimage"
  mkdir -p "$dir"
  printf '%s\n' "$2" >"$dir/TFSAppHub_${1}_amd64.AppImage"
}

tree_head() { git -C "$TREE" rev-parse HEAD; }

# The script under test runs with its stdout and stderr captured, so a case
# prints exactly its one result line; the capture is dumped on stderr only
# when the case fails.
run_build() { # run_build <stdin> — build-hub.sh inside the tree
  (
    cd "$TREE"
    printf '%s' "$1" \
      | env FAKE_BIN_LOG="$LOG" PATH="$FAKEBIN:$PATH" ./build/scripts/build-hub.sh
  ) >"$CASE_DIR/run.out" 2>"$CASE_DIR/run.err"
}

run_release() { # run_release <stdin> — release.sh inside the tree
  (
    cd "$TREE"
    printf '%s' "$1" \
      | env FAKE_BIN_LOG="$LOG" PATH="$FAKEBIN:$PATH" ./build/scripts/release.sh
  ) >"$CASE_DIR/run.out" 2>"$CASE_DIR/run.err"
}

# --- Step 1: build the version being released --------------------------------

case_build_with_older_appimage() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  plant_appimage "0.2.0" "old build"
  if run_build ""; then
    assert_exists "the current version's AppImage is built" \
      "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.AppImage"
  else
    echo "  assertion failed: build-hub.sh exited non-zero with an older-version AppImage beside the new one" >&2
    CASE_STATUS=1
  fi
  case_result "build succeeds with an older-version AppImage in the bundle directory"
}

case_fix_stub_gets_current_version_only() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  plant_appimage "0.2.0" "old build"
  if run_build ""; then
    assert_match "the fix stub received the current version's AppImage" \
      '^fix-appimage-bundle .*TFSAppHub_0\.3\.0_amd64\.AppImage$' "$LOG"
    assert_no_match "the older AppImage never reached the fix stub" \
      'TFSAppHub_0\.2\.0' "$LOG"
  else
    echo "  assertion failed: build-hub.sh exited non-zero" >&2
    CASE_STATUS=1
  fi
  case_result "only the current version's AppImage reaches the fix stub"
}

main() {
  case_build_with_older_appimage
  case_fix_stub_gets_current_version_only
  if [[ "$FAILED" -gt 0 ]]; then
    echo "release-chain: $FAILED case(s) failed" >&2
    exit 1
  fi
}
main "$@"
