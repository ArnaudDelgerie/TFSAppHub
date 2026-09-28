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

"$ROOT_DIR/build/scripts/fix-appimage-bundle.sh" "$APPIMAGE"

SIZE="$(du -h "$APPIMAGE" | cut -f1)"
echo "Built $APPIMAGE ($SIZE)"
