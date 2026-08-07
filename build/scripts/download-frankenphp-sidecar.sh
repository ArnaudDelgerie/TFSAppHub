#!/usr/bin/env bash
set -euo pipefail

# Fetch the FrankenPHP sidecar binary once into the hub's resources. The hub
# resolves it from there in dev; a system-wide /usr/bin/frankenphp is used as a
# fallback. Idempotent: skips the download if an executable binary is already
# present.
#
# This is the interpreter the hub installs *and* runs every app with — no host
# PHP is ever involved — so `make sidecar` is a prerequisite of any real use,
# not just of packaging.
#
# Pin a known-good version rather than tracking `latest`, so a clone at a given
# tag always fetches the same sidecar. Override per-invocation if needed —
# FRANKENPHP_SHA256 is required alongside FRANKENPHP_VERSION, since the pins
# below only cover the default version's assets:
#   FRANKENPHP_VERSION=1.12.5 FRANKENPHP_SHA256=<sha256 for your platform's asset> make sidecar
FRANKENPHP_VERSION="${FRANKENPHP_VERSION:-1.12.4}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RES_DIR="$ROOT_DIR/hub/resources"
DEST="$RES_DIR/frankenphp"

if [[ -x "$DEST" ]]; then
  echo "FrankenPHP sidecar already present: $DEST"
  exit 0
fi

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  ASSET="frankenphp-linux-x86_64" ;;
  Darwin-arm64)  ASSET="frankenphp-mac-arm64" ;;
  Darwin-x86_64) ASSET="frankenphp-mac-x86_64" ;;
  *)
    echo "Unsupported host. Download FrankenPHP manually into $DEST." >&2
    exit 1
    ;;
esac

# SHA-256 pins for each supported asset of the version above, taken from
# GitHub's release-asset digest (GET /repos/php/frankenphp/releases/tags/v1.12.4),
# which GitHub computes server-side at upload time — not asserted by the
# vendor. Cross-checked by hand at implementation time against a local
# `sha256sum` of the downloaded linux-x86_64 asset (matched exactly).
#
# These differ from the station's pins for the same version: upstream replaced
# the v1.12.4 assets in place at some point between 2025-07-15 (when the
# station fetched linux-x86_64 as 4868ea32…) and 2026-08-07 (when this repo
# fetched b39c7511… under the same tag and the same URL). Nothing was tampered
# with locally — the tag simply is not immutable, which is the whole reason the
# checksum gate exists. Consequence worth knowing: the station's copy of this
# script now refuses to download on a fresh clone, and re-pinning it is a
# separate change over there.
case "$ASSET" in
  frankenphp-linux-x86_64) PINNED_SHA256="b39c7511483c99faf0d857cc0789b39cc23023038260b31d4b40dd4ae18795dc" ;;
  frankenphp-mac-arm64)    PINNED_SHA256="fb38e69514a04875b83900da0e1585d611fe0f52f3a904c50bef5605347e5dec" ;;
  frankenphp-mac-x86_64)   PINNED_SHA256="0e97b3e2bb8e98c0f8c6275921e1d44fb6459abfaa3bc0525e56c07ae13aa55b" ;;
esac

if [[ -n "${FRANKENPHP_SHA256:-}" ]]; then
  EXPECTED_SHA256="$FRANKENPHP_SHA256"
elif [[ "$FRANKENPHP_VERSION" != "1.12.4" ]]; then
  echo "FRANKENPHP_VERSION overridden to '$FRANKENPHP_VERSION' without FRANKENPHP_SHA256: the pinned checksums above only cover v1.12.4's assets. Set FRANKENPHP_SHA256 to the expected SHA-256 for $ASSET at that version." >&2
  exit 1
else
  EXPECTED_SHA256="$PINNED_SHA256"
fi

# sha256sum ships on Linux; macOS ships shasum instead — support both since
# this script also targets Darwin (see the case above).
compute_sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    echo "Neither sha256sum nor shasum found on PATH — cannot verify download integrity" >&2
    exit 1
  fi
}

mkdir -p "$RES_DIR"
URL="https://github.com/php/frankenphp/releases/download/v${FRANKENPHP_VERSION}/${ASSET}"
echo "Downloading FrankenPHP ${FRANKENPHP_VERSION} ($ASSET)..."
curl --fail --location "$URL" -o "$DEST"

ACTUAL_SHA256="$(compute_sha256 "$DEST")"
if [[ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]]; then
  rm -f "$DEST"
  echo "SHA-256 mismatch for $ASSET: expected $EXPECTED_SHA256, got $ACTUAL_SHA256 — refusing to install a tampered or corrupted download" >&2
  exit 1
fi

chmod +x "$DEST"
echo "Downloaded and verified $DEST"
