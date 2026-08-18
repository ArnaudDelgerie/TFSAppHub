## Runtime identity: one binary, N applications

The hard problem, and the one that gated the whole design.

Everything an operating system keys per application — the GTK application id,
the D-Bus name, the single-instance key, `WM_CLASS`, and the WebKitGTK
website-data directory holding the app's cookies — normally comes from a value
baked into the binary at build time. The hub has one binary and N apps, so all
of it has to follow a value resolved from argv instead.

It does, and almost for free: Tauri reads the app id from the **runtime** config
when the builder starts GTK, so mutating the context's identifier beforehand
moves all of them at once. This was measured before it was relied on — two
identities of one binary launched side by side on Wayland and X11 produced two
owned bus names, two window classes and two separate `~/.local/share/<identifier>/`
trees, with no cookie crossing in either direction despite both windows sharing
the `127.0.0.1` cookie origin.

The constraint that falls out: **all of it must happen before the builder runs,
and before anything else touches GTK.** That is why the config mutation and
`set_prgname` live in the same entry point — they share one deadline, and
splitting them is how one of them eventually gets forgotten.

