//! The deliberately small native file and directory chooser IPC surface.
//!
//! This module returns a path chosen by the user; it does not grant filesystem
//! access, read the selection, or retain it. The runtime capability assembled
//! in `window.rs` is the authorization boundary for this command.

use std::path::PathBuf;

use serde::Deserialize;
use tauri_plugin_dialog::{DialogExt, FilePath};

const DIALOG_UNAVAILABLE: &str = "picker_unavailable";
const PATH_NOT_ABSOLUTE: &str = "picker_path_not_absolute";
const PATH_NOT_UTF8: &str = "picker_path_not_utf8";
const PATH_NOT_LOCAL: &str = "picker_path_not_local";

/// The only two native selection modes exposed to an app frontend.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PickKind {
    File,
    Directory,
}

/// Turn a completed native selection into the JSON IPC result.
///
/// `None` is cancellation. A native path is required to be absolute and valid
/// UTF-8 before it can cross the JSON boundary: replacing bytes would make the
/// returned string name a different path.
pub fn selected_path_to_wire(path: Option<PathBuf>) -> Result<Option<String>, &'static str> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err(PATH_NOT_ABSOLUTE);
    }
    path.into_os_string()
        .into_string()
        .map(Some)
        .map_err(|_| PATH_NOT_UTF8)
}

fn file_path_to_wire(path: Option<FilePath>) -> Result<Option<String>, &'static str> {
    let path = path
        .map(|path| path.into_path().map_err(|_| PATH_NOT_LOCAL))
        .transpose()?;
    selected_path_to_wire(path)
}

/// Open the requested native chooser from the calling window and await the
/// plugin callback without blocking either Tauri's main thread or its IPC
/// dispatcher.
#[tauri::command]
pub async fn pick_path(
    window: tauri::Window,
    kind: PickKind,
) -> Result<Option<String>, &'static str> {
    let (sender, mut receiver) = tauri::async_runtime::channel(1);
    let reply = move |path| {
        // The webview may have gone away while the native chooser was open;
        // dropping the reply is then normal and must not panic the dialog
        // callback thread.
        let _ = sender.blocking_send(path);
    };

    #[cfg(desktop)]
    let dialog = window.dialog().file().set_parent(&window);
    #[cfg(not(desktop))]
    let dialog = window.dialog().file();

    match kind {
        PickKind::File => dialog.pick_file(reply),
        PickKind::Directory => dialog.pick_folder(reply),
    }

    let selected = receiver.recv().await.ok_or(DIALOG_UNAVAILABLE)?;
    file_path_to_wire(selected)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{selected_path_to_wire, PickKind, PATH_NOT_ABSOLUTE, PATH_NOT_UTF8};

    #[test]
    fn both_documented_request_kinds_deserialise() {
        assert_eq!(
            serde_json::from_str::<PickKind>(r#""file""#).unwrap(),
            PickKind::File
        );
        assert_eq!(
            serde_json::from_str::<PickKind>(r#""directory""#).unwrap(),
            PickKind::Directory
        );
    }

    #[test]
    fn malformed_or_unknown_request_kinds_are_refused() {
        for kind in [r#""folder""#, r#"true"#, r#"{}"#] {
            assert!(serde_json::from_str::<PickKind>(kind).is_err(), "{kind}");
        }
    }

    #[test]
    fn cancellation_serialises_as_the_json_null_response() {
        let response = selected_path_to_wire(None).unwrap();

        assert_eq!(response, None);
        assert_eq!(serde_json::to_string(&response).unwrap(), "null");
    }

    #[test]
    fn relative_paths_are_not_claimed_to_be_native_selections() {
        assert_eq!(
            selected_path_to_wire(Some(PathBuf::from("relative"))),
            Err(PATH_NOT_ABSOLUTE)
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_refused_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let path = PathBuf::from(OsString::from_vec(b"/tmp/picker-\xff".to_vec()));

        assert_eq!(selected_path_to_wire(Some(path)), Err(PATH_NOT_UTF8));
    }
}
