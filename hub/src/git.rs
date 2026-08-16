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
    fmt,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
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

#[allow(dead_code)] // The pinned-object seam is consumed by publish step 3.
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

    /// The project path as Git sees it from the repository root.  Keeping
    /// this byte-oriented is important: a valid Git path need not be UTF-8.
    fn project_prefix(&self, project: &Path) -> Result<PathBuf, GitError> {
        let output = self.run(project, &["rev-parse", "--show-prefix"])?;
        if !output.status.success() {
            return Err(GitError::NotAWorkTree {
                project: project.to_path_buf(),
            });
        }
        let prefix = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
        Ok(PathBuf::from(OsString::from_vec(prefix.to_vec())))
    }

    /// Refuse unless `sha` is already reachable from the configured upstream.
    /// `status`'s ahead count is a useful author-facing diagnosis, but this
    /// object-level check is the proof the later object reads rely on.
    fn upstream_contains(&self, project: &Path, sha: &str, upstream: &str) -> Result<(), GitError> {
        let output = self.run(project, &["merge-base", "--is-ancestor", sha, upstream])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(GitError::NotPushed {
                sha: sha.to_string(),
                upstream: upstream.to_string(),
            })
        }
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

    /// Pin the one pushed commit from which a publication must read every
    /// later input.  The returned prefix makes a project nested in a larger
    /// repository unambiguous when its tree is enumerated.
    pub fn snapshot(
        &self,
        project: &Path,
        explicit_repo: Option<&str>,
    ) -> Result<Snapshot, GitError> {
        let commit = self.ensure_pushed(project, explicit_repo)?;
        let upstream = self
            .status(project)?
            .upstream
            .ok_or_else(|| GitError::NoUpstream {
                branch: commit.branch.clone(),
            })?;
        self.upstream_contains(project, &commit.sha, &upstream)?;
        Ok(Snapshot {
            commit,
            project_prefix: self.project_prefix(project)?,
        })
    }

    /// Enumerate the pinned commit's project-relative tree, never the index
    /// or files currently on disk.
    pub fn tree_entries(
        &self,
        project: &Path,
        snapshot: &Snapshot,
    ) -> Result<Vec<TreeEntry>, GitError> {
        let mut command = Command::new(&self.program);
        command
            .arg("-C")
            .arg(project)
            .args(["ls-tree", "-r", "-z", &snapshot.commit.sha, "--"]);
        if !snapshot.project_prefix.as_os_str().is_empty() {
            command.arg(&snapshot.project_prefix);
        }
        let output =
            spawn_with_retry(&mut command).map_err(|source| GitError::NotInstalled { source })?;
        if !output.status.success() {
            return Err(GitError::MissingObject {
                object: snapshot.commit.sha.clone(),
            });
        }
        parse_ls_tree(&output.stdout, &snapshot.project_prefix)
    }

    /// Start the one bounded `git cat-file --batch` child used to stream
    /// pinned blobs.  Call [`BlobReader::copy_blob`] once per selected entry.
    pub fn blob_reader(&self, project: &Path) -> Result<BlobReader, GitError> {
        let mut command = Command::new(&self.program);
        command
            .arg("-C")
            .arg(project)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|source| GitError::NotInstalled { source })?;
        let stdin = child.stdin.take().ok_or(GitError::MalformedBatch {
            detail: "git cat-file did not expose stdin".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or(GitError::MalformedBatch {
            detail: "git cat-file did not expose stdout".to_string(),
        })?;
        Ok(BlobReader {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
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

/// The immutable commit and project root selected by [`Git::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Snapshot {
    pub commit: Commit,
    pub project_prefix: PathBuf,
}

/// One permitted object from a pinned Git tree.  `mode` is retained verbatim
/// for the archive writer: Git's executable bit and symlink representation
/// are metadata of the commit, not of a later worktree file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub mode: u32,
    pub object_id: String,
}

#[allow(dead_code)]
impl TreeEntry {
    pub fn is_executable(&self) -> bool {
        self.mode == 0o100755
    }

    pub fn is_symlink(&self) -> bool {
        self.mode == 0o120000
    }
}

/// One live `git cat-file --batch` child.  It requests and copies one blob at
/// a time, so publication never buffers the source archive in memory.
#[allow(dead_code)]
pub struct BlobReader {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[allow(dead_code)]
impl BlobReader {
    pub fn copy_blob(
        &mut self,
        object: &str,
        destination: &mut (impl Write + ?Sized),
    ) -> Result<u64, GitError> {
        self.stdin
            .write_all(object.as_bytes())
            .and_then(|()| self.stdin.write_all(b"\n"))
            .and_then(|()| self.stdin.flush())
            .map_err(|source| GitError::BatchIo {
                object: object.to_string(),
                source,
            })?;

        let mut header = Vec::new();
        self.stdout
            .read_until(b'\n', &mut header)
            .map_err(|source| GitError::BatchIo {
                object: object.to_string(),
                source,
            })?;
        let header = header.strip_suffix(b"\n").unwrap_or(&header);
        if header.is_empty() {
            return Err(GitError::MalformedBatch {
                detail: format!("no response header for {object}"),
            });
        }
        if header.ends_with(b" missing") {
            return Err(GitError::MissingObject {
                object: object.to_string(),
            });
        }
        let mut fields = header.split(|byte| *byte == b' ');
        let returned = fields.next();
        let kind = fields.next();
        let size = fields.next();
        if returned != Some(object.as_bytes())
            || kind != Some(b"blob".as_slice())
            || size.is_none()
            || fields.next().is_some()
        {
            return Err(GitError::MalformedBatch {
                detail: format!("invalid response header for {object}"),
            });
        }
        let size = std::str::from_utf8(size.unwrap())
            .ok()
            .and_then(|size| size.parse::<u64>().ok())
            .ok_or_else(|| GitError::MalformedBatch {
                detail: format!("invalid blob length for {object}"),
            })?;
        let mut limited = (&mut self.stdout).take(size);
        let copied = io::copy(&mut limited, destination).map_err(|source| GitError::BatchIo {
            object: object.to_string(),
            source,
        })?;
        if copied != size {
            return Err(GitError::MalformedBatch {
                detail: format!("short blob response for {object}: expected {size}, got {copied}"),
            });
        }
        let mut terminator = [0];
        self.stdout
            .read_exact(&mut terminator)
            .map_err(|source| GitError::BatchIo {
                object: object.to_string(),
                source,
            })?;
        if terminator != [b'\n'] {
            return Err(GitError::MalformedBatch {
                detail: format!("blob response for {object} has no trailing newline"),
            });
        }
        Ok(size)
    }
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

/// Parse `git ls-tree -r -z` records (`<mode> <type> <object>\t<path>\0`)
/// and trim the Git-supplied project prefix without turning paths into text.
#[allow(dead_code)]
fn parse_ls_tree(stdout: &[u8], prefix: &Path) -> Result<Vec<TreeEntry>, GitError> {
    let prefix = prefix.as_os_str().as_bytes();
    stdout
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| GitError::MalformedTree {
                    detail: "ls-tree record has no tab before its path".to_string(),
                })?;
            let (header, path) = record.split_at(tab);
            let path = &path[1..];
            let mut fields = header.split(|byte| *byte == b' ');
            let mode = fields.next();
            let kind = fields.next();
            let object = fields.next();
            if mode.is_none() || kind.is_none() || object.is_none() || fields.next().is_some() {
                return Err(GitError::MalformedTree {
                    detail: "ls-tree record has an invalid header".to_string(),
                });
            }
            let mode_bytes = mode.unwrap();
            let mode = std::str::from_utf8(mode_bytes)
                .ok()
                .and_then(|mode| u32::from_str_radix(mode, 8).ok())
                .ok_or_else(|| GitError::MalformedTree {
                    detail: "ls-tree record has an invalid mode".to_string(),
                })?;
            let object = object.unwrap();
            if !(object.len() == 40 || object.len() == 64)
                || !object.iter().all(u8::is_ascii_hexdigit)
            {
                return Err(GitError::MalformedTree {
                    detail: "ls-tree record has an invalid object id".to_string(),
                });
            }
            let kind = kind.unwrap();
            if mode == 0o160000 || kind == b"commit" {
                return Err(GitError::UnsupportedTreeEntry {
                    path: PathBuf::from(OsString::from_vec(path.to_vec())),
                    mode,
                    kind: String::from_utf8_lossy(kind).into_owned(),
                });
            }
            if kind != b"blob" || !matches!(mode, 0o100644 | 0o100755 | 0o120000) {
                return Err(GitError::UnsupportedTreeEntry {
                    path: PathBuf::from(OsString::from_vec(path.to_vec())),
                    mode,
                    kind: String::from_utf8_lossy(kind).into_owned(),
                });
            }
            let path = path
                .strip_prefix(prefix)
                .ok_or_else(|| GitError::MalformedTree {
                    detail: "ls-tree path lies outside the requested project prefix".to_string(),
                })?;
            let path = path.strip_prefix(b"/").unwrap_or(path);
            if path.is_empty() {
                return Err(GitError::MalformedTree {
                    detail: "ls-tree record names the project root, not a file".to_string(),
                });
            }
            Ok(TreeEntry {
                path: PathBuf::from(OsString::from_vec(path.to_vec())),
                mode,
                object_id: String::from_utf8_lossy(object).into_owned(),
            })
        })
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
#[allow(dead_code)]
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
    /// The named SHA is not in the configured upstream's history.
    NotPushed { sha: String, upstream: String },
    /// `git remote get-url <remote>` itself failed.
    NoRemote { remote: String },
    /// The remote's URL is not a `github.com` spelling this hub recognises.
    UnrecognisedRemote { remote: String, url: String },
    /// The pinned commit or one of its blobs cannot be read from Git.
    MissingObject { object: String },
    /// `ls-tree` did not produce the documented NUL-delimited record shape.
    MalformedTree { detail: String },
    /// A tree entry cannot be represented in the V0.1 release format.
    UnsupportedTreeEntry {
        path: PathBuf,
        mode: u32,
        kind: String,
    },
    /// `cat-file --batch` returned an invalid protocol response.
    MalformedBatch { detail: String },
    /// An I/O failure while speaking to the bounded `cat-file` child.
    BatchIo { object: String, source: io::Error },
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
            Self::NotPushed { sha, upstream } => write!(
                formatter,
                "commit {sha} is not reachable from upstream {upstream} — push it before publishing."
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
            Self::MissingObject { object } => write!(
                formatter,
                "Git cannot read pinned object {object}; publish refuses rather than reading the working tree."
            ),
            Self::MalformedTree { detail } => write!(
                formatter,
                "Git returned a malformed pinned tree: {detail}."
            ),
            Self::UnsupportedTreeEntry { path, mode, kind } => write!(
                formatter,
                "pinned tree entry {} ({mode:o} {kind}) is unsupported; submodules cannot be published.",
                path.display()
            ),
            Self::MalformedBatch { detail } => write!(
                formatter,
                "Git returned a malformed blob response: {detail}."
            ),
            Self::BatchIo { object, source } => write!(
                formatter,
                "could not stream pinned Git object {object}: {source}."
            ),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotInstalled { source } => Some(source),
            Self::BatchIo { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
