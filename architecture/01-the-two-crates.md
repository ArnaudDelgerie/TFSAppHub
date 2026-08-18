## The two crates

```
core/   identity-agnostic: process supervision, sidecar spawning, port
        allocation, health polling, log rotation, APP_SECRET, external links
hub/    everything that knows which app it is serving
```

**`core/` never depends on `tauri`.** The rule is enforced by the dependency
graph rather than by convention, and it is what keeps the supervision half
testable in CI on a machine with no display, no GTK and no session bus.

The boundary is not "reusable versus not". It is "does this need to know which
app it is?" — a process group, a port, a health poll and a log file do not. A
window, a keyring namespace, a data directory and a manifest do.

Three modules currently sit in `hub/` that belong below the line — `secrets`,
`bridge` and `worker` — because they read hub-side configuration types and
Tauri state. They move down when those inputs become parameters, and when a
second consumer exists to justify it. Not for tidiness.

