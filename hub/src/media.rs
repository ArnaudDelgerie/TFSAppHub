//! `actions.media`: the microphone reached through the ordinary web platform
//! (CONTRACT.md §7, `.project/decision/007-the-microphone-is-a-declared-capability.md`).
//!
//! Unlike every other capability group, this one grants nothing to
//! `invoke()` and starts no bridge route — an app that declares
//! `actions.media.microphone` reaches it by calling `getUserMedia()` on its
//! own page, exactly as it would in a browser that had granted the
//! permission. What this module does is make that call succeed only when it
//! should: WebKitGTK's `enable-media-stream` setting is written only when the
//! manifest declares the microphone, and a `permission-request` handler is
//! connected on **every** window regardless — wry itself connects none, so
//! installing a handler that defaults to allow-by-omission would be worse
//! than the platform's own default; this handler makes the deny explicit and
//! logged instead.
//!
//! **The decision is pure and the wiring is not**, and they are kept apart
//! for exactly that reason: [`decide`] takes plain values and is what the
//! unit tests exhaust the table of, while [`install_permission_handler`]
//! reaches into `webkit2gtk` to read the webview's live URI and connect the
//! real signal. The origin check reuses `window::same_origin` — "the app's
//! own origin" is defined once, by `window.rs`'s `classify_navigation`, and
//! this module asks the same question rather than re-deriving it.
//!
//! **Exactly two request types can ever be allowed**, both only while the
//! microphone is declared and the requesting page is the app's own origin: a
//! `UserMediaPermissionRequest` for an audio device and not a video device
//! (WebKit offers no partial grant on a combined request, so audio+video is
//! refused as a whole), and a `DeviceInfoPermissionRequest` — without it,
//! `enumerateDevices()` returns no labels and the app cannot let a person
//! choose between a built-in microphone and a headset. Every other
//! permission kind is denied unconditionally.

use tauri::Url;

use crate::window::{same_origin, AppOriginSlot};

/// What WebKit is asking to authorize, reduced to what [`decide`] needs to
/// know. `Other` covers every permission kind this group does not name:
/// geolocation, notification, pointer lock, a media key system, website data
/// access, and whatever WebKit adds next — all denied unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPermissionKind {
    UserMedia { audio: bool, video: bool },
    DeviceInfo,
    Other,
}

/// The pure decision the `permission-request` handler applies on every
/// window (decision 007): allow only an audio-only capture request or a
/// device-info request, and only while the app declared the microphone and
/// the requesting page is the app's own origin. Everything else — a video or
/// combined request, any other permission kind, an undeclared app, a page
/// that is not (or not yet) the app's own origin — is denied.
pub fn decide(
    kind: MediaPermissionKind,
    microphone_declared: bool,
    page_is_app_origin: bool,
) -> bool {
    if !microphone_declared || !page_is_app_origin {
        return false;
    }
    match kind {
        MediaPermissionKind::UserMedia { audio, video } => audio && !video,
        MediaPermissionKind::DeviceInfo => true,
        MediaPermissionKind::Other => false,
    }
}

/// Whether the page a request came from is the app's own origin — `false`
/// while the splash has not learned the backend's origin yet, which is what
/// keeps the cold-start page from ever receiving a grant (decision 007).
/// Pure wrapper over `window::same_origin`, kept here so [`decide`]'s callers
/// need not import `window.rs` for one comparison.
pub fn page_is_app_origin(app_origin: Option<&Url>, current_page: Option<&Url>) -> bool {
    match (app_origin, current_page) {
        (Some(origin), Some(page)) => same_origin(origin, page),
        _ => false,
    }
}

/// The reason logged for a denied user-media request — the caller already
/// knows it is a `UserMedia` kind; this only says *why* [`decide`] refused
/// it, so `hub.log` names a cause rather than just "denied".
pub fn user_media_denial_reason(
    microphone_declared: bool,
    page_is_app_origin: bool,
    audio: bool,
    video: bool,
) -> &'static str {
    if !microphone_declared {
        "this app does not declare actions.media.microphone"
    } else if !page_is_app_origin {
        "the request was not on the app's own origin"
    } else if video {
        "the request included video, and this group grants audio only"
    } else if !audio {
        "the request was not for an audio device"
    } else {
        "denied"
    }
}

/// Classify a live WebKit permission request into what [`decide`] needs.
/// Not unit-tested: constructing a real `WebKitPermissionRequest` needs a
/// running WebKitGTK process, which is exactly what the pure functions above
/// exist to keep out of this table's own tests.
fn classify_request(request: &webkit2gtk::PermissionRequest) -> MediaPermissionKind {
    use webkit2gtk::glib::Cast;
    use webkit2gtk::{DeviceInfoPermissionRequest, UserMediaPermissionRequest};

    if let Some(user_media) = request.downcast_ref::<UserMediaPermissionRequest>() {
        use webkit2gtk::UserMediaPermissionRequestExt;
        MediaPermissionKind::UserMedia {
            audio: user_media.is_for_audio_device(),
            video: user_media.is_for_video_device(),
        }
    } else if request
        .downcast_ref::<DeviceInfoPermissionRequest>()
        .is_some()
    {
        MediaPermissionKind::DeviceInfo
    } else {
        MediaPermissionKind::Other
    }
}

/// Install this window's media grant: the `enable-media-stream` setting when
/// `microphone_declared`, and the `permission-request` handler unconditionally
/// (plan 050 step 2). Called after every window `create_splash_window` and
/// `create_app_window` build, on both the splash and every later window —
/// the splash and the app share one webview (`architecture/09`), so this is
/// what keeps a page that is not yet the app's own origin from ever getting a
/// grant, and what turns wry's absent-handler default into an explicit,
/// logged deny for an app that declared nothing.
///
/// Returns whatever `WebviewWindow::with_webview`'s dispatch returned: `Err`
/// only when the runtime could not schedule the closure at all (the window is
/// already gone, or the event loop is shutting down), which is the "handler
/// was actually installed on this backend" signal `TFS_MEDIA_MICROPHONE`
/// reports (CONTRACT.md §3, §8).
pub fn install_permission_handler<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    microphone_declared: bool,
    app_origin: AppOriginSlot,
) -> tauri::Result<()> {
    window.with_webview(move |webview| {
        use webkit2gtk::{SettingsExt, WebViewExt};

        let webview = webview.inner();
        if microphone_declared {
            if let Some(settings) = webview.settings() {
                settings.set_enable_media_stream(true);
            }
        }
        webview.connect_permission_request(move |webview, request| {
            use webkit2gtk::PermissionRequestExt;

            let current_page = webview.uri().and_then(|uri| Url::parse(&uri).ok());
            let is_app_origin = page_is_app_origin(app_origin.get(), current_page.as_ref());
            let kind = classify_request(request);
            if decide(kind, microphone_declared, is_app_origin) {
                request.allow();
            } else {
                if let MediaPermissionKind::UserMedia { audio, video } = kind {
                    eprintln!(
                        "tfsapp-hub: denying a user-media permission request: {}",
                        user_media_denial_reason(microphone_declared, is_app_origin, audio, video)
                    );
                }
                request.deny();
            }
            true
        });
    })
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
