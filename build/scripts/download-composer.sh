#!/usr/bin/env bash
set -euo pipefail

# Fetch composer.phar once into the hub's resources, beside the FrankenPHP
# sidecar. Idempotent: skips the download if the phar is already present.
#
# The hub installs every app's dependencies itself, with its own interpreter —
# `frankenphp php-cli composer.phar install` — so that the tree is resolved by
# the very PHP that will later serve it and no host PHP is ever involved. That
# makes this as much a prerequisite of `install` as `make sidecar` is: without
# it, nothing installs.
#
# Pin a known-good version rather than tracking the "latest" redirect, so a
# clone at a given tag always fetches the same Composer. Override
# per-invocation if needed — COMPOSER_SHA256 is required alongside
# COMPOSER_VERSION, since the pin below only covers the default version:
#   COMPOSER_VERSION=2.9.0 COMPOSER_SHA256=<sha256> make composer
COMPOSER_VERSION="${COMPOSER_VERSION:-2.8.12}"

# SHA-256 for the version above, taken from the `.sha256sum` getcomposer.org
# publishes beside the phar and cross-checked by hand at pin time against a
# local `sha256sum` of the download (matched exactly, 2026-08-07).
#
# Worth being exact about what this buys, since both values come from the same
# host: it catches a corrupted or truncated download, and it catches the asset
# being replaced under an unchanged version number later — which is not
# hypothetical, the FrankenPHP sidecar's own pins had to be rewritten for
# exactly that. It is not a claim about the origin at the moment it was pinned.
PINNED_SHA256="f446ea719708bb85fcbf4ef18def5d0515f1f9b4d703f6d820c9c1656e10a2f2"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RES_DIR="$ROOT_DIR/hub/resources"
DEST="$RES_DIR/composer.phar"

if [[ -f "$DEST" ]]; then
  echo "Composer already present: $DEST"
  exit 0
fi

if [[ -n "${COMPOSER_SHA256:-}" ]]; then
  EXPECTED_SHA256="$COMPOSER_SHA256"
elif [[ "$COMPOSER_VERSION" != "2.8.12" ]]; then
  echo "COMPOSER_VERSION overridden to '$COMPOSER_VERSION' without COMPOSER_SHA256: the pin above only covers v2.8.12. Set COMPOSER_SHA256 to the expected SHA-256 for that version's composer.phar." >&2
  exit 1
else
  EXPECTED_SHA256="$PINNED_SHA256"
fi

# sha256sum ships on Linux; macOS ships shasum instead — same two-backend
# helper as the sidecar script, for the same reason.
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
URL="https://getcomposer.org/download/${COMPOSER_VERSION}/composer.phar"
echo "Downloading Composer ${COMPOSER_VERSION}..."
curl --fail --location "$URL" -o "$DEST"

ACTUAL_SHA256="$(compute_sha256 "$DEST")"
if [[ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]]; then
  rm -f "$DEST"
  echo "SHA-256 mismatch for composer.phar ${COMPOSER_VERSION}: expected $EXPECTED_SHA256, got $ACTUAL_SHA256 — refusing to install a tampered or corrupted download" >&2
  exit 1
fi

echo "Downloaded and verified $DEST"
