//! Running an app's own commands with the interpreter the hub bundles.
//!
//! One promise holds this module together: **no host PHP is ever involved.**
//! The hub ships FrankenPHP and a `composer.phar`, and every line of an app's
//! PHP — Composer's own resolution, the scripts Composer fires, the app's
//! `bin/console` lifecycle commands — runs under that one interpreter. It is
//! what makes Composer's platform introspection automatically right (the
//! dependency tree is resolved by the very PHP that will serve the app, so
//! there is no `requires-core` field to invent) and it is what lets the hub run
//! on a machine with no PHP installed at all, which is the machine most users
//! have.
//!
//! Keeping that promise takes one piece of plumbing that is easy to miss, and
//! was found by measuring rather than by reading (2026-08-07, on the pinned
//! sidecar):
//!
//! ```text
//! frankenphp php-cli -r 'echo PHP_BINARY;'   ->   (empty)
//! ```
//!
//! `PHP_BINARY` is the constant every PHP tool uses to re-invoke its own
//! interpreter for a child process — Composer's `@php`, Symfony Flex's
//! auto-scripts. Under `frankenphp php-cli` it is empty, so
//! `PhpExecutableFinder` walks on to the next candidate and finds the *host's*
//! `php` — measured here as `/usr/bin/php8.4` serving an app the hub had just
//! resolved against PHP 8.5.8. Silently, with no error to read. On a machine
//! with no PHP at all it does not even fail quietly: `composer install` simply
//! cannot run its own scripts.
//!
//! [`Toolchain::shim`] closes it. `<hub root>/bin/php` is a two-line shell
//! script re-entering `frankenphp php-cli`, exported as `PHP_BINARY` and
//! prepended to `PATH`, so anything looking for "the PHP that is running me"
//! finds the bundled one. It also drops the `-d ini=value` arguments Composer
//! appends, because `frankenphp php-cli` takes a script or `-r` and no PHP CLI
//! options at all — measured the same day, and the reason a plain
//! `exec frankenphp php-cli "$@"` shim fails on the very first script.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use tfsapp_core::sidecar::{command_with_env, path_to_string};

use crate::{
    paths::{Paths, PathsError},
    platform::{self, PlatformError},
};

/// `<hub>/resources/composer.phar` — what `make composer` downloads, beside
/// the FrankenPHP `make sidecar` fetches. Same resolution rule and same
/// packaged-mode caveat as [`platform::bundled_frankenphp`].
pub fn bundled_composer() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/composer.phar")
}

/// Everything the hub needs to run one app's PHP.
pub struct Toolchain {
    /// The bundled (or system-wide fallback) FrankenPHP.
    pub frankenphp: PathBuf,
    /// The bundled `composer.phar`.
    pub composer: PathBuf,
    /// `<hub root>/bin/php` — see the module header.
    pub shim: PathBuf,
}

/// Resolve the interpreter and Composer, and (re)write the shim.
///
/// The shim is rewritten on every call rather than created once: it hardcodes
/// the interpreter's path, and a hub self-update moves that path underneath it.
/// A stale shim would point at a FrankenPHP that no longer exists, which is the
/// same failure as having no shim at all, minus the chance of noticing.
pub fn toolchain(paths: &Paths) -> Result<Toolchain, PhpError> {
    let frankenphp = platform::hub_frankenphp()?;

    let composer = bundled_composer();
    if !composer.is_file() {
        return Err(PhpError::NoComposer { path: composer });
    }

    let shim = paths.php_shim_path();
    write_shim(&shim, &frankenphp)?;

    Ok(Toolchain {
        frankenphp,
        composer,
        shim,
    })
}

