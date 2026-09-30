//! `tfsapp-hub --update` — the hub replacing itself with its own latest
//! release (`../plan/020-hub-self-update-and-revalidation.md`).
//!
//! This module starts with the one thing worth getting right before any I/O
//! exists around it: [`check`], the pure comparison between the hub's own
//! running version and an already-fetched release. Reusing the station's
//! `update.rs` `evaluate_release` shape (`v<version>` tag, `>` for "newer",
//! never a downgrade) rather than a second parser — ported here because the
//! hub has no manifest to read its own next version from the way an app
//! does; the tag is the only place it lives.
//!
//! `run` extends plan 020's ten-step order with a probe after verification:
//! `$APPIMAGE` guard, check, confirm, download, verify, probe, refresh the
//! stable copy, snapshot the registry, swap the binary, swap `$APPIMAGE`, report.
//! Before refreshing the stable copy, failures leave installed files untouched.
//! From the snapshot on, the
//! anchor is what makes the change reversible (`--rollback`,
//! `hub_rollback.rs`, this plan's step 7).

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::{
    cli::{EXIT_FAILED, EXIT_OK},
    hub_bin::{self, HubBinError},
    paths::Paths,
    prompt,
    registry::{self, RegistryError},
    release::{self, GitHubRelease, ReleaseError},
};

/// [`check`]'s outcome — pure over an already-fetched release, so it needs no
/// network of its own; `run` (step 4) is what calls
/// [`release::fetch_latest_release_at`] before handing the result here.
#[derive(Debug, PartialEq)]
pub enum HubUpdateCheck<'a> {
    /// The forge's latest release is newer than the running hub, and carries
    /// everything `--update` needs to fetch it.
    Available {
        version: semver::Version,
        asset_name: &'a str,
        asset_url: &'a str,
        checksums_url: &'a str,
    },
    /// The forge's latest release is this version or older. Never treated as
    /// a downgrade to refuse — `--update` follows `latest` and there is no
    /// `--ref` on it (the plan's Out of scope), so "older" only ever means
    /// "nothing to do".
    UpToDate,
    /// The release exists but could not be turned into something `--update`
    /// can act on: an unparseable tag, or a release missing one of its two
    /// required assets. The message is [`release::ReleaseError`]'s own, or
    /// names the tag directly — either way, something a user can read
    /// without a second translation.
    Unavailable(String),
}

/// Turn a release tag into a comparable version: strip a leading `v`, then
/// parse as semver. The one rule for that anywhere in the hub —
/// `update_check`'s per-app answer (`../plan/021-the-update-check-an-app-can-
/// read.md`) reuses this rather than growing a second one.
pub(crate) fn parse_tag_version(tag: &str) -> Result<semver::Version, semver::Error> {
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag))
}

/// Run an AppImage far enough to exercise its loader and all of its direct
/// dependencies. The expected name comes from the running Tauri package info,
/// which is also the source of our own `--version` output.
fn probe(
    path: &Path,
    expected_name: &str,
    timeout: Duration,
) -> Result<semver::Version, ProbeError> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ProbeError::DoesNotRun(error.to_string()))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeError::DoesNotRun(error.to_string()));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| ProbeError::DoesNotRun(error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().take(3).collect::<Vec<_>>().join("; ");
        return Err(ProbeError::DoesNotRun(if detail.is_empty() {
            output.status.to_string()
        } else {
            detail
        }));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    let Some(version) = line
        .strip_prefix(expected_name)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return Err(ProbeError::NotTheHub(line.to_string()));
    };
    semver::Version::parse(version).map_err(|_| ProbeError::NotTheHub(line.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
enum ProbeError {
    DoesNotRun(String),
    TimedOut,
    NotTheHub(String),
}

/// Compare `current` — the running hub's own version — against `release`,
/// already fetched by the caller. No network, no filesystem.
pub fn check<'a>(current: &semver::Version, release: &'a GitHubRelease) -> HubUpdateCheck<'a> {
    let latest = match parse_tag_version(&release.tag_name) {
        Ok(version) => version,
        Err(error) => {
            return HubUpdateCheck::Unavailable(format!(
                "release {} has a tag that is not a version ({error})",
                release.tag_name
            ))
        }
    };

    if latest <= *current {
        return HubUpdateCheck::UpToDate;
    }

    match release::resolve_appimage_assets(release) {
        Ok(assets) => HubUpdateCheck::Available {
            version: latest,
            asset_name: assets.archive_name,
            asset_url: assets.archive_url,
            checksums_url: assets.checksums_url,
        },
        Err(error) => HubUpdateCheck::Unavailable(error.to_string()),
    }
}

