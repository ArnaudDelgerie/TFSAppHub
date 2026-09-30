#!/usr/bin/env bash
set -euo pipefail

# Repair what linuxdeploy got wrong inside the hub's own AppImage (called from
# build-hub.sh right after `cargo tauri build` produces it), and drop one
# bundled library linuxdeploy got *right* but that the hub does not want
# frozen. Ported from TFSAppWorkstation's script of the same name, reduced to
# one image — the hub's — instead of one per app, since plan 012 packages the
# hub itself rather than a per-app build.
#
# 1. The FrankenPHP sidecar. tauri-bundler shells out to linuxdeploy to
#    assemble the AppImage, and linuxdeploy runs patchelf (rpath -> $ORIGIN) on
#    every ELF file it finds in the AppDir — including our
#    hub/resources/frankenphp. patchelf corrupts static-PIE Go binaries
#    (rewritten program headers -> SIGSEGV on every exec; known upstream
#    limitation, NixOS/patchelf#152), and neither tauri-bundler nor linuxdeploy
#    exposes a way to exclude a file from that pass. So we repair after the
#    fact: put the pristine binary back.
#
# 2. The display backend. linuxdeploy-plugin-gtk writes
#    apprun-hooks/linuxdeploy-plugin-gtk.sh into the AppDir, and that generated
#    hook is sourced by AppRun. Older versions hard-force `export GDK_BACKEND=x11`,
#    pinning Wayland sessions to XWayland and causing touchpad scroll overshoot.
#    We rewrite that line to defer to the session backend (HOOK_PATCH below).
#    Newer versions comment that export out; accept that native behavior too.
#
# 3. `.DirIcon`. linuxdeploy writes it as an *absolute* symlink into the
#    build machine's own AppDir directory, which does not exist once the
#    image is mounted at its random FUSE path on a user's machine — file
#    managers and AppImage integrators fall back to a generic icon. We
#    replace it with a relative symlink to the icon named by the AppImage's
#    own `.desktop` file (`Icon=`) — not just "the one top-level *.png", since
#    the hub's own AppDir root actually carries two: `TFSAppHub.png` (a plain
#    copy tauri-bundler drops there) and `tfsapp-hub.png` (linuxdeploy's own
#    relative symlink into `usr/share/icons/...`, already correctly named
#    after `Icon=`). Picking by `Icon=` is also just the correct rule on its
#    own terms — a `.DirIcon` that pointed at the wrong asset would be no less
#    broken for being relative.
#
# And one deliberate removal, not a repair of anything broken:
#
# 4. `libwayland-*`. linuxdeploy bundles the build host's copy alongside the
#    WebKit/GTK stack it exists to freeze — but the Wayland *client* library is
#    an ABI the running **session** owns, not the toolkit. A copy frozen older
#    than the host's own Mesa/compositor is a known way to make the image fail
#    to start on a newer distribution — the failure mode TFSAppWorkstation's
#    plan 060 could only flag, never fix. We delete every `libwayland-*` from
#    the AppDir instead, so the dynamic linker falls through to the session's
#    own copy — the one built for it — the same way it would for any other
#    library this AppImage does not carry.
#
# 5. The media framework. Tauri's GStreamer plugin freezes the build host's
#    plugins and helpers alongside WebKit and libgstreamer, so the plugin ABI
#    matches the core library in the image. It also pulls in PulseAudio and
#    PipeWire client libraries. Those speak to the running session's audio
#    server, and PipeWire loads SPA modules from the host's own paths, so leave
#    these client libraries to the session instead. Capture-critical plugins
#    are verified in the final artifact below.
#
# 6. Graphics dispatch and driver libraries belong to the host GPU stack
#    (GLVND, Mesa, NVIDIA). Bundling one generation beside the host's libEGL
#    mixes two stacks; AppImage's excludelist leaves them to the host for the
#    same reason. The host's libwebkit2gtk-4.1-0 depends on libgles2, so a
#    desktop that can run WebKit already has the GLES dispatch library.
#
# Usage: build/scripts/fix-appimage-bundle.sh [appimage-path...]
# Defaults to every *.AppImage under the hub's own bundle dir when no path is
# given; a no-op when no AppImage is found there, or when the sidecar, the
# hook, .DirIcon and the absence of session-owned libraries are already correct.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PRISTINE="$ROOT_DIR/hub/resources/frankenphp"
BUNDLE_DIR="$ROOT_DIR/target/release/bundle/appimage"