/// Write the `php` shim at `path`, pointing at `frankenphp`.
fn write_shim(path: &Path, frankenphp: &Path) -> Result<(), PhpError> {
    use std::os::unix::fs::PermissionsExt;

    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PhpError::Io { path, source }
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }

    // Single-quoted, with any embedded quote closed and reopened the way sh
    // requires: the interpreter's path runs through the user's home directory
    // and is not the hub's to assume anything about.
    let quoted = format!(
        "'{}'",
        frankenphp.display().to_string().replace('\'', "'\\''")
    );
    let script = format!(
        "#!/bin/sh\n\
         # Written by tfsapp-hub, rewritten on every use — edits are lost.\n\
         # Stands in for `php` so that anything an app's Composer or console\n\
         # spawns lands on the interpreter the hub bundles, never on a host PHP.\n\
         # The loop drops the `-d ini=value` options Composer appends: FrankenPHP's\n\
         # php-cli takes a script or -r and no PHP CLI options, and would read the\n\
         # first flag as a filename.\n\
         while [ $# -gt 0 ]; do\n\
         \x20 case \"$1\" in\n\
         \x20   -d) shift 2 ;;\n\
         \x20   -d*|-n|-q) shift ;;\n\
         \x20   *) break ;;\n\
         \x20 esac\n\
         done\n\
         exec {quoted} php-cli \"$@\"\n"
    );

    // Written beside the target and renamed over it, not written in place. Two
    // installs can genuinely overlap, and an in-place rewrite gives the other
    // one a window in which the file it is about to execute is open for writing
    // — which the kernel answers with ETXTBSY, an error whose text ("Text file
    // busy") explains nothing to whoever meets it. A rename swaps whole inodes,
    // so a concurrent execution keeps running the generation it started with.
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, script).map_err(io_error(&temporary))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))
        .map_err(io_error(&temporary))?;
    fs::rename(&temporary, path).map_err(io_error(path))?;
    Ok(())
}

impl Toolchain {
    /// `composer install --no-dev` against `app_dir`.
    ///
    /// `--no-dev` because an installed app is a deployment, not a checkout:
    /// PHPUnit and friends have no business on a user's machine. Composer's
    /// scripts are **not** disabled — a Symfony app's Flex auto-scripts are how
    /// its assets get installed and its container compiled, and turning them off
    /// would leave a tree that composed but does not run. That is also the
    /// honest reading of what an install is: running third-party PHP, exactly
    /// like `composer require` does on the developer's own machine.
    pub fn composer_install(
        &self,
        app_dir: &Path,
        envs: &[(&str, String)],
    ) -> Result<(), PhpError> {
        let mut command = self.php(envs);
        command
            .arg(&self.composer)
            .args([
                "install",
                "--no-dev",
                "--no-interaction",
                "--optimize-autoloader",
            ])
            .current_dir(app_dir);
        self.wait(command, "composer install --no-dev")
    }

    /// One `bin/console` invocation, from a manifest `commands` entry.
    ///
    /// Split on whitespace and passed as argv, with no shell in between
    /// (CONTRACT.md §2): a manifest cannot become a shell-injection vector, and
    /// only `bin/console` commands can be declared.
    pub fn console(
        &self,
        app_dir: &Path,
        envs: &[(&str, String)],
        arguments: &str,
    ) -> Result<(), PhpError> {
        let arguments: Vec<&str> = arguments.split_whitespace().collect();
        if arguments.is_empty() {
            return Ok(());
        }

        let mut command = self.php(envs);
        command
            .arg(app_dir.join("bin/console"))
            .args(&arguments)
            .current_dir(app_dir);
        self.wait(command, &format!("bin/console {}", arguments.join(" ")))
    }

    /// One `bin/console` invocation whose output goes to a **log file** rather
    /// than to the terminal.
    ///
    /// The same command as [`Toolchain::console`], for the moment that has no
    /// terminal to write to: a launch. An app opened from a desktop entry has
    /// nowhere for stdout to go, so a lifecycle command failing there would
    /// otherwise leave nothing behind but an exit code. Each run is headed with
    /// `=== [event] command ===` in `commands.log`, appended never truncated, so
    /// several launches stay comparable in one file.
    ///
    /// `event` is the same word the station uses in its own header lines
    /// (`pre-install`, `messenger-setup`, …) — the file is read by people who
    /// know that vocabulary, and inventing a second one here would only make the
    /// two hosts' logs harder to compare.
    pub fn console_logged(
        &self,
        app_dir: &Path,
        envs: &[(&str, String)],
        arguments: &str,
        log_file: &Path,
        event: &str,
    ) -> Result<(), PhpError> {
        let arguments: Vec<&str> = arguments.split_whitespace().collect();
        if arguments.is_empty() {
            return Ok(());
        }
        let label = format!("bin/console {}", arguments.join(" "));

        tfsapp_core::log::append_log(log_file, &format!("=== [{event}] {label} ==="));

        let mut command = self.php(envs);
        command
            .arg(app_dir.join("bin/console"))
            .args(&arguments)
            .current_dir(app_dir);

        let output = command.output().map_err(|source| {
            tfsapp_core::log::append_log(log_file, &format!("spawn error: {source}"));
            PhpError::Unstartable {
                label: label.clone(),
                source,
            }
        })?;
        tfsapp_core::log::append_log(log_file, &String::from_utf8_lossy(&output.stdout));
        tfsapp_core::log::append_log(log_file, &String::from_utf8_lossy(&output.stderr));

        if !output.status.success() {
            return Err(PhpError::FailedLogged {
                label,
                detail: match output.status.code() {
                    Some(code) => format!("exited with status {code}"),
                    None => "was killed by a signal".to_string(),
                },
                log_file: log_file.to_path_buf(),
            });
        }
        Ok(())
    }

