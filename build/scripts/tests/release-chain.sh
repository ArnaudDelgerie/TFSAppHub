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
assert_file_content() { # assert_file_content <description> <path> <expected>
  local desc="$1" path="$2" expected="$3" actual=""
  if [[ ! -f "$path" ]]; then
    echo "  assertion failed: $desc ($path does not exist)" >&2
    CASE_STATUS=1
    return
  fi
  actual="$(<"$path")"
  assert_equals "$desc" "$actual" "$expected"
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
  # Optional move, to prove release.sh re-checks the tree just before
  # publishing: dirty a tracked file, or commit it and move HEAD.
  if [[ "${FAKE_CARGO_MOVE:-}" == "dirty" ]]; then
    printf 'moved during build\n' >>"$PWD/../CHANGELOG.md"
  elif [[ "${FAKE_CARGO_MOVE:-}" == "commit" ]]; then
    printf 'moved during build\n' >>"$PWD/../CHANGELOG.md"
    git -C "$PWD/.." commit -q -am "moved during build"
  fi
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

write_record() { # write_record <version> <revision> — a planted .source-commit
  printf '%s\n' "$2" >"$TREE/target/release/bundle/appimage/TFSAppHub_${1}_amd64.source-commit"
}

plant_versions() { # plant_versions <version> — a planted .versions.txt
  printf 'glibc_floor=stub\n' \
    >"$TREE/target/release/bundle/appimage/TFSAppHub_${1}_amd64.versions.txt"
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
      | env FAKE_BIN_LOG="$LOG" FAKE_CARGO_MOVE="${FAKE_CARGO_MOVE:-}" \
          PATH="$FAKEBIN:$PATH" ./build/scripts/release.sh
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

# --- Step 2: provenance — clean, pushed, recorded, re-checked, named ----------

case_build_records_head() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  if run_build ""; then
    assert_file_content "the record is HEAD on a clean tree" \
      "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.source-commit" \
      "$(tree_head)"
  else
    echo "  assertion failed: build-hub.sh exited non-zero" >&2
    CASE_STATUS=1
  fi
  case_result "a build on a clean tree records HEAD in .source-commit"
}

case_build_records_dirty_head() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  printf 'local edit\n' >>"$TREE/CHANGELOG.md"
  if run_build ""; then
    assert_file_content "the record is HEAD-dirty on a dirty tree" \
      "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.source-commit" \
      "$(tree_head)-dirty"
  else
    echo "  assertion failed: build-hub.sh exited non-zero on a dirty tree" >&2
    CASE_STATUS=1
  fi
  case_result "a build on a dirty tree records HEAD-dirty in .source-commit"
}

case_release_refuses_dirty_tree() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  printf 'local edit\n' >>"$TREE/CHANGELOG.md"
  if run_release ""; then
    echo "  assertion failed: release.sh succeeded on a dirty tree" >&2
    CASE_STATUS=1
  else
    assert_match "the refusal names the fix (commit or stash)" \
      'commit or stash' "$CASE_DIR/run.err"
    assert_no_match "no cargo call" '^cargo ' "$LOG"
    assert_no_match "no release create" 'release create' "$LOG"
  fi
  case_result "release refuses a dirty tree before any gh call"
}

case_release_refuses_unpushed_commit() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  printf 'local edit\n' >>"$TREE/CHANGELOG.md"
  git -C "$TREE" commit -q -am "unpushed"
  if run_release ""; then
    echo "  assertion failed: release.sh succeeded with an unpushed commit" >&2
    CASE_STATUS=1
  else
    assert_match "the refusal names the fix (push)" 'push' "$CASE_DIR/run.err"
    assert_no_match "no cargo call" '^cargo ' "$LOG"
    assert_no_match "no release create" 'release create' "$LOG"
  fi
  case_result "release refuses a local commit that is not pushed"
}

case_release_refuses_branch_without_upstream() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  git -C "$TREE" switch -q -c side-branch
  if run_release ""; then
    echo "  assertion failed: release.sh succeeded on a branch without an upstream" >&2
    CASE_STATUS=1
  else
    assert_match "the refusal names the fix (push)" 'push' "$CASE_DIR/run.err"
    assert_no_match "no cargo call" '^cargo ' "$LOG"
    assert_no_match "no release create" 'release create' "$LOG"
  fi
  case_result "release refuses a HEAD without an upstream"
}

