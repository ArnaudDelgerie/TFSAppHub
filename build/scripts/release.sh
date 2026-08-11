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
#   1. Version guard: tag = v<hub version>; stop if that release exists.
#   2. Changelog gate: CHANGELOG.md must exist at the repo root with a
#      "## <version>" heading ("## v<version>" and "## [<version>]" count too,
#      with an optional trailing date); refuse otherwise. That section becomes
#      the release notes, nothing more.
#   3. Build (`make build`), or reuse a matching AppImage already under
#      target/release/bundle/appimage/ — prompted once, default rebuild.
#   4. Checksums: SHA256SUMS.txt (integrity for the unsigned flow).
#   5. Publish: gh release create with the .AppImage and the checksums as
#      assets (GitHub object storage, never committed — the repo clone stays
#      light). A fresh build (not a reused one) is then offered for cleanup —
#      keep on disk by default, or discard both files.
#
# Unlike the station, and unlike this script's own per-*app* release.sh
# ancestor: the version comes from hub/Cargo.toml, the hub's own version —
# there is no tfsapp.config.json here, the hub is not an app.
#
# The releases repo is a constant, not resolved or prompted for: the hub is
# one project with one release stream, not N apps each choosing their own.
# Override it from the environment for a fork or a private mirror. The value
# itself lives in build/releases-repo, one shared source of truth read here
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

if [[ -z "${RELEASES_REPO:-}" ]]; then
  RELEASES_REPO_FILE="$ROOT_DIR/build/releases-repo"
  [[ -f "$RELEASES_REPO_FILE" ]] \
    || die "no build/releases-repo file at $RELEASES_REPO_FILE"
  RELEASES_REPO="$(grep -vE '^[[:space:]]*(#|$)' "$RELEASES_REPO_FILE" | head -n1)"
  [[ -n "$RELEASES_REPO" ]] \
    || die "build/releases-repo ($RELEASES_REPO_FILE) has no value line"
fi

# --- Preconditions ----------------------------------------------------------
command -v gh >/dev/null 2>&1 \
  || die "gh (GitHub CLI) is not installed — see https://cli.github.com"
gh auth status >/dev/null 2>&1 \
  || die "gh is not authenticated. Run: gh auth login"
[[ -f "$CARGO_TOML" ]] || die "hub/Cargo.toml not found at $CARGO_TOML"

APP_VERSION="$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "$CARGO_TOML")"
[[ -n "$APP_VERSION" ]] \
  || die "could not read a \"version\" line from $CARGO_TOML"

REPO="$RELEASES_REPO"
if gh repo view "$REPO" >/dev/null 2>&1; then
  echo "Releases repo: $REPO (exists)"
else
  echo "Creating public releases repo $REPO ..."
  gh repo create "$REPO" --public --add-readme \
    --description "Downloads and release notes for TFSAppHub." \
    || die "failed to create $REPO"
fi

# --- Step 1: version guard --------------------------------------------------
TAG="v$APP_VERSION"

if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  die "release $TAG already exists on $REPO — bump the version in hub/Cargo.toml first."
fi
echo "Releasing $TAG"

# --- Step 2: changelog gate --------------------------------------------------
# Hard prerequisite: CHANGELOG.md must exist at the repo root and carry a
# level-2 heading for $APP_VERSION. The version has to come from a known,
# predictable place — but not in one single spelling: "## 1.2.0", "## v1.2.0"
# and "## [1.2.0]" (Keep a Changelog) all count, with anything after the
# version on the same line (a date, a link) ignored. What is *not* accepted
# is a heading that merely contains the version, so "## 1.2.0.1" never
# answers for 1.2.0.
#
# The matched section becomes the release notes verbatim — never the whole
# file, never a generic fallback — extracted to a temp file cleaned up on
# exit. The extractor below matches the heading with this same regex: the two
# must never drift apart, or the gate would pass on a heading the extractor
# then fails to find, publishing empty notes.
CHANGELOG_FILE="$ROOT_DIR/CHANGELOG.md"
[[ -f "$CHANGELOG_FILE" ]] \
  || die "no CHANGELOG.md found at $CHANGELOG_FILE."

