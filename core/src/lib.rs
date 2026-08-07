//! Identity-agnostic half of the hub.
//!
//! Process supervision, sidecar spawning, port allocation, health polling and
//! log rotation land here in plan 002, ported from the station. Nothing in
//! this crate may reach for `tauri` — see the note in `Cargo.toml`.

/// This crate's version, as recorded by Cargo.
///
/// A placeholder so the skeleton has one thing worth asserting; it also keeps
/// `hub/` linked against `tfsapp-core` from day one, so the dependency edge is
/// exercised by the build rather than merely declared.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_the_crate_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
