//! What PHP looks like, from the bundled FrankenPHP's own mouth.
//!
//! The hub bundles one FrankenPHP and runs every installed app with it. A hub
//! self-update therefore replaces PHP *underneath apps that are already
//! installed*, whose `composer.lock` was resolved against the old one. The
//! station never had this problem: one AppImage, one PHP, one app, updated
//! together or not at all.
//!
//! [`Platform`] is how the hub notices. Each app records the fingerprint it was
//! installed against; after a self-update the running hub's own fingerprint is
//! compared with each of them, and the ones that differ are marked for
//! revalidation — `composer install` against the existing lock, on that app's
//! next use, lazily. What acts on that mark is the revalidation flow's job; all
//! that is settled here is how the value is computed, because it has to be
//! recorded from the very first install or there would be nothing to compare
//! against later.
//!
//! **It is not a security check and not a version pin.** It proves nothing
//! about the binary, authenticates nothing, and blocks no launch on its own: an
//! app whose fingerprint differs is revalidated, never refused. Read as
//! anything stronger it would be a false guarantee.
//!
//! Both halves come from one `php-cli` call at runtime — `major.minor` because
//! that is the granularity a `composer.lock`'s platform requirements are
//! written at, and the loaded-extension list because a missing extension breaks
//! a lock just as surely as a version bump. Nothing is baked at build time,
//! which is what lets the same binary answer for whatever FrankenPHP it happens
//! to be shipping.

// Read for real by the revalidation flow and written by the installer (plans
// 006 and after); today the hidden `__platform` subcommand in `main.rs` is its
// only caller. Remove the allow with the first real consumer.
#![allow(dead_code)]

use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

use crate::registry::Platform;

/// One `php-cli` line: the version this fingerprint is keyed on, then the
/// loaded extensions.
///
/// `get_loaded_extensions()` is asked for its raw order and sorted on the Rust
/// side — the sort is part of the fingerprint's definition, and defining it
/// here rather than in a PHP `sort()` keeps it out of reach of anything the
/// interpreter's own collation might do differently between builds.
const PROBE_SCRIPT: &str = r#"echo PHP_MAJOR_VERSION, ".", PHP_MINOR_VERSION, "\n", implode(",", get_loaded_extensions());"#;

/// What the bundled interpreter answered, before it is reduced to a
/// fingerprint.
///
/// The extension list is kept whole here, and only hashed on the way into the
/// registry: a human comparing the hub against a real `php -m` needs the names,
/// and the registry needs one short comparable value. Same data, two audiences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `major.minor`, e.g. `8.5`.
    pub php_version: String,
    /// Sorted and deduplicated, so the fingerprint depends on the *set* of
    /// extensions and not on the order PHP happened to list them in.
    pub extensions: Vec<String>,
}