/// `$APPIMAGE`'s value, refused when unset or empty — the Overview's order,
/// step 1: "`$APPIMAGE` is set — otherwise refuse, nothing contacted." Pure
/// over an already-read environment variable rather than reading it itself,
/// so the unset/empty/set cases are each one line to test. `None` means this
/// is not a packaged hub — a `cargo run`, a plain binary — and there is no
/// image on disk for `--update` to replace.
pub(crate) fn resolve_appimage_target(appimage_env: Option<&str>) -> Option<PathBuf> {
    match appimage_env {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

/// Whether `a` and `b` are the same file — `$APPIMAGE` against the stable
/// copy (the Overview's "Two files, not one": when they coincide there is
/// one swap, not two). Reuses `hub_bin::same_file`'s canonicalize comparison
/// rather than a second one.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    hub_bin::same_file(a, b)
}

/// Which half of the rollback anchor is missing — [`anchor_state`]'s
/// refusal, named so the message can say which one rather than a bare "no
/// anchor": half-present is exactly as unusable as absent, but it is a
/// different thing to have gone wrong (an interrupted `--update`, never a
/// network problem).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingAnchorHalf {
    Binary,
    Registry,
    Both,
}

impl fmt::Display for MissingAnchorHalf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Binary => "the previous hub binary",
            Self::Registry => "its registry snapshot",
            Self::Both => "the previous hub binary and its registry snapshot",
        };
        write!(formatter, "{text}")
    }
}

/// Whether the rollback anchor left by a previous `--update` is usable:
/// `binary_path` present and non-empty, `registry_snapshot_path` present
/// beside it. No version comparison against the running hub — correction 3
/// of the plan's Overview: the anchor is deleted by the `--rollback` that
/// consumes it, so its mere presence already answers "is there something to
/// roll back to".
pub(crate) fn anchor_state(
    binary_path: &Path,
    registry_snapshot_path: &Path,
) -> Result<(), MissingAnchorHalf> {
    let binary_present = fs::metadata(binary_path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false);
    let registry_present = registry_snapshot_path.is_file();

    match (binary_present, registry_present) {
        (true, true) => Ok(()),
        (false, true) => Err(MissingAnchorHalf::Binary),
        (true, false) => Err(MissingAnchorHalf::Registry),
        (false, false) => Err(MissingAnchorHalf::Both),
    }
}

/// What [`run`] did, for the exit code and the report.
#[derive(Debug)]
pub(crate) enum UpdateOutcome {
    /// Step 2: the forge's latest release is this version or older.
    UpToDate { current: semver::Version },
    /// Step 4: the user declined the confirmation.
    Declined,
    /// Steps 7–9 all landed.
    Updated {
        version: semver::Version,
        stable_path: PathBuf,
        appimage_path: PathBuf,
    },
}

/// `tfsapp-hub --update [--yes]` — resolve `Paths`, read `$APPIMAGE`, and turn
/// the pipeline's outcome into an exit code.
pub fn run(
    current: &semver::Version,
    expected_name: &str,
    from: Option<&Path>,
    assume_yes: bool,
) -> i32 {
    let paths = match Paths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            return EXIT_FAILED;
        }
    };
    let appimage_env = std::env::var("APPIMAGE").ok();

    match update(
        &paths,
        appimage_env.as_deref(),
        current,
        expected_name,
        from,
        assume_yes,
    ) {
        Ok(UpdateOutcome::UpToDate { current }) => {
            println!("tfsapp-hub {current} is already the latest release.");
            EXIT_OK
        }
        // Declining is not a failure of the command, but nothing changed
        // either — a script reading 0 would conclude it did.
        Ok(UpdateOutcome::Declined) => EXIT_FAILED,
        Ok(UpdateOutcome::Updated {
            version,
            stable_path,
            appimage_path,
        }) => {
            report_updated(&version, &stable_path, &appimage_path);
            EXIT_OK
        }
        Err(error) => {
            eprintln!("tfsapp-hub: {error}");
            EXIT_FAILED
        }
    }
}

