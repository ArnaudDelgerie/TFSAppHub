//! `tfsapp-hub dev <path>` — the live-source constructor of [`LaunchSpec`].
//!
//! The counterpart to `open::resolve`: where that one reads an installed
//! snapshot from the registry, this one reads a project directory straight
//! from the filesystem, live, never snapshotted (`.project/plan/009-dev-local-
//! source.md`). Both are pure resolve functions — no sidecar, no window, no
//! guard — and both exist to let a mistyped path or a missing file fail at a
//! terminal instead of inside a half-open window.
//!
//! **The identity is `dev.<identifier>`.** `dev ./x` and `open x` (once `x` is
//! installed) resolve to the same manifest, so without a prefix they would
//! share a GTK app id, a single-instance key, a WebKit cookie store and the
//! §5 liveness lock — while pointing at two different databases. The prefix
//! is a runtime namespace only: `product_name` and `icon_path` stay the
//! project's own, unprefixed, because a user should never read "dev." in a
//! window title.

use std::{fmt, path::PathBuf};

use crate::{
    identity::Identity,
    launch::{LaunchSpec, Source},
    manifest::{self, ManifestError},
};

/// The runtime-identity namespace a dev session's identifier is prefixed
/// with — see the module header.
pub const DEV_IDENTIFIER_PREFIX: &str = "dev.";

/// The live constructor of [`LaunchSpec`]: check the project looks like a
/// TFSApp, load its manifest, and build the spec dev's own launch pipeline
/// (steps 3–4) will run against.
///
/// Every refusal below names the file it looked for and the path it looked
/// under — the same principle `open::resolve` follows, for the same reason:
/// the audience is someone at a terminal who typed one path and got nothing.
/// Order matters only in that it is the order CONTRACT.md §1 lists an app's
/// required layout, so the first refusal to fire is the first thing a reader
/// of that section would check too.
pub fn resolve(project_path: &str) -> Result<LaunchSpec, DevError> {
    let project_path = PathBuf::from(project_path);
    if !project_path.is_dir() {
        return Err(DevError::NotADirectory { path: project_path });
    }

    for (what, relative) in [
        ("bin/console", "bin/console"),
        ("public/index.php", "public/index.php"),
    ] {
        let path = project_path.join(relative);
        if !path.is_file() {
            return Err(DevError::MissingEntryPoint { what, path });
        }
    }

    let loaded = manifest::load(&project_path)?;
    let manifest = loaded.manifest;

    let identity = Identity {
        identifier: format!("{DEV_IDENTIFIER_PREFIX}{}", manifest.identifier),
        product_name: manifest.product_name.clone(),
        icon_path: manifest
            .icon_path
            .as_ref()
            .map(|relative| project_path.join(relative)),
    };

    let label = project_path.display().to_string();
    let state_root = project_path.join("var");

    Ok(LaunchSpec {
        source: Source::Live,
        app_dir: project_path,
        identity,
        manifest,
        state_root,
        label,
        warnings: loaded.warnings,
    })
}

/// Every way `dev <path>` can be refused before anything is spawned.
#[derive(Debug)]
pub enum DevError {
    NotADirectory { path: PathBuf },
    MissingEntryPoint { what: &'static str, path: PathBuf },
    Manifest(ManifestError),
}

impl fmt::Display for DevError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotADirectory { path } => write!(
                formatter,
                "{} is not a directory. `tfsapp-hub dev` runs a project's live source in \
                 place — point it at the project's own root.",
                path.display()
            ),
            Self::MissingEntryPoint { what, path } => write!(
                formatter,
                "{} does not exist. A TFSApp project must have a {what} (CONTRACT.md §1).",
                path.display()
            ),
            Self::Manifest(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DevError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ManifestError> for DevError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

#[cfg(test)]
#[path = "dev_tests.rs"]
mod tests;