case_release_notes_name_the_revision() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  if run_release $'y\n'; then
    assert_equals "the notes end with Built from owner/TFSAppHub@HEAD" \
      "$(tail -n 1 "$LOG.notes")" "Built from owner/TFSAppHub@$(tree_head)"
    assert_match "the changelog section is the notes body" \
      'Notes body for 0\.3\.0' "$LOG.notes"
    assert_match "the release was created" 'release create' "$LOG"
  else
    echo "  assertion failed: release.sh exited non-zero on a clean, pushed tree" >&2
    CASE_STATUS=1
  fi
  case_result "happy path: the notes end with Built from owner/TFSAppHub@<HEAD>"
}

case_reuse_when_record_matches_head() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  plant_appimage "0.3.0" "existing build"
  plant_versions "0.3.0"
  write_record "0.3.0" "$(tree_head)"
  if run_release $'y\n'; then
    assert_no_match "no cargo call — the build was reused" '^cargo ' "$LOG"
    assert_match "the release was still created" 'release create' "$LOG"
  else
    echo "  assertion failed: release.sh exited non-zero reusing a matching build" >&2
    CASE_STATUS=1
  fi
  case_result "a build recorded from HEAD is offered for reuse (y: no rebuild)"
}

case_rebuild_when_record_differs_or_missing() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  plant_appimage "0.3.0" "existing build"
  write_record "0.3.0" "0000000000000000000000000000000000000000"
  if run_release $'y\n'; then
    assert_match "a differing record forces a rebuild" '^cargo ' "$LOG"
    assert_match "the release was still created" 'release create' "$LOG"
  else
    echo "  assertion failed: release.sh exited non-zero with a differing record" >&2
    CASE_STATUS=1
  fi
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  plant_appimage "0.3.0" "existing build"
  if run_release $'y\n'; then
    assert_match "a missing record forces a rebuild" '^cargo ' "$LOG"
    assert_match "the release was still created" 'release create' "$LOG"
  else
    echo "  assertion failed: release.sh exited non-zero with a missing record" >&2
    CASE_STATUS=1
  fi
  case_result "a build from another revision (or with no record) is rebuilt without prompting"
}

case_release_refuses_tree_moved_during_build() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  FAKE_CARGO_MOVE=dirty
  if run_release $'y\n'; then
    echo "  assertion failed: release.sh published after cargo dirtied the tree" >&2
    CASE_STATUS=1
  else
    assert_match "the refusal names the move" \
      'the tree moved during the build' "$CASE_DIR/run.err"
    assert_no_match "no release create" 'release create' "$LOG"
  fi
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  FAKE_CARGO_MOVE=commit
  if run_release $'y\n'; then
    echo "  assertion failed: release.sh published after cargo moved HEAD" >&2
    CASE_STATUS=1
  else
    assert_match "the refusal names the move" \
      'the tree moved during the build' "$CASE_DIR/run.err"
    assert_no_match "no release create" 'release create' "$LOG"
  fi
  FAKE_CARGO_MOVE=
  case_result "a tree that moved during the build is refused, nothing published"
}

# --- Step 3: publish the compatibility record --------------------------------

case_release_attaches_three_assets() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  if run_release $'y\n'; then
    assert_match "release create attaches AppImage, .versions.txt and sums" \
      'release create .*TFSAppHub_0\.3\.0_amd64\.AppImage .*TFSAppHub_0\.3\.0_amd64\.versions\.txt .*SHA256SUMS\.txt' "$LOG"
  else
    echo "  assertion failed: release.sh exited non-zero" >&2
    CASE_STATUS=1
  fi
  case_result "the AppImage, its .versions.txt and the sums are attached to the release"
}

case_sums_list_both_files() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  if run_release $'y\n'; then
    assert_equals "the sums list exactly two files" \
      "$(wc -l <"$TREE/target/release/bundle/appimage/SHA256SUMS.txt")" "2"
    assert_match "the AppImage is listed by bare name" \
      'TFSAppHub_0\.3\.0_amd64\.AppImage$' \
      "$TREE/target/release/bundle/appimage/SHA256SUMS.txt"
    assert_match "the record is listed by bare name" \
      'TFSAppHub_0\.3\.0_amd64\.versions\.txt$' \
      "$TREE/target/release/bundle/appimage/SHA256SUMS.txt"
    assert_no_match "no absolute paths in the sums" \
      '[[:space:]]/tmp/' "$TREE/target/release/bundle/appimage/SHA256SUMS.txt"
  else
    echo "  assertion failed: release.sh exited non-zero" >&2
    CASE_STATUS=1
  fi
  case_result "SHA256SUMS.txt lists both assets by bare name, two lines"
}