/// [`run`]'s pipeline, minus resolving `Paths`/`$APPIMAGE` — what lets the
/// whole thing run against a throwaway root in a test. The scratch directory
/// is removed on the way out regardless of how the call ends, matching
/// `install::install` and `publish::publish`.
pub(crate) fn update(
    paths: &Paths,
    appimage_env: Option<&str>,
    current: &semver::Version,
    expected_name: &str,
    from: Option<&Path>,
    assume_yes: bool,
) -> Result<UpdateOutcome, HubUpdateError> {
    let scratch = paths.scratch_dir();
    let result = match from {
        Some(source) => update_from_at(
            paths,
            &scratch,
            appimage_env,
            current,
            expected_name,
            source,
            assume_yes,
        ),
        None => update_at(
            paths,
            &scratch,
            release::GITHUB_API_BASE,
            appimage_env,
            current,
            expected_name,
            assume_yes,
        ),
    };
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// [`update`]'s pipeline, against `base_url` instead of GitHub's real API —
/// the seam `hub_update_tests.rs` uses to run a whole `--update` against a
/// local stub, matching `install_into`/`update_into`'s own shape. Production
/// always calls this with [`release::GITHUB_API_BASE`], through [`update`]
/// above.
///
/// Plan 020's order with a probe after checksum verification: the refresh is
/// deferred until the candidate has run, so probe failure changes no installed
/// file. The registry snapshot lands before the binary swap, so a failure
/// mid-swap never anchors a registry the new hub had
/// already begun to rewrite; `$APPIMAGE` (9) is swapped last because a
/// launcher pointing at the stale `bin/tfsapp-hub` is the worse of the two
/// possible partial failures (the plan's "Two files, not one").
fn update_at(
    paths: &Paths,
    scratch: &Path,
    base_url: &str,
    appimage_env: Option<&str>,
    current: &semver::Version,
    expected_name: &str,
    assume_yes: bool,
) -> Result<UpdateOutcome, HubUpdateError> {
    update_at_after_anchor(
        paths,
        scratch,
        base_url,
        appimage_env,
        current,
        expected_name,
        assume_yes,
        |_| {},
    )
}

#[allow(clippy::too_many_arguments)] // The final callback is a test-only swap failure seam.
fn update_at_after_anchor(
    paths: &Paths,
    scratch: &Path,
    base_url: &str,
    appimage_env: Option<&str>,
    current: &semver::Version,
    expected_name: &str,
    assume_yes: bool,
    after_anchor: impl FnOnce(&Paths),
) -> Result<UpdateOutcome, HubUpdateError> {
    // 1. `$APPIMAGE` is set — otherwise refuse, nothing contacted.
    let appimage_target =
        resolve_appimage_target(appimage_env).ok_or(HubUpdateError::NotPackaged)?;

    // 2. Resolve latest from the releases repo; not newer → say so and exit 0.
    let release = release::fetch_latest_release_at(base_url, release::RELEASES_REPO)
        .map_err(HubUpdateError::Release)?;
    let (version, asset_name, asset_url, checksums_url) = match check(current, &release) {
        HubUpdateCheck::UpToDate => {
            return Ok(UpdateOutcome::UpToDate {
                current: current.clone(),
            })
        }
        HubUpdateCheck::Unavailable(reason) => return Err(HubUpdateError::CheckFailed(reason)),
        HubUpdateCheck::Available {
            version,
            asset_name,
            asset_url,
            checksums_url,
        } => (
            version,
            asset_name.to_string(),
            asset_url.to_string(),
            checksums_url.to_string(),
        ),
    };

    // 3. Confirm on the terminal (--yes to skip).
    announce(current, &version, &asset_name);
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(UpdateOutcome::Declined);
    }

    // 4. Download the .AppImage to the hub's scratch directory, and its
    // SHA256SUMS.txt.
    fs::create_dir_all(scratch).map_err(|source| HubUpdateError::Io {
        path: scratch.to_path_buf(),
        source,
    })?;
    let archive_path = scratch.join(&asset_name);
    println!("Downloading {asset_name} …");
    release::download_to(&asset_url, &archive_path).map_err(HubUpdateError::Release)?;

    // 5. Verify. Mismatch, missing entry, or unreadable sums → scratch
    // removed by the caller, nothing on disk touched, exit non-zero.
    let checksums_text = release::fetch_text(&checksums_url).map_err(HubUpdateError::Release)?;
    let checksums = release::parse_sha256sums(&checksums_text);
    let actual = release::sha256_file(&archive_path).map_err(HubUpdateError::Release)?;
    match release::verify(&checksums, &asset_name, &actual) {
        release::VerifyOutcome::Match => {}
        release::VerifyOutcome::Mismatch { expected, actual } => {
            return Err(HubUpdateError::ChecksumMismatch {
                asset_name,
                expected,
                actual,
            })
        }
        release::VerifyOutcome::MissingEntry => {
            return Err(HubUpdateError::ChecksumMissing { asset_name })
        }
    }

    // 6. Exercise the downloaded image before any installed file or anchor
    // is touched. Its checksum proves integrity; this proves it can start on
    // this machine and reports the version promised by the release tag.
    make_executable(&archive_path)?;
    let probed = probe(&archive_path, expected_name, Duration::from_secs(60));
    match probed {
        Ok(found) if found == version => {}
        Ok(found) => {
            return Err(HubUpdateError::ReleaseMismatch {
                expected: version,
                detail: format!("reported version {found}"),
            })
        }
        Err(ProbeError::NotTheHub(output)) => {
            return Err(HubUpdateError::ReleaseMismatch {
                expected: version,
                detail: format!("reported {output:?}"),
            })
        }
        Err(error) => {
            return Err(HubUpdateError::ReleaseDoesNotRun {
                version,
                detail: probe_failure_detail(error),
            })
        }
    }

    finish_update(
        paths,
        &appimage_target,
        &archive_path,
        version,
        after_anchor,
    )
}

