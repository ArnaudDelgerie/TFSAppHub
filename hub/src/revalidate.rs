//! Lazy revalidation — the other half of `needs-revalidation`.
//!
//! `reconcile.rs` marks; this module clears the mark, or turns it into
//! `broken`. The fast parent `open::resolve` only carries the mark; the child
//! runs this from its serve thread, behind the splash and after it holds the
//! launch locks, on that app's next `open <id>` after a hub self-update moved
//! its PHP. There is no background job and no revalidation at update time,
//! because the whole point of the mark is that most installed apps are never
//! opened again before the *next* self-update, and resolving dependencies for
//! an app nobody is about to run would be wasted work most of the time.
//!
//! **What actually changed, and what this deliberately does not do.** A hub
//! self-update can move two things: the FrankenPHP binary and the extensions
//! it loads. Neither moves the app's own code or its `app_version` — that is
//! still exactly what `install`/`update` last wrote. So this runs one
//! `composer install` against the snapshot's existing `composer.lock`, and
//! nothing else: no lifecycle command (CONTRACT.md §6's `pre-`/`post-update`
//! hooks answer a version change, not a platform one, and `app_version` has
//! not moved), and no rewrite of the data dir's version record, which already
//! names the right version and would gain nothing from being restamped to
//! itself.

use std::path::Path;

use crate::{
    app_env::{self, EnvError},
    manifest::Manifest,
    paths::{Paths, PathsError},
    php::{self, PhpError},
    platform::{self, PlatformError},
    registry::{self, RegistryError, State},
};

/// What came of re-resolving `id`'s dependencies.
#[derive(Debug)]
pub enum Outcome {
    /// Composer resolved cleanly against the running hub's PHP — carrying the
    /// `Platform` just probed, so a caller building plan 024's cache stamp
    /// never has to re-probe (or worse, reach for the registry entry's own
    /// in-memory copy, which this same call is what makes stale).
    Ready(registry::Platform),
    /// It did not. Composer's own explanation already reached the terminal by
    /// the time this is returned — [`php::Toolchain::composer_install`] runs
    /// with inherited stdio, the same as an `install`'s or an `update`'s — so
    /// nothing here repeats it.
    Broken,
}

/// Re-run `composer install --no-dev` for `id`, against the toolchain this hub
/// bundles right now, and write the outcome back to the registry under its
/// lock.
///
/// On [`Outcome::Ready`] the entry's `platform` is advanced to the one just
/// probed — the new baseline a future reconciliation compares against. On
/// [`Outcome::Broken`] it is left exactly as it was: that value is what
/// `OpenError::Broken`'s message means by "resolved against PHP …", and
/// overwriting it with the platform that just failed would make that sentence
/// say the opposite of what happened.
///
/// An entry that has vanished from the registry between `open::resolve`'s own
/// read and this call — a `remove` racing an `open` — is not an error here:
/// there is nothing left to write the outcome onto, and the caller's next
/// registry read will find it gone and say so in its own words.
pub fn revalidate(
    paths: &Paths,
    id: &str,
    app_dir: &Path,
    manifest: &Manifest,
) -> Result<Outcome, RevalidateError> {
    let toolchain = php::toolchain(paths)?;
    let platform = platform::probe(&toolchain.frankenphp)?.fingerprint();

    let state_root = paths.create_app_data_dir(&manifest.identifier)?;
    let environment = app_env::resolve(
        manifest,
        app_dir,
        &manifest.identifier,
        &state_root,
        app_env::Mode::Install,
    )?;

    let outcome = match toolchain.composer_install(app_dir, &environment.vars) {
        Ok(()) => Outcome::Ready(platform.clone()),
        Err(_) => Outcome::Broken,
    };

    registry::update(paths, |registry| {
        if let Some(entry) = registry.get_mut(id) {
            entry.state = match outcome {
                Outcome::Ready(_) => State::Ready,
                Outcome::Broken => State::Broken,
            };
            if let Outcome::Ready(ref platform) = outcome {
                entry.platform = platform.clone();
            }
            entry.updated_at = registry::now_timestamp();
        }
    })?;

    Ok(outcome)
}

#[derive(Debug)]
pub enum RevalidateError {
    Php(PhpError),
    Platform(PlatformError),
    Paths(PathsError),
    Env(EnvError),
    Registry(RegistryError),
}

impl std::fmt::Display for RevalidateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Php(error) => write!(formatter, "{error}"),
            Self::Platform(error) => write!(formatter, "{error}"),
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Env(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RevalidateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Php(error) => Some(error),
            Self::Platform(error) => Some(error),
            Self::Paths(error) => Some(error),
            Self::Env(error) => Some(error),
            Self::Registry(error) => Some(error),
        }
    }
}

impl From<PhpError> for RevalidateError {
    fn from(error: PhpError) -> Self {
        Self::Php(error)
    }
}

impl From<PlatformError> for RevalidateError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl From<PathsError> for RevalidateError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

impl From<EnvError> for RevalidateError {
    fn from(error: EnvError) -> Self {
        Self::Env(error)
    }
}

impl From<RegistryError> for RevalidateError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

#[cfg(test)]
#[path = "revalidate_tests.rs"]
mod tests;
