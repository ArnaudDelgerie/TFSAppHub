#!/usr/bin/env bash
set -euo pipefail

# Small-scale, artisanal release flow — the unsigned alternative to
# tauri-plugin-updater + signing. One command builds the hub's own AppImage
# and publishes it to a public releases repo as GitHub release assets. Ported
# from TFSAppWorkstation's script of the same name; two things do not come
# across, both properties of an *app* being released rather than of the hub
# itself: the per-project `releases_repo` resolution/write-back (the hub has
# no `tfsapp.config.json` of its own — see below), and the
# `actions.secrets.ipc` confirmation gate.
#
# End to end:
#   1. Provenance gate: the working tree must be clean and its HEAD pushed to
#      the branch's upstream; that HEAD is pinned as the revision this release
#      is of. A dirty tree, an unpushed or detached HEAD is refused with the
#      fix named — local and cheapest first, before any gh call.
#   2. Version guard: tag = v<hub version>; stop if that release exists.
#   3. Changelog gate: CHANGELOG.md must exist at the repo root with one of
#      three spellings — "## <version>", "## v<version>", "## [<version>]" —
#      each optionally followed by anything from a space on (a date, a link);
#      no other spelling is accepted. That section becomes the release notes,
#      nothing more, and the notes end with "Built from <repo>@<sha>": the
#      provenance of the binary, not an authenticity claim (decision 004).
#   4. Build in Docker (`docker compose run --rm build` from build/) on the
#      official Ubuntu LTS base (plan 074), or reuse an AppImage already under
#      target/release/bundle/appimage/ — offered only when its
#      .source-commit reads exactly the pinned HEAD; prompted once, default
#      rebuild. A build from any other revision is rebuilt without prompting.
#   5. Verify the chosen image was built on the official Dockerfile base,
#      then write SHA256SUMS.txt for the AppImage and its .versions.txt.
#   6. Re-check: the tree must still be clean, HEAD still the pinned revision,
#      and the chosen build's .source-commit must still read it — otherwise
#      refuse, publish nothing.
#   7. Publish: create the releases repo if it does not exist, then
#      gh release create with the .AppImage, its .versions.txt and
#      the checksums as assets (GitHub object storage, never committed — the
#      repo clone stays light). Every gh call that writes comes after every
#      refusal. A fresh build (not a reused one) is then
#      offered for cleanup — keep on disk by default, or discard all four
#      files.
#
# Unlike the station, and unlike this script's own per-*app* release.sh
# ancestor: the version comes from hub/Cargo.toml, the hub's own version —
# there is no tfsapp.config.json here, the hub is not an app.
#
# The releases repo is fixed by build/releases-repo, not prompted for: the hub
# is one project with one release stream. A fork edits that file, which is
# the shared source of truth read here
# and by hub/build.rs (TFSAPP_RELEASES_REPO, for hub self-update in
# release.rs) — resolved relative to this script's own directory so it works
# from anywhere it's invoked from.
#
# Requires: gh, authenticated (`gh auth login`). Public releases repo only:
# the eventual anonymous in-app update check carries no token, so a private
# repo is out of scope.
#
# Usage: build/scripts/release.sh

die() { echo "release: $*" >&2; exit 1; }

