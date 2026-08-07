//! Identity-agnostic half of the hub.
//!
//! Process supervision, sidecar spawning, port allocation, health polling and
//! log rotation land here in plan 002, ported from the station. Nothing in
//! this crate may reach for `tauri` — see the note in `Cargo.toml`.
//!
//! Every item ported from the station is `pub` here where it was `pub(crate)`
//! there: over there one binary crate held both halves, here the boundary is a
//! crate boundary and `hub/` is a separate consumer. That widening is the only
//! blanket edit the port makes — it changes no behaviour, and it also means
//! nothing in this crate needs `#[allow(dead_code)]` while its consumer is
//! still being written.

pub mod app_secret;
pub mod browser;
pub mod health;
pub mod log;
pub mod ports;
pub mod process;
pub mod sidecar;

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
