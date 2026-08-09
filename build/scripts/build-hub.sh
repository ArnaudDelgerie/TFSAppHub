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

(cd "$HUB_DIR" && cargo tauri build --bundles appimage)

# The workspace's shared target/, not hub/target/ — hub/ is a member of the
# root Cargo.toml's [workspace], and cargo puts every member's output there.
BUNDLE_DIR="$ROOT_DIR/target/release/bundle/appimage"
shopt -s nullglob
appimages=("$BUNDLE_DIR"/*.AppImage)
if [[ ${#appimages[@]} -ne 1 ]]; then
  echo "build-hub: expected exactly one *.AppImage under $BUNDLE_DIR, found ${#appimages[@]}." >&2
  exit 1
fi
APPIMAGE="${appimages[0]}"

SIZE="$(du -h "$APPIMAGE" | cut -f1)"
echo "Built $APPIMAGE ($SIZE)"