# Bracket expressions rather than backslash escapes throughout: the regex is
# handed to awk through -v, which would eat the backslashes ("\." is not an
# awk escape sequence). "." and "+" are the only ERE metacharacters a semver
# string can contain.
VERSION_RE="${APP_VERSION//+/[+]}"
VERSION_RE="${VERSION_RE//./[.]}"
HEADING_RE="^## [[]?v?${VERSION_RE}[]]?([[:space:]].*)?$"

grep -qE "$HEADING_RE" "$CHANGELOG_FILE" \
  || die "CHANGELOG.md has no '## $APP_VERSION' heading for version $APP_VERSION — '## v$APP_VERSION' and '## [$APP_VERSION]' are accepted too, optionally followed by a date."

NOTES_FILE="$(mktemp)"
trap 'rm -f "$NOTES_FILE"' EXIT
awk -v heading_re="$HEADING_RE" '
  $0 ~ heading_re { found=1; print; next }
  found && /^## / { exit }
  found { print }
' "$CHANGELOG_FILE" >"$NOTES_FILE"

# --- Step 3: build (or reuse) ------------------------------------------------
OUTPUT_DIR="$ROOT_DIR/target/release/bundle/appimage"
shopt -s nullglob
existing=("$OUTPUT_DIR/TFSAppHub_${APP_VERSION}_"*.AppImage)
FRESH_BUILD=1

if [[ ${#existing[@]} -gt 1 ]]; then
  die "found ${#existing[@]} AppImages matching TFSAppHub_${APP_VERSION}_*.AppImage in $OUTPUT_DIR — ambiguous, resolve by hand first."
elif [[ ${#existing[@]} -eq 1 ]]; then
  read -r -p "Found an existing build for $TAG — reuse it instead of rebuilding? [y/N] " REUSE
  if [[ "$REUSE" =~ ^[Yy]$ ]]; then
    APPIMAGE="${existing[0]}"
    FRESH_BUILD=0
    echo "Reusing $APPIMAGE"
  fi
fi

if [[ -z "${APPIMAGE:-}" ]]; then
  "$ROOT_DIR/build/scripts/build-hub.sh"
  appimage=("$OUTPUT_DIR/TFSAppHub_${APP_VERSION}_"*.AppImage)
  [[ ${#appimage[@]} -eq 1 ]] || die \
    "expected exactly one AppImage matching TFSAppHub_${APP_VERSION}_*.AppImage in $OUTPUT_DIR, found ${#appimage[@]}"
  APPIMAGE="${appimage[0]}"
fi

# --- Step 4: checksums ------------------------------------------------------
# Generated from inside target/release/bundle/appimage/ so the file records a
# bare filename and not this machine's absolute path.
SUMS="$OUTPUT_DIR/SHA256SUMS.txt"
( cd "$OUTPUT_DIR" && sha256sum "$(basename "$APPIMAGE")" ) >"$SUMS"
echo "Checksums:"
cat "$SUMS"

# --- Step 5: publish --------------------------------------------------------
# Release notes are exactly the section extracted by the changelog gate
# (step 2) — --generate-notes can't work cross-repo, the releases repo has
# none of this repo's commits.
gh release create "$TAG" \
  --repo "$REPO" \
  --title "$TAG" \
  --notes-file "$NOTES_FILE" \
  "$APPIMAGE" "$SUMS"

echo "Published $TAG to $REPO."

# A reused artifact was already on disk before this run and is left alone —
# only a build this run actually produced is offered for cleanup.
if [[ "$FRESH_BUILD" -eq 1 ]]; then
  read -r -p "Keep the build (AppImage + SHA256SUMS.txt) on disk? [Y/n] " KEEP
  if [[ "$KEEP" =~ ^[Nn]$ ]]; then
    rm -f "$APPIMAGE" "$SUMS"
    echo "Removed $APPIMAGE and $SUMS."
  fi
fi

exit 0
