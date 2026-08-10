//! The `gh` seam (`../plan/019-publish-an-app.md` step 3): every
//! authenticated call `publish` makes, run through GitHub's own CLI on the
//! *author's* machine — `../decision/003-the-hub-publishes-apps.md`'s
//! consequence that the hub never holds a forge credential, applied. Four
//! gated calls, each with its own refusal, and the one call that actually
//! publishes.
//!
//! [`Gh`] holds the program to run — `"gh"` in production, a fake script in
//! tests — as a parameter, never a `PATH` mutation: the test suite is
//! threaded and `PATH` is process-global. Every call is a `Command` with
//! fixed argv, never through a shell.

use std::{ffi::OsString, fmt, path::Path, process::Command, process::Output};

use serde::Deserialize;

use crate::release::SHA256SUMS_ASSET_NAME;

pub struct Gh {
    program: OsString,
}

impl Gh {
    /// The real `gh` on `PATH` — production's only constructor.
    pub fn new() -> Self {
        Self {
            program: OsString::from("gh"),
        }
    }

    /// A specific program to run instead of `"gh"` — always a test fixture,
    /// passed as a parameter rather than by mutating `PATH`. `pub(crate)` so
    /// `publish_tests.rs`'s own fixtures can build one too, not just this
    /// module's.
    #[cfg(test)]
    pub(crate) fn at(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<Output, GhError> {
        Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|_| GhError::NotInstalled)
    }

    /// Gate 6: `gh --version`. A spawn failure at *any* other call below is
    /// folded into this same refusal — if `gh` cannot be run at all, "not
    /// installed" is the accurate thing to tell the author, whichever call
    /// happened to be the one that tried it first.
    pub fn ensure_installed(&self) -> Result<(), GhError> {
        match self.run(&["--version"]) {
            Ok(output) if output.status.success() => Ok(()),
            _ => Err(GhError::NotInstalled),
        }
    }

    /// Gate 7: `gh auth status`.
    pub fn ensure_authenticated(&self) -> Result<(), GhError> {
        let output = self.run(&["auth", "status"])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(GhError::NotAuthenticated)
        }
    }

    /// Gate 8: `gh api repos/<repo>/git/ref/tags/<tag>` — settled in the
    /// plan's "the tag must already be pushed": `publish` never lets `gh`
    /// create the tag itself, since the hub has no git client with which to
    /// ask whether the working tree it just archived is what that tag would
    /// point at.
    pub fn ensure_tag_pushed(&self, repo: &str, tag: &str) -> Result<(), GhError> {
        let endpoint = format!("repos/{repo}/git/ref/tags/{tag}");
        let output = self.run(&["api", &endpoint])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(GhError::TagNotPushed {
                repo: repo.to_string(),
                tag: tag.to_string(),
            })
        }
    }

    /// Gate 9: no release already carries `tag`, draft included. Run
    /// authenticated — unlike 018's anonymous resolver, since a draft is
    /// invisible to an anonymous reader and is exactly the collision this
    /// guard exists to catch.
    pub fn ensure_no_existing_release(&self, repo: &str, tag: &str) -> Result<(), GhError> {
        match self.existing_release(repo, tag)? {
            Some(view) => Err(GhError::ReleaseExists {
                repo: repo.to_string(),
                tag: tag.to_string(),
                draft: view.is_draft,
            }),
            None => Ok(()),
        }
    }

    /// `gh release view <tag> --repo <repo> --json isDraft,assets` — `None`
    /// when no release carries `tag` at all (a failing `gh` exit, which is
    /// how `gh` reports "not found"), `Some` otherwise, carrying the two
    /// fields both the version guard and the post-upload cleanup need.
    fn existing_release(&self, repo: &str, tag: &str) -> Result<Option<ReleaseView>, GhError> {
        let output = self.run(&[
            "release",
            "view",
            tag,
            "--repo",
            repo,
            "--json",
            "isDraft,assets",
        ])?;
        if !output.status.success() {
            return Ok(None);
        }
        let view =
            serde_json::from_slice(&output.stdout).map_err(|error| GhError::UnreadableJson {
                context: "gh release view",
                detail: error.to_string(),
            })?;
        Ok(Some(view))
    }

    /// `gh release delete <tag> --repo <repo> --yes` — never called on its
    /// own refusal; only [`Self::create_release`]'s cleanup calls it, and
    /// folds any failure of its own into "did not clean up", since the
    /// original creation failure is what the author needs to see.
    fn delete_release(&self, repo: &str, tag: &str) -> bool {
        matches!(
            self.run(&["release", "delete", tag, "--repo", repo, "--yes"]),
            Ok(output) if output.status.success()
        )
    }

    /// `gh release create <tag> --repo <repo> --title <tag> --notes-file
    /// <notes_path> <archive_path> <sums_path>` — one process, fixed argv,
    /// never `--generate-notes` (the notes are the changelog section,
    /// verbatim). Returns the release URL `gh` prints on success.
    ///
    /// `gh` creates the release and *then* uploads assets, so a failed
    /// upload can leave a release with no archive on it. On failure this
    /// re-checks and, if the release now exists without both assets,
    /// deletes it — so a retry meets no version guard of the failed
    /// attempt's own making.
    pub fn create_release(
        &self,
        repo: &str,
        tag: &str,
        notes_path: &Path,
        archive_name: &str,
        archive_path: &Path,
        sums_path: &Path,
    ) -> Result<String, GhError> {
        let output = Command::new(&self.program)
            .arg("release")
            .arg("create")
            .arg(tag)
            .arg("--repo")
            .arg(repo)
            .arg("--title")
            .arg(tag)
            .arg("--notes-file")
            .arg(notes_path)
            .arg(archive_path)
            .arg(sums_path)
            .output()
            .map_err(|_| GhError::NotInstalled)?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let incomplete_release_deleted = self.clean_up_incomplete_release(repo, tag, archive_name);
        Err(GhError::CreateFailed {
            stderr,
            incomplete_release_deleted,
        })
    }

    /// After a failed `release create`: delete the release `tag` now names,
    /// but only if it is missing either asset — a release that somehow has
    /// both is not this cleanup's business to touch. Returns whether a
    /// deletion actually happened.
    fn clean_up_incomplete_release(&self, repo: &str, tag: &str, archive_name: &str) -> bool {
        let Ok(Some(view)) = self.existing_release(repo, tag) else {
            return false;
        };
        let complete = view.assets.iter().any(|asset| asset.name == archive_name)
            && view
                .assets
                .iter()
                .any(|asset| asset.name == SHA256SUMS_ASSET_NAME);
        !complete && self.delete_release(repo, tag)
    }
}

