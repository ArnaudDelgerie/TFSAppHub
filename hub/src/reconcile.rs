//! Reconciliation: a changed `hub_version` marks what moved.
//!
//! `--update` (`hub_update.rs`) swaps the hub's binary and the PHP it bundles,
//! but does not itself know which installed apps that PHP move affects — it
//! runs before the new hub has ever probed its own platform, and the apps it
//! would need to compare against live in a registry it is not the one to
//! rewrite in the middle of a binary swap. This module is where that
//! comparison actually happens: on the *next* invocation, of any command that
//! acts on an app, against the hub that is now running.
//!
//! The registry's own `hub_version` is what makes the common case cheap: on
//! every command but the one right after a self-update, it already matches
//! the running hub and [`reconcile`] returns without ever starting
//! FrankenPHP to ask it anything. Only a mismatch is worth a probe.

use crate::{
    paths::Paths,
    platform::{self, PlatformError},
    registry::{self, RegistryError, State},
};

/// Compare the running hub's version against the registry's own record. Equal
/// → return immediately, no probe. Different → probe the platform once, mark
/// every entry whose fingerprint no longer matches `needs-revalidation` (an
/// entry already `broken` is left alone — it is already worse than marked),
/// then re-stamp the registry with the running hub's version and platform.
///
/// Called once from `main.rs`'s dispatch, before any `Level::App` command
/// runs. A failure here is reported and swallowed by the caller rather than
/// blocking the command it precedes: the command below either does not need
/// the registry at all, or is about to read it itself and will surface the
/// same failure in its own words.
pub fn reconcile(paths: &Paths, hub_version: &str) -> Result<(), ReconcileError> {
    let registry = registry::load(paths)?;
    if registry.hub_version.as_deref() == Some(hub_version) {
        return Ok(());
    }

    let platform = platform::hub_platform()?;
    let marked = registry::update(paths, |registry| {
        let marked: Vec<String> = registry
            .apps
            .iter_mut()
            .filter(|entry| entry.platform != platform && entry.state != State::Broken)
            .map(|entry| {
                entry.state = State::NeedsRevalidation;
                entry.id.clone()
            })
            .collect();
        registry.stamp(hub_version, platform.clone());
        marked
    })?;

    // Silent when nothing was marked: the ordinary shape of this branch is a
    // fresh install stamping the registry for the first time, or a self-update
    // whose new PHP happens to fingerprint the same as the old one. Neither is
    // news. A rewrite that *does* move an app's state must announce itself —
    // a silent one would be indistinguishable from a bug.
    if !marked.is_empty() {
        eprintln!(
            "tfsapp-hub: the hub's PHP changed under {} — {} on next use.",
            marked.join(", "),
            match marked.len() {
                1 => "it will revalidate",
                _ => "they will revalidate",
            }
        );
    }

    Ok(())
}

#[derive(Debug)]
pub enum ReconcileError {
    Registry(RegistryError),
    Platform(PlatformError),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::Platform(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ReconcileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Platform(error) => Some(error),
        }
    }
}

impl From<RegistryError> for ReconcileError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<PlatformError> for ReconcileError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

#[cfg(test)]
#[path = "reconcile_tests.rs"]
mod tests;