HOOK_REL="apprun-hooks/linuxdeploy-plugin-gtk.sh"
# The line linuxdeploy-plugin-gtk generates, and the line we replace it with.
# A plugin release that renames or reformats its own line must break the build
# loudly rather than silently ship the XWayland scroll jump again, so matching
# neither pattern is a fatal error below.
HOOK_ORIGINAL_RE='^export GDK_BACKEND=x11([[:space:]]|$)'
# shellcheck disable=SC2016  # literal text written into the hook, not expanded here
HOOK_PATCHED_LINE='export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"'
HOOK_PATCH="$(cat <<EOF
# Patched by TFSAppHub (build/scripts/fix-appimage-bundle.sh).
# linuxdeploy-plugin-gtk hard-codes \`export GDK_BACKEND=x11\` here, which puts
# the webview on XWayland; XWayland's synthetic kinetic scrolling turns a
# touchpad scroll into a 100-200px overshoot at finger-lift. Defer to the
# session's own backend instead — an X11 session still resolves to x11. The
# \`:-\` default keeps GDK_BACKEND=x11 available from the environment, without a
# rebuild, for anyone hitting tauri-apps/tauri#8541 (the crash the plugin's
# original comment cited).
$HOOK_PATCHED_LINE
EOF
)"

shopt -s nullglob
if [[ $# -gt 0 ]]; then
  appimages=("$@")
else
  appimages=("$BUNDLE_DIR"/*.AppImage)
fi
if [[ ${#appimages[@]} -eq 0 ]]; then
  echo "No AppImage under $BUNDLE_DIR — nothing to fix."
  exit 0
fi

test -x "$PRISTINE" || { echo "Pristine sidecar missing: $PRISTINE" >&2; exit 1; }

# Same tool linuxdeploy used to pack the AppImage, cached by tauri-bundler
# right before it produced the file we are fixing.
APPIMAGETOOL="${XDG_CACHE_HOME:-$HOME/.cache}/tauri/linuxdeploy-plugin-appimage.AppImage"
if [[ ! -x "$APPIMAGETOOL" ]]; then
  echo "appimagetool not found at $APPIMAGETOOL (expected from the tauri build)." >&2
  exit 1
fi

# Echoes "patched" for our line, "stale" for the active upstream X11 line,
# or "native" when upstream itself comments out the X11 line. Anything else
# is fatal (see HOOK_ORIGINAL_RE).
hook_state() {
  local hook="$1"
  if [[ ! -f "$hook" ]]; then
    echo "Expected GTK hook missing from the AppImage: $HOOK_REL" >&2
    return 1
  fi
  if grep -Fxq "$HOOK_PATCHED_LINE" "$hook"; then
    echo "patched"
  elif grep -Eq "$HOOK_ORIGINAL_RE" "$hook"; then
    echo "stale"
  elif grep -Eq '^[[:space:]]*#[[:space:]]*export GDK_BACKEND=x11([[:space:]]|$)' "$hook" \
       && ! grep -Eq '^[[:space:]]*export[[:space:]]+GDK_BACKEND=' "$hook"; then
    echo "native"
  else
    echo "No recognised GDK_BACKEND line in $HOOK_REL — linuxdeploy-plugin-gtk" >&2
    echo "must have changed it. Expected a line matching $HOOK_ORIGINAL_RE," >&2
    echo "or the already-patched $HOOK_PATCHED_LINE. Refusing to ship an" >&2
    echo "unpatched hook: the app would silently run on XWayland again." >&2
    return 1
  fi
}

# True (exit 0) when the symlink at $1 exists, is not dangling, and resolves
# to a location inside the AppDir rooted at $2. False for an absolute symlink
# even when it happens to also resolve on this machine — that's exactly the
# .DirIcon defect: correct on the build host, broken everywhere else.
symlink_resolves_inside() {
  local link="$1" root="$2" resolved
  resolved="$(realpath -e "$link" 2>/dev/null)" || return 1
  [[ "$resolved" == "$root"/* ]]
}

# WebKit/GTK/GLib/GStreamer libraries the bundle freezes. libwayland-client
# is deliberately not here — it is deleted below, never frozen (defect 4).
FROZEN_SONAMES=(
  libwebkit2gtk-4.1.so.0
  libjavascriptcoregtk-4.1.so.0
  libgtk-3.so.0
  libglib-2.0.so.0
  libgstreamer-1.0.so.0
)

# True (exit 0) when no `libwayland-*` file survives anywhere under AppDir
# root $1.
no_bundled_wayland() {
  [[ -z "$(find "$1" \( -type f -o -type l \) -iname 'libwayland-*' -print -quit)" ]]
}

# Audio server clients belong to the running session, not the frozen media
# framework. PipeWire in particular loads SPA modules from the host's paths.
no_bundled_audio_clients() {
  [[ -z "$(find "$1" \( -type f -o -type l \) \( -name 'libpipewire-*' -o -name 'libpulse*' \) -print -quit)" ]]
}

# Graphics dispatch and driver libraries must come from the host GPU stack.
graphics_driver_files() {
  local root="$1"
  shift
  find "$root" \( -type f -o -type l \) \( \
    -name 'libGL.so*' -o -name 'libEGL*' -o -name 'libGLESv2*' -o \
    -name 'libGLX*' -o -name 'libOpenGL*' -o -name 'libGLdispatch*' -o \
    -name 'libgbm*' -o -name 'libdrm*' \) "$@"
}

no_bundled_graphics_driver() {
  [[ -z "$(graphics_driver_files "$1" -print -quit)" ]]
}

# Refuse an image whose copied GStreamer set cannot supply the elements WebKit
# needs for microphone capture. Name the build-host package to install.
verify_capture_plugins() {
  local plugins="$1/usr/lib/gstreamer-1.0" name package
  for name in libgstapp.so libgstcoreelements.so; do
    case "$name" in
      libgstapp.so) package=gstreamer1.0-plugins-base ;;
      libgstcoreelements.so) package=libgstreamer1.0-0 ;;
    esac
    if [[ ! -f "$plugins/$name" ]]; then
      echo "Verification failed: missing GStreamer plugin $name in $plugins; install build-host package $package." >&2
      return 1
    fi
  done
  if [[ ! -f "$plugins/libgstpulseaudio.so" && ! -f "$plugins/libgstpipewire.so" ]]; then
    echo "Verification failed: missing GStreamer audio source plugin libgstpulseaudio.so (build-host package gstreamer1.0-pulseaudio) or libgstpipewire.so (build-host package gstreamer1.0-pipewire) in $plugins." >&2
    return 1
  fi
}

# The highest GLIBC_x.y symbol version any ELF file under AppDir root $1
# imports — the actual floor a machine needs to run every binary and every
# bundled shared library this AppImage carries, not just the toolkit
# libraries FROZEN_SONAMES names below. `readelf -V` lists both required
# (verneed) and defined (verdef) symbol versions; scanning both is safe here
# because nothing bundled *defines* a GLIBC_* version — glibc itself is never
# bundled, it is the one ABI every one of these binaries assumes the host
# provides. GNU `sort -V` orders the dotted x.y version strings correctly.
highest_glibc_requirement() {
  local root="$1" file
  while IFS= read -r -d '' file; do
    file -b "$file" 2>/dev/null | grep -q ELF || continue
    readelf -V "$file" 2>/dev/null | grep -oE 'GLIBC_[0-9]+\.[0-9]+'
  done < <(find "$root" -type f -print0) | sed 's/^GLIBC_//' | sort -Vu | tail -1
}

# Captured once, up front: piping a live `ldconfig -p` straight into an awk
# that `exit`s on its first match closes the pipe while ldconfig is still
# writing, which SIGPIPEs it — fatal under this script's `pipefail`. Reusing
# a variable via a here-string sidesteps that (no live producer to SIGPIPE)
# and is cheaper than re-running ldconfig per soname besides.
LDCONFIG_CACHE="$(ldconfig -p)"

# Prints "<package>=<version>" for the Debian package owning the build host's
# copy of soname $1 — the host is what linuxdeploy actually copied into $2
# (the extracted AppDir), so its installed package version is the frozen
# one. Fails loudly (rather than recording "unknown") at every step: an
# unrecorded frozen version defeats the point of this record.
resolve_library_version() {
  local soname="$1" appdir_root="$2"
  local bundled_matches bundled host_path dpkg_matches pkg version

  bundled_matches="$(find "$appdir_root" -type f -name "$soname" 2>/dev/null)"
  bundled="${bundled_matches%%$'\n'*}"
  if [[ -z "$bundled" ]]; then
    echo "Bundle does not carry $soname — expected it in $appimage." >&2
    exit 1
  fi

  host_path="$(awk -v s="$soname" '$1 == s { print $NF; exit }' <<<"$LDCONFIG_CACHE")"
  if [[ -z "$host_path" ]]; then
    echo "Build host's ldconfig cache has no entry for $soname — cannot record its frozen version." >&2
    exit 1
  fi
  # ldconfig reports the /lib/... alias; dpkg's file database only knows the
  # canonical /usr/lib/... path (and the versioned file the .so.0 symlink
  # resolves to), so dpkg -S on the alias finds nothing.
  host_path="$(realpath -e "$host_path" 2>/dev/null)" || host_path=""
  if [[ -z "$host_path" ]]; then
    echo "Build host's $soname (via ldconfig) doesn't resolve to a real file — cannot record its frozen version." >&2
    exit 1
  fi

  dpkg_matches="$(dpkg -S "$host_path" 2>/dev/null)"
  pkg="${dpkg_matches%%$'\n'*}"
  pkg="${pkg%%:*}"
  if [[ -z "$pkg" ]]; then
    echo "No dpkg owner found for $host_path ($soname) — cannot record its frozen version." >&2
    exit 1
  fi

  version="$(dpkg-query -W -f='${Version}' "$pkg" 2>/dev/null)"
  if [[ -z "$version" ]]; then
    echo "dpkg-query found no installed version for package $pkg ($soname)." >&2
    exit 1
  fi
  echo "$pkg=$version"
}

for appimage in "${appimages[@]}"; do
  appimage="$(realpath "$appimage")"
  workdir="$(mktemp -d "$(dirname "$appimage")/.appimage-fix-XXXXXX")"
  trap 'rm -rf "$workdir"' EXIT

  (cd "$workdir" && "$appimage" --appimage-extract > /dev/null)

  bundled=("$workdir"/squashfs-root/usr/lib/*/resources/frankenphp)
  if [[ ${#bundled[@]} -ne 1 ]]; then
    echo "Expected exactly one bundled frankenphp in $appimage, found ${#bundled[@]}." >&2
    exit 1
  fi

  hook="$workdir/squashfs-root/$HOOK_REL"
  hook_state="$(hook_state "$hook")"

  dir_icon="$workdir/squashfs-root/.DirIcon"
  dir_icon_ok=true
  symlink_resolves_inside "$dir_icon" "$workdir/squashfs-root" || dir_icon_ok=false

  wayland_ok=true
  no_bundled_wayland "$workdir/squashfs-root" || wayland_ok=false
  audio_clients_ok=true
  no_bundled_audio_clients "$workdir/squashfs-root" || audio_clients_ok=false
  graphics_driver_ok=true
  no_bundled_graphics_driver "$workdir/squashfs-root" || graphics_driver_ok=false

  # Repack only when something actually needs fixing — but "already fixed" has
  # to mean every repair, otherwise an already-pristine sidecar would
  # short-circuit the pass and quietly drop one of the others.
  if cmp -s "${bundled[0]}" "$PRISTINE" && [[ "$hook_state" != "stale" ]] && [[ "$dir_icon_ok" == true ]] && [[ "$wayland_ok" == true ]] && [[ "$audio_clients_ok" == true ]] && [[ "$graphics_driver_ok" == true ]]; then
    echo "$(basename "$appimage"): sidecar, GTK hook, .DirIcon and session-owned libraries already correct, skipping."
  else
    cp "$PRISTINE" "${bundled[0]}"
    chmod 755 "${bundled[0]}"
    if [[ "$hook_state" == "stale" ]]; then
      # Spelled as two anchored alternatives rather than HOOK_ORIGINAL_RE's
      # `(…|$)` group: `$` inside a group is not portably an anchor in awk EREs.
      awk -v patch="$HOOK_PATCH" \
        '$0 ~ /^export GDK_BACKEND=x11$/ || $0 ~ /^export GDK_BACKEND=x11[[:space:]]/ \
           { print patch; next } { print }' \
        "$hook" >"$hook.tmp"
      mv "$hook.tmp" "$hook"
      chmod 755 "$hook"
    fi
    if [[ "$dir_icon_ok" == false ]]; then
      # Pick the icon by the AppImage's own .desktop Icon= line, not by
      # "the one top-level *.png" — the AppDir root can carry more than one
      # (see the header comment).
      desktop_files=("$workdir/squashfs-root"/*.desktop)
      if [[ ${#desktop_files[@]} -ne 1 ]]; then
        echo "Expected exactly one top-level *.desktop in $appimage, found ${#desktop_files[@]}." >&2
        exit 1
      fi
      icon_name="$(awk -F= '$1 == "Icon" { print $2; exit }' "${desktop_files[0]}")"
      if [[ -z "$icon_name" ]]; then
        echo "No Icon= line in $(basename "${desktop_files[0]}") — cannot repair .DirIcon." >&2
        exit 1
      fi
      icon_file="$workdir/squashfs-root/$icon_name.png"
      if [[ ! -e "$icon_file" ]]; then
        echo "Expected $icon_name.png (from Icon=$icon_name) at the AppDir root, found none." >&2
        exit 1
      fi
      ln -sf "$(basename "$icon_file")" "$dir_icon"
    fi
    if [[ "$wayland_ok" == false ]]; then
      while IFS= read -r -d '' lib; do
        rm -f "$lib"
      done < <(find "$workdir/squashfs-root" \( -type f -o -type l \) -iname 'libwayland-*' -print0)
    fi
    if [[ "$audio_clients_ok" == false ]]; then
      while IFS= read -r -d '' lib; do
        rm -f "$lib"
      done < <(find "$workdir/squashfs-root" \( -type f -o -type l \) \( -name 'libpipewire-*' -o -name 'libpulse*' \) -print0)
    fi
    if [[ "$graphics_driver_ok" == false ]]; then
      while IFS= read -r -d '' lib; do
        rm -f "$lib"
      done < <(graphics_driver_files "$workdir/squashfs-root" -print0)
    fi
    OUTPUT="$workdir/fixed.AppImage" ARCH="$(uname -m)" APPIMAGE_EXTRACT_AND_RUN=1 \
      "$APPIMAGETOOL" --appdir "$workdir/squashfs-root" > "$workdir/repack.log" 2>&1 \
      || { echo "Repack failed for $appimage:" >&2; cat "$workdir/repack.log" >&2; exit 1; }
    mv "$workdir/fixed.AppImage" "$appimage"
  fi

  # Verify: pull the whole (re)packed AppImage back out and check it fresh —
  # trust nothing about the input to the repack, only what actually shipped.
  # A full extraction (rather than the three selective ones this replaced) is
  # what the symlink walk below needs anyway.
  (cd "$workdir" && rm -rf squashfs-root && "$appimage" --appimage-extract > /dev/null)
  verify_root="$workdir/squashfs-root"
  verify=("$verify_root"/usr/lib/*/resources/frankenphp)
  cmp -s "${verify[0]}" "$PRISTINE" \
    || { echo "Verification failed: sidecar in $appimage still differs from $PRISTINE." >&2; exit 1; }
  verified_hook_state="$(hook_state "$verify_root/$HOOK_REL")"
  [[ "$verified_hook_state" != "stale" ]] \
    || { echo "Verification failed: $HOOK_REL in $appimage still forces GDK_BACKEND." >&2; exit 1; }
  case "$(readlink "$verify_root/.DirIcon")" in
    /*) echo "Verification failed: .DirIcon in $appimage is still an absolute symlink." >&2; exit 1 ;;
  esac
  no_bundled_wayland "$verify_root" \
    || { echo "Verification failed: $appimage still bundles a libwayland-*." >&2; exit 1; }
  no_bundled_audio_clients "$verify_root" \
    || { echo "Verification failed: $appimage still bundles a libpipewire-* or libpulse* client library." >&2; exit 1; }
  no_bundled_graphics_driver "$verify_root" \
    || { echo "Verification failed: $appimage still bundles a graphics-driver library." >&2; exit 1; }
  verify_capture_plugins "$verify_root" || exit 1

  # Belt-and-braces beyond the three known defects above: nothing else
  # linuxdeploy produced should be an absolute or dangling symlink either.
  offenders=()
  while IFS= read -r -d '' link; do
    symlink_resolves_inside "$link" "$verify_root" || offenders+=("${link#"$verify_root"/} -> $(readlink "$link")")
  done < <(find "$verify_root" -type l -print0)
  if [[ ${#offenders[@]} -gt 0 ]]; then
    echo "Verification failed: ${#offenders[@]} symlink(s) in $appimage are absolute or dangling:" >&2
    printf '  %s\n' "${offenders[@]}" >&2
    exit 1
  fi

  # Record which renderer/toolkit versions this release froze — the build
  # host's `apt upgrade` moves on, but nothing in the shipped AppImage does
  # until the next rebuild. Written from verify_root (the final artifact),
  # not the pre-repack tree, so the record matches what actually shipped.
  test -f /etc/os-release || { echo "No /etc/os-release on the build host — cannot record its OS release." >&2; exit 1; }

  # The glibc floor: measured from whatever produced this image, with no
  # knowledge of *how* — a local `make build` and a containerised one write
  # this exact same field, and this script never asks which one it was. It is
  # what a user meets as a loader error before a single line of the hub's own
  # code runs, so it belongs beside the artifact rather than in a document
  # they would have to go and find (see README.md "It does not start").
  glibc_floor="$(highest_glibc_requirement "$verify_root")"
  if [[ -z "$glibc_floor" ]]; then
    echo "No GLIBC_x.y symbol version found in any bundled ELF in $appimage — cannot record its glibc floor." >&2
    exit 1
  fi
  build_host_glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $NF}')"
  if [[ -z "$build_host_glibc" ]]; then
    echo "getconf GNU_LIBC_VERSION gave no version on the build host — cannot record it." >&2
    exit 1
  fi

  version_record="${appimage%.AppImage}.versions.txt"
  {
    echo "# Frozen renderer/toolkit versions bundled into $(basename "$appimage")"
    echo "# Generated by build/scripts/fix-appimage-bundle.sh — do not hand-edit."
    echo "# These are frozen at build time: the end user's own package updates"
    echo "# never reach them. Only a rebuild changes this file."
    # shellcheck disable=SC1091  # /etc/os-release is a runtime file, not a repo source to follow
    echo "build_host_os=$(. /etc/os-release && echo "$PRETTY_NAME")"
    echo "build_host_glibc=$build_host_glibc"
    # The minimum glibc a machine needs to run this AppImage at all — below
    # it, every binary here fails in the dynamic loader before any of the
    # hub's own error handling can run.
    echo "glibc_floor=$glibc_floor"
    for soname in "${FROZEN_SONAMES[@]}"; do
      resolve_library_version "$soname" "$verify_root"
    done
  } >"$version_record.tmp"
  mv "$version_record.tmp" "$version_record"
  echo "$(basename "$appimage"): sidecar verified pristine, GTK hook defers to the session backend, .DirIcon is relative, no session-owned client libraries bundled, GStreamer capture plugins present, frozen versions recorded to $(basename "$version_record")."
  rm -rf "$workdir"
  trap - EXIT
done