impl Probe {
    pub fn fingerprint(&self) -> Platform {
        let mut hasher = Sha256::new();
        for extension in &self.extensions {
            hasher.update(extension.as_bytes());
            hasher.update(b"\n");
        }
        let digest = hasher.finalize();

        Platform {
            php_version: self.php_version.clone(),
            extensions_hash: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }
}

/// `bundle.resources` places `resources/frankenphp` and `resources/composer.phar`
/// under `<AppDir>/usr/lib/<productName>/` — the AppImage's own resource dir.
/// `tauri::utils::platform::resource_dir` reads it back from `APPDIR`, which the
/// AppImage runtime exports before `AppRun` hands off, so this needs no
/// `AppHandle` — the reason it can run from `install`, a headless CLI command
/// with no `tauri::Builder` in sight. `None` outside an AppImage (`APPDIR` unset
/// and no `../lib/<productName>` beside the binary): that is the ordinary case
/// for `cargo run` and `cargo test`, and callers fall through to the dev path.
///
/// `resource_dir` only reads `PackageInfo.name`, so a `PackageInfo` is built
/// here by hand instead of invoking `tauri::generate_context!()` a second time
/// (which would re-embed the icon and re-parse `tauri.conf.json` for nothing).
/// `PACKAGE_NAME` has to equal `tauri.conf.json`'s `"productName"` exactly — the
/// AppImage bundler names the resource dir after it — and
/// `platform_tests::the_package_name_matches_tauri_conf_json` is the guard
/// against the two drifting apart.
pub(crate) fn packaged_resource_dir() -> Option<PathBuf> {
    const PACKAGE_NAME: &str = "TFSAppHub";
    let package_info = tauri::PackageInfo {
        name: PACKAGE_NAME.to_string(),
        version: semver::Version::new(0, 0, 0),
        authors: "",
        description: "",
        crate_name: "",
    };
    tauri::utils::platform::resource_dir(&package_info, &tauri::Env::default()).ok()
}

/// Ordered candidates for the bundled FrankenPHP: the packaged AppImage's
/// resource dir first, then `<hub>/resources/frankenphp` — `make sidecar`'s
/// download, and where the crate lives while run from a build.
/// `resolve_frankenphp_binary` picks the first that is a real, non-empty,
/// executable file, and covers the system-wide `/usr/bin/frankenphp` fallback
/// either way.
pub fn bundled_frankenphp() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(resource_dir) = packaged_resource_dir() {
        candidates.push(resource_dir.join("resources/frankenphp"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/frankenphp"));
    candidates
}

/// The fingerprint of the interpreter this hub actually runs apps with.
pub fn hub_platform() -> Result<Platform, PlatformError> {
    Ok(probe(&hub_frankenphp()?)?.fingerprint())
}

/// The interpreter this hub runs apps with: the bundled one, or a system-wide
/// install as a fallback.
pub fn hub_frankenphp() -> Result<PathBuf, PlatformError> {
    tfsapp_core::sidecar::resolve_frankenphp_binary(&bundled_frankenphp())
        .map_err(|error| PlatformError::NoInterpreter(error.to_string()))
}

/// Ask `frankenphp` what it is.
pub fn probe(frankenphp: &Path) -> Result<Probe, PlatformError> {
    let output = Command::new(frankenphp)
        .arg("php-cli")
        .arg("-r")
        .arg(PROBE_SCRIPT)
        .output()
        .map_err(|source| PlatformError::Unstartable {
            binary: frankenphp.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(PlatformError::Failed {
            binary: frankenphp.to_path_buf(),
            status: match output.status.code() {
                Some(code) => format!("exited with status {code}"),
                None => "was killed by a signal".to_string(),
            },
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

/// The pure half of [`probe`]: turn the two printed lines into a [`Probe`].
fn parse_probe(stdout: &str) -> Result<Probe, PlatformError> {
    let unreadable = |detail: &str| PlatformError::Unreadable {
        detail: detail.to_string(),
        output: stdout.trim().to_string(),
    };

    let mut lines = stdout.trim().lines();
    let php_version = lines.next().unwrap_or_default().trim();
    let mut parts = php_version.split('.');
    let looks_like_a_version = matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(major), Some(minor), None)
            if !major.is_empty()
                && !minor.is_empty()
                && major.chars().all(|c| c.is_ascii_digit())
                && minor.chars().all(|c| c.is_ascii_digit())
    );
    if !looks_like_a_version {
        return Err(unreadable(
            "the first line should be a major.minor PHP version",
        ));
    }

    let mut extensions: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    if extensions.is_empty() {
        // Not a pedantic check: an interpreter with no extensions at all would
        // hash identically to one whose list failed to print, and the two mean
        // very different things for an app's lock file.
        return Err(unreadable(
            "the second line should list the loaded extensions",
        ));
    }
    extensions.sort();
    extensions.dedup();

    Ok(Probe {
        php_version: php_version.to_string(),
        extensions,
    })
}

#[derive(Debug)]
pub enum PlatformError {
    /// No FrankenPHP anywhere — the hub cannot run a single app without one, so
    /// this is fatal wherever it appears, not a degraded mode.
    NoInterpreter(String),
    Unstartable {
        binary: PathBuf,
        source: std::io::Error,
    },
    Failed {
        binary: PathBuf,
        status: String,
        stderr: String,
    },
    Unreadable {
        detail: String,
        output: String,
    },
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoInterpreter(detail) => write!(formatter, "{detail}"),
            Self::Unstartable { binary, source } => {
                write!(formatter, "cannot run {}: {source}", binary.display())
            }
            Self::Failed {
                binary,
                status,
                stderr,
            } => write!(
                formatter,
                "{} {status} while reporting its PHP version: {stderr}",
                binary.display()
            ),
            Self::Unreadable { detail, output } => write!(
                formatter,
                "cannot read the PHP platform — {detail}. It answered: {output:?}"
            ),
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unstartable { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "platform_tests.rs"]
mod tests;