/// The subset of `gh release view`'s JSON this module reads.
#[derive(Deserialize, Debug)]
struct ReleaseView {
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize, Debug)]
struct ReleaseAsset {
    name: String,
}

/// Everything the `gh` seam can refuse over.
#[derive(Debug)]
pub enum GhError {
    /// `gh` could not be run at all — covers "not installed" and any other
    /// spawn failure, at any call (see [`Gh::ensure_installed`]'s doc).
    NotInstalled,
    /// `gh auth status` failed.
    NotAuthenticated,
    /// `v<app_version>` does not exist on the repository yet.
    TagNotPushed { repo: String, tag: String },
    /// A release already carries the tag — draft included.
    ReleaseExists {
        repo: String,
        tag: String,
        draft: bool,
    },
    /// `gh release create` failed; `incomplete_release_deleted` says whether
    /// a half-created release (missing an asset) was found and removed.
    CreateFailed {
        stderr: String,
        incomplete_release_deleted: bool,
    },
    /// `gh` printed something that did not parse as the JSON this module
    /// expected from it.
    UnreadableJson {
        context: &'static str,
        detail: String,
    },
}

impl fmt::Display for GhError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => write!(
                formatter,
                "gh is not installed (or could not be run) — install it from \
                 https://cli.github.com."
            ),
            Self::NotAuthenticated => {
                write!(formatter, "gh is not authenticated — run `gh auth login`.")
            }
            Self::TagNotPushed { repo, tag } => write!(
                formatter,
                "{tag} does not exist on {repo} — push it first: git tag {tag} && git push \
                 origin {tag}."
            ),
            Self::ReleaseExists { repo, tag, draft } => write!(
                formatter,
                "release {tag} already exists on {repo}{} — bump app_version, or delete the {} \
                 release on the forge.",
                if *draft { " as a draft" } else { "" },
                if *draft { "draft" } else { "existing" },
            ),
            Self::CreateFailed {
                stderr,
                incomplete_release_deleted,
            } => {
                write!(formatter, "gh release create failed: {stderr}")?;
                if *incomplete_release_deleted {
                    write!(
                        formatter,
                        " — the half-created release (missing an asset) was deleted, so a retry \
                         starts clean."
                    )?;
                }
                Ok(())
            }
            Self::UnreadableJson { context, detail } => write!(
                formatter,
                "{context} printed output that isn't the JSON expected: {detail}"
            ),
        }
    }
}

impl std::error::Error for GhError {}

#[cfg(test)]
#[path = "gh_tests.rs"]
mod tests;
