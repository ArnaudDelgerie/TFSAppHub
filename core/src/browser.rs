use std::{
    collections::HashMap,
    process::{Command, Stdio},
};

// `url::Url` directly, not `tauri::Url` as the station writes it: the two are
// the same type — `tauri` merely re-exports this crate — but importing it
// through `tauri` would be the single import of that crate in `core/`, and
// this crate's whole point is that there is none. The caller in `hub/` can
// keep passing whatever `tauri` hands it. Worded so as not to spell that
// import out even in prose — plan 002 greps this tree for it, and a comment
// that trips a mechanical check teaches everyone to ignore the check.
use url::Url;

/// Every AppDir-injected variable `linuxdeploy-plugin-gtk`'s
/// `apprun-hooks/linuxdeploy-plugin-gtk.sh` exports, plus the dynamic
/// loader variables the AppImage's own `AppRun.wrapped` prepends — read back
/// from a built `tfsapp-demo_0.2.0_amd64.AppImage` (plan 054). Dropped
/// outright from a browser child's environment, unlike `XDG_DATA_DIRS`
/// below, which is rewritten instead of removed.
const APPIMAGE_ENV_VARS_TO_REMOVE: &[&str] = &[
    "GTK_PATH",
    "GTK_DATA_PREFIX",
    "GTK_EXE_PREFIX",
    "GTK_THEME",
    "GTK_IM_MODULE_FILE",
    "GDK_PIXBUF_MODULE_FILE",
    "GIO_EXTRA_MODULES",
    "GSETTINGS_SCHEMA_DIR",
    "GDK_BACKEND",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
];

/// What a browser child's environment needs changed relative to the
/// launcher's own (constraint 3): variables to drop outright, and
/// variables to overwrite with a rewritten value (only ever `XDG_DATA_DIRS`
/// — see `sanitize_browser_env`). Kept separate from `std::process::Command`
/// itself so the sanitising logic is unit-testable without spawning
/// anything.
#[derive(Debug, PartialEq, Eq)]
pub struct EnvDiff {
    pub remove: Vec<&'static str>,
    pub set: Vec<(&'static str, String)>,
}

/// Pure core of the environment sanitising (plan 054, constraint 3): given
/// the launcher's own environment, decide what a spawned browser's
/// environment must drop or rewrite so it doesn't inherit the AppImage's
/// GTK/XDG environment. `GTK_PATH` and friends are dropped outright;
/// `XDG_DATA_DIRS` is rewritten instead, dropping only the entries rooted
/// under `APPDIR` that the AppRun hook prepends — the system dirs listed
/// after them are legitimate and must survive. Outside an AppImage none of
/// these variables are set, so every check below is naturally a no-op —
/// this is what keeps a dev launch's environment untouched without a
/// separate code path.
pub fn sanitize_browser_env(env: &HashMap<String, String>) -> EnvDiff {
    let remove = APPIMAGE_ENV_VARS_TO_REMOVE
        .iter()
        .copied()
        .filter(|name| env.contains_key(*name))
        .collect::<Vec<_>>();

    let mut diff = EnvDiff {
        remove,
        set: Vec::new(),
    };

    if let Some(xdg_data_dirs) = env.get("XDG_DATA_DIRS") {
        let rewritten = strip_appdir_entries(xdg_data_dirs, env.get("APPDIR").map(String::as_str));
        if rewritten.is_empty() {
            diff.remove.push("XDG_DATA_DIRS");
        } else if rewritten != *xdg_data_dirs {
            diff.set.push(("XDG_DATA_DIRS", rewritten));
        }
    }

    diff
}

/// Drop every `:`-separated entry of `value` rooted under `appdir` — the
/// AppRun hook's own `"$APPDIR/usr/share:/usr/share:$XDG_DATA_DIRS"`
/// prepend. A no-op when `appdir` is unknown (dev, where `APPDIR` is never
/// set) or empty.
fn strip_appdir_entries(value: &str, appdir: Option<&str>) -> String {
    let Some(appdir) = appdir.filter(|appdir| !appdir.is_empty()) else {
        return value.to_string();
    };
    value
        .split(':')
        .filter(|entry| !entry.starts_with(appdir))
        .collect::<Vec<_>>()
        .join(":")
}

/// Open `url` in the user's default browser via `xdg-open`, spawned
/// detached from the launcher (no stdin/stdout/stderr inheritance, never
/// waited on — the browser outlives the click) with `sanitize_browser_env`'s
/// diff applied so it doesn't inherit the AppImage's GTK/XDG environment
/// (constraint 3). Never fatal: a missing `xdg-open` or a spawn failure is
/// logged and the launcher keeps running — the same click just does
/// nothing, rather than a dialog interrupting whatever the user was doing.
pub fn open_in_system_browser(url: &Url) {
    let env: HashMap<String, String> = std::env::vars().collect();
    let diff = sanitize_browser_env(&env);

    let mut command = Command::new("xdg-open");
    command.arg(url.as_str());
    for name in &diff.remove {
        command.env_remove(name);
    }
    for (name, value) in &diff.set {
        command.env(name, value);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Err(error) = command.spawn() {
        eprintln!("TFSAppHub: cannot open {url} in the system browser: {error}");
    }
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod tests;
