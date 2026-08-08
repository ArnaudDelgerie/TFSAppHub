//! Rendering, writing and removing one app's `.desktop` entry.
//!
//! The whole file format lives here so nothing about it leaks into `install`
//! or `remove` — both call [`write`] and [`remove`] and know nothing about
//! keys, escaping or quoting.
//!
//! **The filename is `<identifier>.desktop`**, resolved by
//! [`crate::paths::Paths::desktop_entry_path`]: it is the file GNOME matches
//! first against `_GTK_APPLICATION_ID`
//! (`.project/plan/003-runtime-identity.md` step 5), and a `tfsapp-`-prefixed
//! name would miss that key entirely and leave matching on the
//! `StartupWMClass` fallback alone.
//!
//! `Name=` and `Icon=` come from the same `Identity` the window itself is
//! built from (`Manifest::identity`), so the two surfaces cannot drift the way
//! the runtime-identity spike watched them drift on the station.
//!
//! **`X-TFSApp-Id` is the marker, and it is what makes removal safe.** Without
//! it, removal would delete a path built from a manifest's `identifier` — a
//! value the app chose. With it, the hub only ever deletes a file it can prove
//! it wrote, for the app being removed; a hand-written or foreign file at the
//! same path is left alone and said so.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    identity::Identity,
    paths::{Paths, PathsError},
};

/// One line of the entry, in the order written — sourced from CONTRACT.md §2's
/// identity table and from nowhere else.
pub fn render(id: &str, identity: &Identity, hub_executable: &Path) -> String {
    let mut entry = String::new();
    entry.push_str("[Desktop Entry]\n");
    entry.push_str("Type=Application\n");
    entry.push_str("Version=1.0\n");
    entry.push_str(&format!("Name={}\n", escape_value(&identity.product_name)));
    entry.push_str(&format!(
        "Exec={} open {}\n",
        exec_argument(hub_executable),
        id
    ));
    if let Some(icon) = &identity.icon_path {
        entry.push_str(&format!(
            "Icon={}\n",
            escape_value(&icon.display().to_string())
        ));
    }
    entry.push_str("Terminal=false\n");
    // `open <id>` returns as soon as it has re-executed the `__open` child,
    // and that child inherits `DESKTOP_STARTUP_ID` / the xdg-activation token
    // through its environment — so it is the process that would consume it,
    // which is the argument for shipping this. Decided by measurement on the
    // user's GNOME session (plan file, step 6) rather than by this argument
    // alone: dropped instead if the launch spinner hangs to its timeout.
    entry.push_str("StartupNotify=true\n");
    entry.push_str(&format!(
        "StartupWMClass={}\n",
        escape_value(&identity.identifier)
    ));
    entry.push_str(&format!("X-TFSApp-Id={}\n", escape_value(id)));
    entry
}

/// The Desktop Entry Specification's value escapes for a `string` (and
/// `localestring`) value: `\\`, `\n`, `\t`, `\r` and `\s`. Applied to every
/// value this module writes except `Exec=`'s own argument, which follows the
/// separate quoting rules in [`exec_argument`] instead.
///
/// A literal `"` needs none of this: a desktop entry value is not itself
/// quoted (the line simply runs to its end), so a quote character inside
/// `Name=` passes through unescaped and stays spec-valid.
fn escape_value(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            ' ' => escaped.push_str("\\s"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// `Exec=`'s own argument, per the specification's quoting rules rather than
/// [`escape_value`]'s: the hub executable path is a user's `$HOME`, which may
/// hold a space, and an app author's icon or product name never reaches this
/// function at all.
///
/// Doubled `%` first — a stray `%f` in a path must never be read as a field
/// code — then wrapped in double quotes with `"`, `` ` ``, `$` and `\`
/// backslash-escaped inside them, exactly as the specification requires
/// inside a double-quoted `Exec` argument.
fn exec_argument(path: &Path) -> String {
    let percent_doubled = path.display().to_string().replace('%', "%%");

    let mut quoted = String::with_capacity(percent_doubled.len() + 2);
    quoted.push('"');
    for character in percent_doubled.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '`' => quoted.push_str("\\`"),
            '$' => quoted.push_str("\\$"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

/// Render and write `identity`'s entry, creating the applications directory
/// if needed and writing atomically — a desktop watching the directory must
/// never see a partial file.
///
/// A missing `icon_path` is not an error: the entry is written with no
/// `Icon=` line, and this says so on stderr in CONTRACT.md §8's register — the
/// nearest honourable outcome, spoken rather than silent.
pub fn write(
    id: &str,
    identity: &Identity,
    hub_executable: &Path,
    paths: &Paths,
) -> Result<PathBuf, DesktopError> {
    if identity.icon_path.is_none() {
        eprintln!(
            "tfsapp-hub: {id} declares no icon_path — its desktop entry has none and shows a \
             generic icon."
        );
    }

    let path = paths.desktop_entry_path(&identity.identifier)?;
    let directory = paths.applications_dir();
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| DesktopError::Io { path, source }
    };

    fs::create_dir_all(&directory).map_err(io_error(&directory))?;

    let contents = render(id, identity, hub_executable);
    let temporary = path.with_extension("desktop.tmp");
    fs::write(&temporary, &contents).map_err(io_error(&temporary))?;
    fs::rename(&temporary, &path).map_err(io_error(&path))?;

    // GNOME notices a new file on its own; this call is for the desktops that
    // cache. Best-effort and quiet: a `.desktop` binary missing from `PATH`
    // is ordinary, not a reason to fail the write that already succeeded.
    let _ = Command::new("update-desktop-database")
        .arg(&directory)
        .output();

    Ok(path)
}

/// What [`remove`] did to an app's entry — the third outcome is the point:
/// a caller can report it instead of pretending it removed something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    Removed,
    Absent,
    /// A file exists at the path but does not carry `id`'s marker — never
    /// touched.
    LeftAlone,
}

/// Delete `identifier`'s entry, but only if it carries `id`'s `X-TFSApp-Id`
/// marker — see the module header for why that check exists at all.
pub fn remove(id: &str, identifier: &str, paths: &Paths) -> Result<RemovalOutcome, DesktopError> {
    let path = paths.desktop_entry_path(identifier)?;

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RemovalOutcome::Absent),
        Err(source) => return Err(DesktopError::Io { path, source }),
    };

    if !carries_marker(&contents, id) {
        return Ok(RemovalOutcome::LeftAlone);
    }

    fs::remove_file(&path).map_err(|source| DesktopError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(RemovalOutcome::Removed)
}

/// Whether `contents` carries the exact `X-TFSApp-Id=<id>` line [`write`]
/// would have written for `id`.
fn carries_marker(contents: &str, id: &str) -> bool {
    let marker = format!("X-TFSApp-Id={}", escape_value(id));
    contents.lines().any(|line| line == marker)
}

#[derive(Debug)]
pub enum DesktopError {
    Paths(PathsError),
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for DesktopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for DesktopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Paths(error) => Some(error),
            Self::Io { source, .. } => Some(source),
        }
    }
}

impl From<PathsError> for DesktopError {
    fn from(error: PathsError) -> Self {
        Self::Paths(error)
    }
}

#[cfg(test)]
#[path = "desktop_tests.rs"]
mod tests;