[[ $# -eq 0 ]] || die "usage: $0 (no arguments — releases the hub itself)"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_TOML="$ROOT_DIR/hub/Cargo.toml"

RELEASES_REPO_FILE="$ROOT_DIR/build/releases-repo"
[[ -f "$RELEASES_REPO_FILE" ]] \
  || die "no build/releases-repo file at $RELEASES_REPO_FILE"
RELEASES_REPO="$(grep -vE '^[[:space:]]*(#|$)' "$RELEASES_REPO_FILE" | head -n1)"
[[ -n "$RELEASES_REPO" ]] \
  || die "build/releases-repo ($RELEASES_REPO_FILE) has no value line"

# --- Preconditions ----------------------------------------------------------
[[ -f "$CARGO_TOML" ]] || die "hub/Cargo.toml not found at $CARGO_TOML"

APP_VERSION="$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "$CARGO_TOML")"
[[ -n "$APP_VERSION" ]] \
  || die "could not read a \"version\" line from $CARGO_TOML"

# --- Step 1: provenance — clean, pushed, pinned ------------------------------
# Local and cheapest first, before any gh call: a release ties its notes, its
# tag and its binary to one known revision of this tree, the way the app
# publish path pins a pushed commit. A dirty tree or an unpushed HEAD is
# refused with the fix named; a detached HEAD has no upstream and is refused
# the same way. No `git fetch` — the same local knowledge the app gates use.
if [[ -n "$(git -C "$ROOT_DIR" status --porcelain)" ]]; then
  die "the working tree is dirty — commit or stash, then release."
fi
UPSTREAM="$(git -C "$ROOT_DIR" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null)" \
  || die "HEAD has no upstream — push the branch, then release."
if [[ "$(git -C "$ROOT_DIR" rev-list --count '@{u}..HEAD')" -ne 0 ]]; then
  die "HEAD is not pushed to $UPSTREAM — push, then release."
fi
SHA="$(git -C "$ROOT_DIR" rev-parse HEAD)"

# owner/name for the notes' Built from line, from the *raw* remote URL —
# `git remote get-url` would apply insteadOf and name a stand-in. A
# non-GitHub URL is kept verbatim.
UPSTREAM_REMOTE="${UPSTREAM%%/*}"
REPO_URL="$(git -C "$ROOT_DIR" config --get "remote.${UPSTREAM_REMOTE}.url")" \
  || die "no URL recorded for remote '$UPSTREAM_REMOTE' — cannot name the source repository."
REPO_SLUG="$REPO_URL"
case "$REPO_URL" in
  https://github.com/*|ssh://git@github.com/*|git@github.com:*)
    REPO_SLUG="${REPO_SLUG#https://github.com/}"
    REPO_SLUG="${REPO_SLUG#ssh://git@github.com/}"
    REPO_SLUG="${REPO_SLUG#git@github.com:}"
    REPO_SLUG="${REPO_SLUG%.git}"
    ;;
esac

if ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  die "Docker Compose is unavailable — install Docker with the Compose plugin and start the Docker service, then release."
fi
command -v gh >/dev/null 2>&1 \
  || die "gh (GitHub CLI) is not installed — see https://cli.github.com"
gh auth status >/dev/null 2>&1 \
  || die "gh is not authenticated. Run: gh auth login"

REPO="$RELEASES_REPO"

# --- Step 2: version guard --------------------------------------------------
TAG="v$APP_VERSION"

if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  die "release $TAG already exists on $REPO — bump the version in hub/Cargo.toml first."
fi
echo "Releasing $TAG"

# --- Step 3: changelog gate --------------------------------------------------
# Hard prerequisite: CHANGELOG.md must exist at the repo root and carry a
# level-2 heading for $APP_VERSION. The version has to come from a known,
# predictable place — but not in one single spelling: "## 1.2.0", "## v1.2.0"
# and "## [1.2.0]" (Keep a Changelog) all count, with anything after the
# version on the same line (a date, a link) ignored. What is *not* accepted
# is a heading that merely contains the version, so "## 1.2.0.1" never
# answers for 1.2.0.
#
# The content below the matched heading becomes the release notes verbatim —
# never the whole file, never a generic fallback — extracted to a temp file
# cleaned up on exit. The extractor below matches the heading with this same
# regex: the two must never drift apart, or the gate would pass on a heading
# the extractor then fails to find, publishing empty notes.
CHANGELOG_FILE="$ROOT_DIR/CHANGELOG.md"
[[ -f "$CHANGELOG_FILE" ]] \
  || die "no CHANGELOG.md found at $CHANGELOG_FILE."

# Bracket expressions rather than backslash escapes throughout: the regex is
# handed to awk through -v, which would eat the backslashes ("\." is not an
# awk escape sequence). "." and "+" are the only ERE metacharacters a semver
# string can contain. The heading itself is a closed list of three spellings
# and no others — "## 1.2.0", "## v1.2.0", "## [1.2.0]" — each optionally
# followed by anything from a space on (a date, a link). The brackets are part
# of the heading or absent entirely, never one without the other, and the
# version must end at the heading's end: "## 1.2.0.1" does not answer for
# 1.2.0.
VERSION_RE="${APP_VERSION//+/[+]}"
VERSION_RE="${VERSION_RE//./[.]}"
HEADING_RE="^## ([[]v?${VERSION_RE}[]]|v?${VERSION_RE})([[:space:]].*)?$"

grep -qE "$HEADING_RE" "$CHANGELOG_FILE" \
  || die "CHANGELOG.md has no '## $APP_VERSION' heading for version $APP_VERSION — '## v$APP_VERSION' and '## [$APP_VERSION]' are accepted too, optionally followed by a date."

NOTES_FILE="$(mktemp)"
trap 'rm -f "$NOTES_FILE"' EXIT
awk -v heading_re="$HEADING_RE" '
  $0 ~ heading_re { found=1; next }
  found && /^## / { exit }
  found { print }
' "$CHANGELOG_FILE" >"$NOTES_FILE"

# Provenance in the notes: which repository, which revision — the link a
# release in a separate repo has to the source that produced it. Integrity,
# not authenticity (decision 004): the line names where the binary came
# from; it signs nothing.
printf '\nBuilt from %s@%s\n' "$REPO_SLUG" "$SHA" >>"$NOTES_FILE"

# --- Step 4: build (or reuse) ------------------------------------------------
OUTPUT_DIR="$ROOT_DIR/target/release/bundle/appimage"
shopt -s nullglob
existing=("$OUTPUT_DIR/TFSAppHub_${APP_VERSION}_"*.AppImage)
FRESH_BUILD=1

if [[ ${#existing[@]} -gt 1 ]]; then
  die "found ${#existing[@]} AppImages matching TFSAppHub_${APP_VERSION}_*.AppImage in $OUTPUT_DIR — ambiguous, resolve by hand first."
elif [[ ${#existing[@]} -eq 1 ]]; then
  # Reuse only a build that came from this exact HEAD: the record written
  # beside the AppImage by build-hub.sh says which revision produced it.
  # Anything else — an older revision, or no record at all — is rebuilt
  # without prompting.
  existing_image="${existing[0]}"
  recorded=""
  if [[ -f "${existing_image%.AppImage}.source-commit" ]]; then
    recorded="$(<"${existing_image%.AppImage}.source-commit")"
  fi
  if [[ "$recorded" == "$SHA" ]]; then
    read -r -p "Found an existing build for $TAG — reuse it instead of rebuilding? [y/N] " REUSE
    if [[ "$REUSE" =~ ^[Yy]$ ]]; then
      APPIMAGE="$existing_image"
      FRESH_BUILD=0
      echo "Reusing $APPIMAGE"
    fi
  else
    [[ -n "$recorded" ]] || recorded="no record"
    echo "existing build is from $recorded, HEAD is $SHA — rebuilding"
  fi
fi

if [[ -z "${APPIMAGE:-}" ]]; then
  (cd "$ROOT_DIR/build" && docker compose run --rm build)
  appimage=("$OUTPUT_DIR/TFSAppHub_${APP_VERSION}_"*.AppImage)
  [[ ${#appimage[@]} -eq 1 ]] || die \
    "expected exactly one AppImage matching TFSAppHub_${APP_VERSION}_*.AppImage in $OUTPUT_DIR, found ${#appimage[@]}"
  APPIMAGE="${appimage[0]}"
fi

# --- Step 5: checksums ------------------------------------------------------
# The compatibility record the build wrote beside the AppImage — the file a
# user downloads from the same release to read its glibc floor (see README
# "It does not start"). A release without it is undiagnosable, so a missing
# record is a missing build, not a warning.
VERSIONS="${APPIMAGE%.AppImage}.versions.txt"
[[ -f "$VERSIONS" ]] \
  || die "no $(basename "$VERSIONS") beside $(basename "$APPIMAGE") — build this version first (\`cd build && docker compose run --rm build\`), which writes the record, then release."

# A matching .source-commit proves the revision, not the build environment:
# a local `make build` can carry the same SHA and a newer glibc floor. Read
# the official base from the Dockerfile so the April 2027 switch has one
# source of truth. This checks build provenance at release time; it is not a
# general GLIBC/GLIBCXX floor gate inside the build.
BASE_IMAGE_DEFAULT="$(sed -nE 's/^ARG BASE_IMAGE=([^[:space:]]+)[[:space:]]*$/\1/p' "$ROOT_DIR/build/Dockerfile")"
if [[ ! "$BASE_IMAGE_DEFAULT" =~ ^ubuntu:([0-9]{2}\.[0-9]{2})$ ]]; then
  die "build/Dockerfile must have exactly one ARG BASE_IMAGE=ubuntu:YY.MM default for the official release."
fi
EXPECTED_BUILD_HOST_OS="Ubuntu ${BASH_REMATCH[1]}"
BUILD_HOST_OS="$(sed -n 's/^build_host_os=//p' "$VERSIONS")"
case "$BUILD_HOST_OS" in
  "$EXPECTED_BUILD_HOST_OS"|"$EXPECTED_BUILD_HOST_OS".*|"$EXPECTED_BUILD_HOST_OS "*) ;;
  *) die "AppImage build_host_os='${BUILD_HOST_OS:-<missing>}' differs from official base '$EXPECTED_BUILD_HOST_OS' ($BASE_IMAGE_DEFAULT). Rebuild with cd build && docker compose run --rm build; if the host built into the shared target/, run rm -rf target from the repo root first." ;;
esac

# Generated from inside target/release/bundle/appimage/ so the file records a
# bare filename and not this machine's absolute path. Both assets are listed:
# the record is downloaded and checked against the same sums as the AppImage.
SUMS="$OUTPUT_DIR/SHA256SUMS.txt"
( cd "$OUTPUT_DIR" && sha256sum "$(basename "$APPIMAGE")" "$(basename "$VERSIONS")" ) >"$SUMS"
echo "Checksums:"
cat "$SUMS"

# --- Step 6: re-check before publishing --------------------------------------
# The build ran with the tree live on disk; nothing may have moved it between
# the provenance gate and now. Anything that did means the artifact no longer
# corresponds to $SHA — refuse rather than publish.
if [[ -n "$(git -C "$ROOT_DIR" status --porcelain)" ]] \
   || [[ "$(git -C "$ROOT_DIR" rev-parse HEAD)" != "$SHA" ]]; then
  die "the tree moved during the build — nothing published."
fi
recorded_chosen=""
if [[ -f "${APPIMAGE%.AppImage}.source-commit" ]]; then
  recorded_chosen="$(<"${APPIMAGE%.AppImage}.source-commit")"
fi
if [[ "$recorded_chosen" != "$SHA" ]]; then
  die "the tree moved during the build — nothing published."
fi

# --- Step 7: publish --------------------------------------------------------
# Every refusal is now behind us; from here on the script writes outside this
# machine. The releases repo is created only now — creating it before a later
# refusal would leave an empty repo behind a failed release.
if gh repo view "$REPO" >/dev/null 2>&1; then
  echo "Releases repo: $REPO (exists)"
else
  echo "Creating public releases repo $REPO ..."
  gh repo create "$REPO" --public --add-readme \
    --description "Downloads and release notes for TFSAppHub." \
    || die "failed to create $REPO"
fi

# Release notes are exactly the section extracted by the changelog gate
# (step 3) — --generate-notes can't work cross-repo, the releases repo has
# none of this repo's commits.
gh release create "$TAG" \
  --repo "$REPO" \
  --title "$TAG" \
  --notes-file "$NOTES_FILE" \
  "$APPIMAGE" "$VERSIONS" "$SUMS"

echo "Published $TAG to $REPO."

# A reused artifact was already on disk before this run and is left alone —
# only a build this run actually produced is offered for cleanup.
if [[ "$FRESH_BUILD" -eq 1 ]]; then
  read -r -p "Keep the build (AppImage + .versions.txt + .source-commit + SHA256SUMS.txt) on disk? [Y/n] " KEEP
  if [[ "$KEEP" =~ ^[Nn]$ ]]; then
    rm -f "$APPIMAGE" "$VERSIONS" "${APPIMAGE%.AppImage}.source-commit" "$SUMS"
    echo "Removed $(basename "$APPIMAGE"), its records and $(basename "$SUMS") from $OUTPUT_DIR."
  fi
fi

exit 0