fn update_from_at(
    paths: &Paths,
    scratch: &Path,
    appimage_env: Option<&str>,
    current: &semver::Version,
    expected_name: &str,
    source: &Path,
    assume_yes: bool,
) -> Result<UpdateOutcome, HubUpdateError> {
    let appimage_target =
        resolve_appimage_target(appimage_env).ok_or(HubUpdateError::NotPackaged)?;
    fs::create_dir_all(scratch).map_err(|source| HubUpdateError::Io {
        path: scratch.to_path_buf(),
        source,
    })?;
    let candidate = scratch.join("local.AppImage");
    fs::copy(source, &candidate).map_err(|error| HubUpdateError::Io {
        path: source.to_path_buf(),
        source: error,
    })?;
    make_executable(&candidate)?;
    let version = match probe(&candidate, expected_name, Duration::from_secs(60)) {
        Ok(version) => version,
        Err(ProbeError::NotTheHub(output)) => {
            return Err(HubUpdateError::LocalNotTheHub {
                path: source.to_path_buf(),
                output,
            });
        }
        Err(error) => {
            return Err(HubUpdateError::LocalDoesNotRun {
                path: source.to_path_buf(),
                detail: probe_failure_detail(error),
            });
        }
    };
    if version < *current {
        return Err(HubUpdateError::LocalOlder {
            current: current.clone(),
            found: version,
        });
    }
    announce(current, &version, &source.display().to_string());
    if !prompt::confirmed(assume_yes) {
        println!("Aborted — nothing was changed.");
        return Ok(UpdateOutcome::Declined);
    }
    finish_update(paths, &appimage_target, &candidate, version, |_| {})
}