case_sums_verify_in_bundle_dir() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  if run_release $'y\n'; then
    if (cd "$TREE/target/release/bundle/appimage" && sha256sum -c SHA256SUMS.txt >/dev/null 2>&1); then
      :
    else
      echo "  assertion failed: sha256sum -c does not pass in the bundle directory" >&2
      CASE_STATUS=1
    fi
  else
    echo "  assertion failed: release.sh exited non-zero" >&2
    CASE_STATUS=1
  fi
  case_result "sha256sum -c passes in the bundle directory"
}

case_discard_removes_every_file_of_the_version() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  if run_release $'n\n'; then
    assert_missing "the AppImage is gone" \
      "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.AppImage"
    assert_missing "the .versions.txt is gone" \
      "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.versions.txt"
    assert_missing "the .source-commit is gone" \
      "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.source-commit"
    assert_missing "the sums are gone" \
      "$TREE/target/release/bundle/appimage/SHA256SUMS.txt"
  else
    echo "  assertion failed: release.sh exited non-zero" >&2
    CASE_STATUS=1
  fi
  case_result "answering n to keep leaves no file of that version behind"
}

case_release_refuses_missing_versions_record() {
  new_tree "0.3.0" "[0.3.0] - 2026-09-28"
  plant_appimage "0.3.0" "existing build"
  write_record "0.3.0" "$(tree_head)"
  rm -f "$TREE/target/release/bundle/appimage/TFSAppHub_0.3.0_amd64.versions.txt"
  # The record matches HEAD, so the reuse prompt appears — answer y; reusing
  # does not regenerate the .versions.txt, which is what this case removes.
  if run_release $'y\n'; then
    echo "  assertion failed: release.sh published without a .versions.txt" >&2
    CASE_STATUS=1
  else
    assert_match "the refusal names the build step" 'make build|build this version' "$CASE_DIR/run.err"
    assert_no_match "no release create" 'release create' "$LOG"
  fi
  case_result "a build without its .versions.txt is refused, not published"
}

# --- Step 4: only the three changelog spellings -------------------------------

case_heading_spellings_accepted() {
  local heading
  for heading in "0.3.0" "v0.3.0" "[0.3.0]" "[0.3.0] - 2026-09-28"; do
    new_tree "0.3.0" "$heading"
    if run_release $'y\n'; then
      assert_match "release create reached for '## $heading'" 'release create' "$LOG"
      assert_match "the section text is the notes for '## $heading'" \
        'Notes body for 0\.3\.0' "$LOG.notes"
    else
      echo "  assertion failed: '## $heading' was refused at the changelog gate" >&2
      CASE_STATUS=1
    fi
  done
  case_result "the three accepted spellings (optionally dated) each reach release create"
}

case_heading_spellings_refused() {
  local heading
  for heading in "[0.3.0" "0.3.0]" "0.3.0.1"; do
    new_tree "0.3.0" "$heading"
    if run_release ""; then
      echo "  assertion failed: '## $heading' was accepted by the changelog gate" >&2
      CASE_STATUS=1
    else
      assert_match "the refusal names the changelog gate" 'CHANGELOG' "$CASE_DIR/run.err"
      assert_no_match "no cargo call for '## $heading'" '^cargo ' "$LOG"
    fi
  done
  case_result "malformed headings are refused at the gate, before any build"
}

main() {
  case_build_with_older_appimage
  case_fix_stub_gets_current_version_only
  case_build_records_head
  case_build_records_dirty_head
  case_release_refuses_dirty_tree
  case_release_refuses_unpushed_commit
  case_release_refuses_branch_without_upstream
  case_release_notes_name_the_revision
  case_reuse_when_record_matches_head
  case_rebuild_when_record_differs_or_missing
  case_release_refuses_tree_moved_during_build
  case_release_attaches_three_assets
  case_sums_list_both_files
  case_sums_verify_in_bundle_dir
  case_discard_removes_every_file_of_the_version
  case_release_refuses_missing_versions_record
  case_heading_spellings_accepted
  case_heading_spellings_refused
  if [[ "$FAILED" -gt 0 ]]; then
    echo "release-chain: $FAILED case(s) failed" >&2
    exit 1
  fi
}
main "$@"
