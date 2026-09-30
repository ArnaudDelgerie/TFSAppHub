//! Rendering, writing and removing one app's `.desktop` entry.
//!
//! The whole file format lives here so nothing about it leaks into `install`
//! or `remove` — both call [`write`] and [`remove`] and know nothing about
//! keys, escaping or quoting.
//!
//! **The filename is `<identifier>.desktop`**, resolved by
//! [`crate::paths::Paths::desktop_entry_path`]: it is the file GNOME matches
//! first against `_GTK_APPLICATION_ID`
//! (plan 003, step 5), and a `tfsapp-`-prefixed
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
///
/// `mime_types` is the `file_associations` declaration passed in on the side,
/// never part of the `Identity` the window builds from: a declaring app gets
/// a `MimeType=` line and an `Exec=` ending in `-- %F`, so the desktop
/// environment offers it in "Open with" and passes the selected files as
/// separate arguments after the hub's own separator; an app without
/// associations keeps exactly the entry it has always had. The MIME values
/// were validated at manifest parse (restricted RFC 6838 alphabet, no `;`,
/// no whitespace, no control characters), so they are written as they stand.
pub fn render(
    id: &str,
    identity: &Identity,
    hub_executable: &Path,
    mime_types: &[String],
) -> String {
    let declared = !mime_types.is_empty();
    let mut entry = String::new();
    entry.push_str("[Desktop Entry]\n");
    entry.push_str("Type=Application\n");
    entry.push_str("Version=1.0\n");
    entry.push_str(&format!("Name={}\n", escape_value(&identity.product_name)));
    // `%F` is a standalone, unquoted field code: the desktop environment
    // expands it to the selected files as separate local-file arguments —
    // never resold or re-quoted, exactly what `open <id> --`'s separator
    // expects to find after it.
    entry.push_str(&format!(
        "Exec={} open {}{}\n",
        exec_argument(hub_executable),
        id,
        if declared { " -- %F" } else { "" }
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
    if declared {
        // One trailing `;`, and one between each — the specification's list
        // form. A declaration of support, not a default-application claim:
        // which app the desktop *prefers* for a type stays the user's own
        // setting.
        entry.push_str(&format!("MimeType={};\n", mime_types.join(";")));
    }
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
    mime_types: &[String],
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

    let contents = render(id, identity, hub_executable, mime_types);
    let temporary = path.with_extension("desktop.tmp");
    fs::write(&temporary, &contents).map_err(io_error(&temporary))?;
    fs::rename(&temporary, &path).map_err(io_error(&path))?;

    refresh_database(&directory);

    Ok(path)
}

/// Ask the desktop's caching layer to re-read the applications directory,
/// best-effort and quiet: a `.desktop` utility missing from `PATH` is
/// ordinary, not a reason to fail the write or removal that already
/// succeeded. The cache it rebuilds — `mimeinfo.cache` — is regenerated from
/// the directory's own contents, so foreign entries keep their MIME lists
/// and this hub never edits a default-application preference.
fn refresh_database(directory: &Path) {
    let _ = Command::new("update-desktop-database")
        .arg(directory)
        .output();
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
    remove_if(identifier, paths, |contents| carries_marker(contents, id))
}

/// [`remove`]'s own version for a subject with no `id` left to verify
/// against — `purge <identifier>`'s orphan case (plan 023 step 3). The file
/// is named after `identifier` ([`Paths::desktop_entry_path`]), so *any*
/// hub-written entry at that path belongs to this identifier regardless of
/// which `id` wrote it; a hand-written or foreign file — one carrying no
/// `X-TFSApp-Id=` line at all — is left alone exactly as [`remove`] leaves
/// one alone.
pub fn remove_any(identifier: &str, paths: &Paths) -> Result<RemovalOutcome, DesktopError> {
    remove_if(identifier, paths, carries_any_marker)
}

/// Shared by [`remove`] and [`remove_any`]: read the entry, decide whether
/// `carries` says it is this hub's to delete, and delete it if so.
fn remove_if(
    identifier: &str,
    paths: &Paths,
    carries: impl Fn(&str) -> bool,
) -> Result<RemovalOutcome, DesktopError> {
    let path = paths.desktop_entry_path(identifier)?;

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RemovalOutcome::Absent),
        Err(source) => return Err(DesktopError::Io { path, source }),
    };

    if !carries(&contents) {
        return Ok(RemovalOutcome::LeftAlone);
    }

    fs::remove_file(&path).map_err(|source| DesktopError::Io {
        path: path.clone(),
        source,
    })?;
    // The cache still advertises the removed entry's MIME types until it is
    // rebuilt, so a fresh removal refreshes like a write does.
    refresh_database(&paths.applications_dir());
    Ok(RemovalOutcome::Removed)
}

/// Whether `contents` carries the exact `X-TFSApp-Id=<id>` line [`write`]
/// would have written for `id`.
fn carries_marker(contents: &str, id: &str) -> bool {
    let marker = format!("X-TFSApp-Id={}", escape_value(id));
    contents.lines().any(|line| line == marker)
}

/// Whether `contents` carries an `X-TFSApp-Id=` line at all, regardless of
/// which id it names — [`remove_any`]'s looser version of [`carries_marker`].
fn carries_any_marker(contents: &str) -> bool {
    contents
        .lines()
        .any(|line| line.starts_with("X-TFSApp-Id="))
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
