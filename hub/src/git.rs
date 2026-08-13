//! The `git` gate (`../plan/019-publish-an-app.md` step 5): the one place
//! `publish` may run `git`, on the author's own machine, to establish that
//! the archive it is about to build **is** a commit that already exists on
//! the forge — `../decision/002-remote-sources-are-releases.md`'s
//! prohibition on shelling out to `git`, read (per that decision's own
//! 2026-08-10 revision note) as being about the *user's* machine mid-install,
//! not the author's here. Nothing here clones, fetches, reads history, or
//! pushes; one directory's worktree state, once.
//!
//! [`Git`] holds the program to run — `"git"` in production, a fake script
//! in tests — as a parameter, never a `PATH` mutation, matching `gh.rs`'s
//! `Gh`. It lives beside it because it is the same kind of seam, not because
//! the two modules are related: nothing here calls `gh`, and nothing in
//! `gh.rs` calls `git`.

use std::{
    ffi::OsString,
    fmt, io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::Duration,
};

use crate::source;

/// `ETXTBSY` (errno 26 on Linux): the kernel refuses to `exec` a file that
/// some process, anywhere on the system, still holds open for writing. The
/// test fixtures write a fresh script and exec it immediately; a `fork()`
/// from an unrelated, parallel test can transiently inherit another
/// thread's open-for-write fd on some file between that thread's `fork()`
/// and its own `exec()`, and the window lands on this exec often enough to
/// flake `make check`. Production never triggers it — no one else on the
/// real `git`/`gh` binaries. A bounded retry absorbs the race instead of
/// this call surfacing it as a false "not installed".
const ETXTBSY: i32 = 26;
const SPAWN_RETRY_LIMIT: u32 = 100;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Run `command`, retrying only on [`ETXTBSY`] up to [`SPAWN_RETRY_LIMIT`]
/// times — any other outcome, success or failure, returns immediately.
fn spawn_with_retry(command: &mut Command) -> io::Result<Output> {
    let mut retries_left = SPAWN_RETRY_LIMIT;
    loop {
        match command.output() {
            Err(error) if retries_left > 0 && error.raw_os_error() == Some(ETXTBSY) => {
                retries_left -= 1;
                thread::sleep(SPAWN_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

pub struct Git {
    program: OsString,
}

impl Git {
    /// The real `git` on `PATH` — production's only constructor.
    pub fn new() -> Self {
        Self {
            program: OsString::from("git"),
        }
    }

    /// A specific program to run instead of `"git"` — always a test fixture,
    /// passed as a parameter rather than by mutating `PATH`.
    #[cfg(test)]
    pub(crate) fn at(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// `git -C <project> <args>` — every call goes through here, so `-C` is
    /// written in exactly one place. A spawn failure (`git` missing) is
    /// [`GitError::NotInstalled`]; a call that ran but exited non-zero is the
    /// caller's to interpret, since what a non-zero exit *means* differs by
    /// call (not a work tree, no such remote, …).
    fn run(&self, project: &Path, args: &[&str]) -> Result<Output, GitError> {
        let mut command = Command::new(&self.program);
        command.arg("-C").arg(project).args(args);
        spawn_with_retry(&mut command).map_err(|source| GitError::NotInstalled { source })
    }

    /// `git -C <project> rev-parse --show-toplevel` — gate 3: refuses when
    /// `project` is not inside a git work tree at all. The value it returns
    /// is the *repository's* root, not necessarily `project` itself (a
    /// project can sit one or more directories inside its repo); nothing
    /// downstream of this gate needs that root, since the archive is still
    /// exactly `project`'s own tree.
    pub fn repo_root(&self, project: &Path) -> Result<PathBuf, GitError> {
        let output = self.run(project, &["rev-parse", "--show-toplevel"])?;
        if !output.status.success() {
            return Err(GitError::NotAWorkTree {
                project: project.to_path_buf(),
            });
        }
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    }

    /// `git -C <project> status --porcelain -b -- .` — gates 4 and 5 in one
    /// call: `-b` prints the branch/upstream/ahead/behind header line, `--
    /// .` scopes the file list to `project` itself rather than the whole
    /// repository (the point of the distinction [`Self::repo_root`]
    /// documents).
    pub fn status(&self, project: &Path) -> Result<Status, GitError> {
        let output = self.run(project, &["status", "--porcelain", "-b", "--", "."])?;
        if !output.status.success() {
            return Err(GitError::NotAWorkTree {
                project: project.to_path_buf(),
            });
        }
        Ok(parse_status(&String::from_utf8_lossy(&output.stdout)))
    }

    /// `git -C <project> rev-parse HEAD` — the sha `gh release create
    /// --target` puts the tag on.
    pub fn head_sha(&self, project: &Path) -> Result<String, GitError> {
        let output = self.run(project, &["rev-parse", "HEAD"])?;
        if !output.status.success() {
            return Err(GitError::NotAWorkTree {
                project: project.to_path_buf(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// `git -C <project> ls-files -z` — the project's tracked paths, relative
    /// to `project`. `publish` calls this only after [`Self::ensure_pushed`]
    /// proved the tracked tree is clean and pushed, so this list is precisely
    /// the commit whose sha it will tag.
    pub fn ls_files(&self, project: &Path) -> Result<Vec<PathBuf>, GitError> {
        let output = self.run(project, &["ls-files", "-z"])?;
        if !output.status.success() {
            return Err(GitError::NotAWorkTree {
                project: project.to_path_buf(),
            });
        }
        Ok(parse_ls_files(&output.stdout))
    }

    /// `git -C <project> remote get-url <remote>` — gate 6's other half, once
    /// [`Self::status`] has named which remote the branch tracks.
    fn remote_url(&self, project: &Path, remote: &str) -> Result<String, GitError> {
        let output = self.run(project, &["remote", "get-url", remote])?;
        if !output.status.success() {
            return Err(GitError::NoRemote {
                remote: remote.to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Gates 3–6, run in order, folded into what `publish` needs from all
    /// four: the commit being archived, the branch that names it, and the
    /// repository it resolves to. `explicit_repo` (`--repo`) wins outright
    /// over gate 6's own remote read, matching the Overview table's "the
    /// remote it read and that `--repo` overrides it".
    pub fn ensure_pushed(
        &self,
        project: &Path,
        explicit_repo: Option<&str>,
    ) -> Result<Commit, GitError> {
        // Gate 3. The root itself is not needed again — the call is the
        // gate.
        self.repo_root(project)?;

        let status = self.status(project)?;
        // Gate 4.
        if !status.dirty_paths.is_empty() {
            return Err(GitError::Dirty {
                paths: status.dirty_paths,
            });
        }
        // Gate 5.
        let upstream = status
            .upstream
            .clone()
            .ok_or_else(|| GitError::NoUpstream {
                branch: status.branch.clone(),
            })?;
        if status.ahead > 0 {
            return Err(GitError::Ahead {
                branch: status.branch.clone(),
                ahead: status.ahead,
            });
        }

        let sha = self.head_sha(project)?;

        // Gate 6.
        let repo = match explicit_repo {
            Some(repo) => repo.to_string(),
            None => {
                let remote = upstream.split('/').next().unwrap_or("origin").to_string();
                let url = self.remote_url(project, &remote)?;
                github_owner_repo(&url).ok_or_else(|| GitError::UnrecognisedRemote {
                    remote,
                    url: url.clone(),
                })?
            }
        };

        Ok(Commit {
            branch: status.branch,
            sha,
            repo,
        })
    }
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

/// `owner/repo` out of a `git remote get-url` result, whichever of the two
/// spellings that family of repositories uses — `source.rs`'s own extractors,
/// shared rather than re-derived (see [`source::github_https_repo`]'s doc).
fn github_owner_repo(url: &str) -> Option<String> {
    source::github_https_repo(url).or_else(|| source::github_ssh_repo(url))
}

/// What `git status --porcelain -b` printed, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub branch: String,
    /// `<remote>/<branch>`, or `None` when the branch has no upstream at all.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Every changed path the call reported — modified, staged or untracked
    /// alike, since all three would ship in the archive.
    pub dirty_paths: Vec<String>,
}

/// The commit [`Git::ensure_pushed`] proved is on the forge, and what
/// `publish` needs from it: the sha `gh release create --target` tags, the
/// branch the announcement names, and the repository the release is created
/// on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub branch: String,
    pub sha: String,
    pub repo: String,
}

/// The pure half of [`Git::status`]: turn `git status --porcelain -b`'s
/// stdout into a [`Status`], no filesystem or process involved — what makes
/// the branch-line parsing testable without a real `git`.
fn parse_status(stdout: &str) -> Status {
    let mut lines = stdout.lines();
    let (branch, upstream, ahead, behind) = match lines.next() {
        Some(header) => parse_branch_line(header),
        None => (String::new(), None, 0, 0),
    };
    let dirty_paths = lines
        .filter(|line| !line.is_empty())
        // Porcelain v1: two status characters, one space, then the path (a
        // rename prints "old -> new" here, left as one string — this gate
        // only ever *names* what changed, never parses it further).
        .map(|line| line.get(3..).unwrap_or(line).to_string())
        .collect();
    Status {
        branch,
        upstream,
        ahead,
        behind,
        dirty_paths,
    }
}

/// The pure half of [`Git::ls_files`]: split its NUL-delimited output without
/// interpreting filenames as text, since Git permits both newlines and bytes
/// outside UTF-8 in a tracked path.
fn parse_ls_files(stdout: &[u8]) -> Vec<PathBuf> {
    stdout
        .split(|byte| *byte == b'\0')
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(OsString::from_vec(path.to_vec())))
        .collect()
}

/// The `## ` header line's four fields. Handles every shape `--porcelain -b`
/// prints for a branch with commits: `branch`, `branch...remote/branch`, and
/// the same with a trailing `[ahead N]` / `[behind N]` / `[ahead N, behind
/// N]`. A detached `HEAD` or a branch with no commits yet parses as a bare
/// branch name with no upstream — neither can pass gate 5, so no special
/// handling is needed beyond not panicking on them.
fn parse_branch_line(line: &str) -> (String, Option<String>, u32, u32) {
    let rest = line.strip_prefix("## ").unwrap_or(line);
    let (head, tracking) = match rest.split_once(" [") {
        Some((head, bracket)) => (head, bracket.strip_suffix(']')),
        None => (rest, None),
    };
    let (branch, upstream) = match head.split_once("...") {
        Some((branch, upstream)) => (branch.to_string(), Some(upstream.to_string())),
        None => (head.to_string(), None),
    };

    let mut ahead = 0;
    let mut behind = 0;
    if let Some(tracking) = tracking {
        for part in tracking.split(", ") {
            if let Some(count) = part.strip_prefix("ahead ") {
                ahead = count.parse().unwrap_or(0);
            } else if let Some(count) = part.strip_prefix("behind ") {
                behind = count.parse().unwrap_or(0);
            }
        }
    }

    (branch, upstream, ahead, behind)
}

/// Everything the `git` gate can refuse over.
#[derive(Debug)]
pub enum GitError {
    /// `git` could not be run at all — the spawn's own `io::Error` (missing
    /// binary, permission denied, an [`ETXTBSY`] retry exhausted, ...),
    /// carried rather than discarded.
    NotInstalled { source: io::Error },
    /// `project` is not inside a git work tree.
    NotAWorkTree { project: PathBuf },
    /// `project` has an uncommitted or untracked file — named, since all of
    /// them would ship in the archive.
    Dirty { paths: Vec<String> },
    /// The branch has no upstream.
    NoUpstream { branch: String },
    /// The branch is ahead of its upstream — commits sitting locally that
    /// have not been pushed.
    Ahead { branch: String, ahead: u32 },
    /// `git remote get-url <remote>` itself failed.
    NoRemote { remote: String },
    /// The remote's URL is not a `github.com` spelling this hub recognises.
    UnrecognisedRemote { remote: String, url: String },
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled { source } => write!(
                formatter,
                "git is not installed (or could not be run): {source}."
            ),
            Self::NotAWorkTree { project } => write!(
                formatter,
                "{} is not inside a git work tree — publish publishes a commit, and there is \
                 none.",
                project.display()
            ),
            Self::Dirty { paths } => write!(
                formatter,
                "the project directory has changes that are not committed and would ship in \
                 the archive: {}",
                paths.join(", ")
            ),
            Self::NoUpstream { branch } => write!(
                formatter,
                "branch {branch} has no upstream — push it first: git push -u origin {branch}."
            ),
            Self::Ahead { branch, ahead } => write!(
                formatter,
                "branch {branch} is ahead of its upstream by {ahead} commit(s) that are not on \
                 the forge yet — push first: git push."
            ),
            Self::NoRemote { remote } => write!(
                formatter,
                "git remote get-url {remote} failed — the branch's upstream names a remote \
                 that no longer has a URL configured."
            ),
            Self::UnrecognisedRemote { remote, url } => write!(
                formatter,
                "remote {remote} is {url}, which isn't a github.com URL this hub recognises — \
                 pass --repo owner/repo."
            ),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotInstalled { source } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