    /// The two variables that point anything looking for PHP at the shim.
    ///
    /// Both, because they are read by different code: `PHP_BINARY` is what
    /// `PhpExecutableFinder` consults first, and `PATH` is what a script that
    /// plainly spells `php` uses. `PATH` is prepended to, never replaced —
    /// Composer legitimately reaches for `git`, `unzip` and friends.
    ///
    /// Exposed rather than kept inside [`Toolchain::php`] because the sidecar
    /// and the Messenger worker need them just as much: both are PHP processes
    /// that can shell out to PHP, and both would otherwise land on the host's
    /// interpreter — or, on the machine with no PHP at all that the hub exists
    /// to serve, on nothing. Measured, not assumed: see ARCHITECTURE.md's
    /// "The `PHP_BINARY` shim".
    pub fn shim_env(&self) -> Vec<(&'static str, String)> {
        let mut variables = vec![("PHP_BINARY", path_to_string(&self.shim))];
        if let Some(directory) = self.shim.parent() {
            let path = match std::env::var_os("PATH") {
                Some(existing) => {
                    let mut entries = vec![directory.to_path_buf()];
                    entries.extend(std::env::split_paths(&existing));
                    std::env::join_paths(entries).unwrap_or(existing)
                }
                None => directory.as_os_str().to_os_string(),
            };
            variables.push(("PATH", path.to_string_lossy().into_owned()));
        }
        variables
    }

    /// A `frankenphp php-cli` command carrying `envs` plus the shim.
    fn php(&self, envs: &[(&str, String)]) -> Command {
        let mut command = command_with_env(&self.frankenphp, envs);
        command.arg("php-cli");
        for (key, value) in self.shim_env() {
            command.env(key, value);
        }
        command
    }

    /// Run `command` to completion, with its output going straight to the
    /// terminal.
    ///
    /// Inherited rather than captured, deliberately: a five-minute `composer
    /// install` with nothing on screen reads as a hang, and Composer's platform
    /// error *is* the compatibility message a user needs to see — quoting it
    /// back through a wrapper could only make it worse.
    fn wait(&self, mut command: Command, label: &str) -> Result<(), PhpError> {
        let status = command.status().map_err(|source| PhpError::Unstartable {
            label: label.to_string(),
            source,
        })?;

        if !status.success() {
            return Err(PhpError::Failed {
                label: label.to_string(),
                detail: match status.code() {
                    Some(code) => format!("exited with status {code}"),
                    None => "was killed by a signal".to_string(),
                },
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum PhpError {
    Platform(PlatformError),
    Paths(PathsError),
    NoComposer {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Unstartable {
        label: String,
        source: io::Error,
    },
    Failed {
        label: String,
        detail: String,
    },
    /// A command whose output went to a log file rather than the terminal — so
    /// unlike [`PhpError::Failed`], the message has to say where to read it.
    FailedLogged {
        label: String,
        detail: String,
        log_file: PathBuf,
    },
}

impl fmt::Display for PhpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => write!(formatter, "{error}"),
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::NoComposer { path } => write!(
                formatter,
                "Composer not found at {}. Run `make composer` — the hub installs \
                 every app's dependencies with its own PHP and its own Composer, so \
                 nothing installs without it.",
                path.display()
            ),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Unstartable { label, source } => {
                write!(formatter, "cannot start `{label}`: {source}")
            }
            // The command's own output has already reached the terminal, so
            // this line says which one stopped and how, and does not try to
            // repeat what it said.
            Self::Failed { label, detail } => write!(formatter, "`{label}` {detail}"),
            Self::FailedLogged {
                label,
                detail,
                log_file,
            } => write!(
                formatter,
                "`{label}` {detail}. See {} for its full output.",
                log_file.display()
            ),
        }
    }
}

impl std::error::Error for PhpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Platform(error) => Some(error),
            Self::Paths(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Unstartable { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<PlatformError> for PhpError {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl From<PathsError> for PhpError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

#[cfg(test)]
#[path = "php_tests.rs"]
mod tests;