fn finish_update(
    paths: &Paths,
    appimage_target: &Path,
    archive_path: &Path,
    version: semver::Version,
    after_anchor: impl FnOnce(&Paths),
) -> Result<UpdateOutcome, HubUpdateError> {
    // 7. Refresh the stable copy only now. A rejected release must not even
    // repair a stale stable copy: "nothing changed" includes that file.
    hub_bin::ensure_current_at(appimage_target, &paths.hub_executable_path())
        .map_err(HubUpdateError::HubBin)?;

    // 8. Snapshot registry.json beside the anchor, under the registry lock —
    // before the binary swap, per the Overview: a snapshot taken after
    // could already describe a state the new hub had begun to change.
    registry::snapshot_to(paths, &hub_bin::anchor_registry_path(paths))
        .map_err(HubUpdateError::Registry)?;

    // 9. Rename bin/tfsapp-hub → bin/tfsapp-hub.previous; move the verified
    // download into bin/tfsapp-hub, chmod +x.
    let stable_path = paths.hub_executable_path();
    fs::rename(&stable_path, hub_bin::anchor_path(paths)).map_err(|source| HubUpdateError::Io {
        path: stable_path.clone(),
        source,
    })?;
    after_anchor(paths);
    if let Err(source) = hub_bin::ensure_current_at(archive_path, &stable_path) {
        return match fs::rename(hub_bin::anchor_path(paths), &stable_path) {
            Ok(()) => {
                let _ = fs::remove_file(hub_bin::anchor_registry_path(paths));
                Err(HubUpdateError::StableSwapUndone { source })
            }
            Err(undo_source) => Err(HubUpdateError::StableSwapFailed {
                stable_path,
                source,
                undo_source,
            }),
        };
    }

    // 10. Swap $APPIMAGE, unless it is the same file — reusing
    // `ensure_current_at`'s own canonicalize comparison (the "same file"
    // question this time is stable-copy-vs-$APPIMAGE, not source-vs-target,
    // but the mechanism — copy, chmod, atomic rename beside the target — is
    // identical either way, which is exactly why it is reused rather than
    // reimplemented). From here a failure is the plan's named partial
    // failure: the stable copy is already new.
    hub_bin::ensure_current_at(&stable_path, appimage_target).map_err(|error| {
        HubUpdateError::AppimageSwapFailed {
            appimage_path: appimage_target.to_path_buf(),
            source: error,
        }
    })?;

    // 11. Report the new version, the two paths written, that installed apps
    // will be revalidated on their next use, and that --rollback undoes it —
    // done by `run`, from this outcome.
    Ok(UpdateOutcome::Updated {
        version,
        stable_path,
        appimage_path: appimage_target.to_path_buf(),
    })
}

fn make_executable(path: &Path) -> Result<(), HubUpdateError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .map_err(|source| HubUpdateError::Io {
            path: path.into(),
            source,
        })?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions).map_err(|source| HubUpdateError::Io {
        path: path.into(),
        source,
    })
}

fn probe_failure_detail(error: ProbeError) -> String {
    match error {
        ProbeError::DoesNotRun(detail) => detail,
        ProbeError::TimedOut => "timed out after 60 seconds".into(),
        ProbeError::NotTheHub(output) => format!("reported {output:?}"),
    }
}

fn announce(current: &semver::Version, latest: &semver::Version, asset_name: &str) {
    println!("Update available: {current} → {latest}");
    println!("  asset     {asset_name}");
    println!();
    println!(
        "This replaces the hub itself — both the copy every generated launcher\n\
         points at and, if it differs, the file you downloaded. Installed apps\n\
         are not touched now: any whose platform moved under this update are\n\
         revalidated on their own next use."
    );
}

fn report_updated(version: &semver::Version, stable_path: &Path, appimage_path: &Path) {
    println!("Updated to {version}.");
    println!("  {}", stable_path.display());
    if !hub_bin::same_file(stable_path, appimage_path) {
        println!("  {}", appimage_path.display());
    }
    println!(
        "Installed apps whose platform moved under this update will be revalidated on \
         their next use. `tfsapp-hub --rollback` undoes this."
    );
}

