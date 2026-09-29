## Packaging

The hub is packaged once, as an AppImage, and that build decides the
compatibility floor for the entire fleet — it is the only binary anyone links.
An AppImage's portability starts with the glibc it was linked against and
the WebKitGTK ABI it expects; both come from the builder's machine. Its
GStreamer plugins also come from that machine and can require compatible
graphics and audio libraries from the user's session. Rather than fighting
that with a pinned build base of its own, the floor is simply **a property
of whatever built the image**, measured and
recorded beside the artifact in `.versions.txt` — the highest `GLIBC_x.y`
symbol version any bundled ELF imports, the build host's own glibc, the OS,
and the frozen versions of its WebKit/GTK/GLib and GStreamer libraries.
No single release covers every distribution a user might be on; the answer
offered to whoever it fails is `build/compose.yaml`'s `docker compose run --rm
build`, with `BASE_IMAGE` set to a base older than the one that produced the
release (see README.md's "It does not start"). The container is a wrapper
around exactly `make build` / `make check` and nothing else knows it exists —
no branch anywhere in `build/scripts/` or in Rust asks whether it is running
inside one.

**`cargo tauri build` gets three things wrong, repaired after the fact by
`build/scripts/fix-appimage-bundle.sh`.** linuxdeploy runs `patchelf` (rpath →
`$ORIGIN`) on every ELF file it bundles, which corrupts the FrankenPHP
sidecar — a static-PIE Go binary whose rewritten program headers SIGSEGV on
every exec (a known upstream patchelf limitation) — so the pristine binary is
copied back in after the fact. Its GTK plugin writes an `apprun-hooks` script
that hard-forces `GDK_BACKEND=x11`, pinning the whole app to XWayland on a
Wayland session; the hook is rewritten to defer to the session's own backend.
And `.DirIcon` is written as an absolute symlink into the *build machine's*
own AppDir path, broken the moment the image is mounted anywhere else; it is
replaced with a relative symlink to the icon named by the AppImage's own
`.desktop` `Icon=` line. The pass also deletes every bundled `libwayland-*`:
unlike the WebKit/GTK stack, the Wayland *client* library is an ABI the
running session owns, and freezing a copy older than the host's own
Mesa/compositor is a known way to fail on a newer distribution — so the
dynamic linker is left to fall through to the session's own copy instead.
Every repair is verified against the repacked artifact, not assumed from the
input to the repack.

`bundleMediaFramework` makes linuxdeploy copy the build host's GStreamer
plugins and helpers into the AppImage. Its `apprun-hooks` script points
GStreamer at those bundled plugins, so WebKit loads plugins built for the
same frozen GStreamer core library. The final-artifact check refuses an image
without `libgstapp.so`, `libgstcoreelements.so`, or an audio source plugin
(`libgstpulseaudio.so` or `libgstpipewire.so`). The repair pass removes
bundled `libpipewire-*` and `libpulse*` client libraries: their protocol and
PipeWire SPA modules belong to the running audio session. `.versions.txt`
records the frozen `libgstreamer-1.0.so.0` package version. The plugin set
did not raise the measured glibc floor on Ubuntu 24.04 (2.39), and a real
Debian 12 build recorded 2.36 and ran on Ubuntu 24.04. Optional plugins can
still depend on host graphics, X11/Wayland, ALSA, USB, or C++ libraries; a
Docker build on an older base reduces symbol-version risk but cannot supply
an absent host service or device.

### Text rendering

Every WebView this process creates renders text with greyscale antialiasing
rather than subpixel, via `gtk-xft-rgba = none` set on GTK's settings object in
`.setup()`, before any window exists — the whole app, GTK chrome included, and
no stylesheet interacts with it. WebKitGTK renders text with subpixel
antialiasing only on the root layer; an `overflow-y: auto` column that
overflows gets promoted to a non-root composited layer, where rendering falls
back to greyscale, and Skia's mask-gamma preblend — designed to correct per
channel against an LCD mask — lands "in the nearest gray instead of the
nearest colour" against a greyscale mask, so dark text visibly fattens the
moment a column overflows. Measured on one machine (TFSAppWorkstation's plan
058, the station this repo inherited the setting from): **+16.1%** ink per
text pixel between a column below the fold and the same column scrolled,
down to **+0.2%** (noise) once the whole app renders greyscale uniformly.

This is deliberately not configurable — no key exists in `CONTRACT.md`, and
none is planned. The cost is real, not zero: subpixel antialiasing is a
genuine horizontal-resolution gain around 96 dpi, and this removes it from
every app, including ones that never showed the defect. On HiDPI it is a
non-event. The ecosystem took the same direction anyway — GTK4 dropped
subpixel text rendering outright, and macOS has shipped greyscale-only since
Mojave — so this trades a resolution gain few displays still cash in for a
rendering mode that no longer silently switches mid-screen.
