#!/usr/bin/env bash
set -euo pipefail

# `make build`: produce the hub's own AppImage. Refuses up front rather than
# handing `cargo tauri build` an empty stub — `hub/build.rs` writes a 0-byte
# placeholder at each of these paths so a fresh clone still compiles, and a
# packaged hub built from either would ship a sidecar or a Composer that can
# never run (see `hub/build.rs` and `core::sidecar::is_present`). "present"
# alone is not enough to tell the two apart, so this checks size rather than
# mere existence.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HUB_DIR="$ROOT_DIR/hub"
RES_DIR="$HUB_DIR/resources"

for name in frankenphp composer.phar; do
  path="$RES_DIR/$name"
  if [[ ! -s "$path" ]]; then
    echo "build-hub: $path is missing or empty — run \`make resources\` first." >&2
    exit 1
  fi
done

# The version being built, read from hub/Cargo.toml exactly the way
# release.sh reads it (duplicated on purpose — one line each, not a sourced
# library), so `make build` and `make release` can never disagree about which
# AppImage belongs to this tree.
CARGO_TOML="$HUB_DIR/Cargo.toml"
VERSION="$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "$CARGO_TOML")"
if [[ -z "$VERSION" ]]; then
  echo "build-hub: could not read a \"version\" line from $CARGO_TOML" >&2
  exit 1
fi

# The revision this build is of, captured before cargo runs, so the record
# written beside the AppImage names the tree that produced it. A dirty tree
# is still built — a local build stays free — but marked as such (untracked
# files count, ignored ones don't).
REVISION="$(git -C "$ROOT_DIR" rev-parse HEAD)"
if [[ -n "$(git -C "$ROOT_DIR" status --porcelain)" ]]; then
  REVISION="$REVISION-dirty"
fi

(cd "$HUB_DIR" && cargo tauri build --bundles appimage)

# The workspace's shared target/, not hub/target/ — hub/ is a member of the
# root Cargo.toml's [workspace], and cargo puts every member's output there.
BUNDLE_DIR="$ROOT_DIR/target/release/bundle/appimage"
shopt -s nullglob
# Scoped to this version, not every *.AppImage in the directory: an older
# build kept on disk (release.sh's reuse prompt defaults to keeping it) must
# not make an unambiguous build ambiguous.
appimages=("$BUNDLE_DIR"/TFSAppHub_"${VERSION}"_*.AppImage)
if [[ ${#appimages[@]} -ne 1 ]]; then
  echo "build-hub: expected exactly one TFSAppHub_${VERSION}_*.AppImage under $BUNDLE_DIR, found ${#appimages[@]}." >&2
  exit 1
fi
APPIMAGE="${appimages[0]}"

# The file was just rebuilt, so any record left beside it from an earlier
# build of this version is stale — remove it before the fix pass, and write
# the fresh one only after that pass succeeds.
rm -f "${APPIMAGE%.AppImage}.source-commit"

"$ROOT_DIR/build/scripts/fix-appimage-bundle.sh" "$APPIMAGE"

# Where this AppImage came from — release.sh reads it before offering the
# build for reuse, and re-checks it just before publishing.
printf '%s\n' "$REVISION" >"${APPIMAGE%.AppImage}.source-commit"

SIZE="$(du -h "$APPIMAGE" | cut -f1)"
echo "Built $APPIMAGE ($SIZE)"