/// Why [`update`] could not finish.
#[derive(Debug)]
pub enum HubUpdateError {
    /// `$APPIMAGE` is unset or empty: not a packaged hub, so there is no
    /// image on disk for `--update` to replace.
    NotPackaged,
    /// Fetching the latest release, downloading an asset, or fetching its
    /// checksums failed outright — offline, rate limited, not found, or a
    /// malformed response.
    Release(ReleaseError),
    /// The release exists but is not something `--update` can act on — an
    /// unparseable tag, or a release missing one of its two required assets.
    /// [`check`]'s own `Unavailable` reason, carried through unchanged.
    CheckFailed(String),
    ReleaseDoesNotRun {
        version: semver::Version,
        detail: String,
    },
    ReleaseMismatch {
        expected: semver::Version,
        detail: String,
    },
    LocalDoesNotRun {
        path: PathBuf,
        detail: String,
    },
    LocalNotTheHub {
        path: PathBuf,
        output: String,
    },
    LocalOlder {
        current: semver::Version,
        found: semver::Version,
    },
    HubBin(HubBinError),
    Registry(RegistryError),
    ChecksumMismatch {
        asset_name: String,
        expected: String,
        actual: String,
    },
    ChecksumMissing {
        asset_name: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// The stable copy was already replaced, but writing
    /// the new binary over `$APPIMAGE` failed — the Overview's named
    /// partial failure: every generated launcher already runs the new hub,
    /// only this one downloaded file is stale.
    AppimageSwapFailed {
        appimage_path: PathBuf,
        source: HubBinError,
    },
    StableSwapUndone {
        source: HubBinError,
    },
    StableSwapFailed {
        stable_path: PathBuf,
        source: HubBinError,
        undo_source: io::Error,
    },
}

impl fmt::Display for HubUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPackaged => write!(
                formatter,
                "--update is packaged-only ($APPIMAGE is unset) — a `cargo run` or a plain \
                 binary has no installed image to replace."
            ),
            Self::Release(error) => write!(formatter, "{error}"),
            Self::CheckFailed(reason) => write!(formatter, "{reason}"),
            Self::ReleaseDoesNotRun { version, detail } => write!(
                formatter,
                "the official AppImage v{version} does not run on this machine ({detail}); nothing was changed. Rebuild it (README: It does not start), then run `tfsapp-hub --update --from <rebuilt AppImage>`."
            ),
            Self::ReleaseMismatch { expected, detail } => write!(
                formatter,
                "the official AppImage for v{expected} does not match its release tag ({detail}); nothing was changed."
            ),
            Self::LocalDoesNotRun { path, detail } => write!(
                formatter,
                "{} does not run on this machine ({detail}); nothing was changed.",
                path.display()
            ),
            Self::LocalNotTheHub { path, output } => write!(
                formatter,
                "{} is not this hub (reported {output:?}); nothing was changed.",
                path.display()
            ),
            Self::LocalOlder { current, found } => write!(
                formatter,
                "local AppImage v{found} is older than the running hub v{current}; use `tfsapp-hub --rollback` to go back. Nothing was changed."
            ),
            Self::HubBin(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
            Self::ChecksumMismatch {
                asset_name,
                expected,
                actual,
            } => write!(
                formatter,
                "checksum mismatch for {asset_name} (expected {expected}, got {actual}) — \
                 nothing was changed."
            ),
            Self::ChecksumMissing { asset_name } => write!(
                formatter,
                "{asset_name} has no entry in the release's SHA256SUMS.txt — nothing was \
                 changed."
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::AppimageSwapFailed {
                appimage_path,
                source,
            } => write!(
                formatter,
                "the hub was updated — every app launcher already runs the new version — but \
                 {} could not be replaced: {source}. Run `tfsapp-hub --update` again, or \
                 replace that file by hand with the release's .AppImage.",
                appimage_path.display()
            ),
            Self::StableSwapUndone { source } => write!(
                formatter,
                "the downloaded hub could not be installed: {source}. Nothing was changed."
            ),
            Self::StableSwapFailed {
                stable_path,
                source,
                undo_source,
            } => write!(
                formatter,
                "{} is missing: the downloaded hub could not be installed ({source}) and the previous \
                 hub could not be put back ({undo_source}). Every generated launcher is down until \
                 repaired; the complete rollback anchor remains and `tfsapp-hub --rollback` repairs it.",
                stable_path.display()
            ),
        }
    }
}

impl std::error::Error for HubUpdateError {}

#[cfg(test)]
#[path = "hub_update_tests.rs"]
mod tests;
