//! The app-version boundary shared by publication and installation.
//!
//! `semver::Version` accepts pre-release and build metadata by design. The
//! app-manifest contract is narrower: an app version is exactly
//! `MAJOR.MINOR.PATCH`, without a prefix, leading zeroes, suffixes or
//! whitespace. Keep that distinction here so every boundary applies one rule.

use std::fmt;

/// Parse an app-manifest version only when it has the contract's exact
/// `MAJOR.MINOR.PATCH` spelling.
///
/// No caller may normalise the input first: rejecting the author-provided
/// spelling is the point of this boundary. The returned value preserves the
/// semantic comparisons callers already need for install and update decisions.
pub(crate) fn parse_app_version(raw: &str) -> Result<semver::Version, AppVersionError> {
    let version = semver::Version::parse(raw).map_err(|_| AppVersionError)?;

    if !version.pre.is_empty() || !version.build.is_empty() || version.to_string() != raw {
        return Err(AppVersionError);
    }

    Ok(version)
}

/// The stable, contract-facing refusal for every forbidden app-version
/// spelling. Parser details intentionally stay private: they vary by spelling
/// but do not change what an app author must correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppVersionError;

impl fmt::Display for AppVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "must be canonical MAJOR.MINOR.PATCH, with no prefix, leading zeroes or suffix"
        )
    }
}

impl std::error::Error for AppVersionError {}

#[cfg(test)]
#[path = "version_tests.rs"]
mod tests;
