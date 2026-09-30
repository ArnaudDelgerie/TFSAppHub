//! `actions.media`: the microphone reached through the ordinary web platform
//! (CONTRACT.md §7, decision 007).
//!
//! Unlike every other capability group, this one grants nothing to
//! `invoke()` and starts no bridge route — an app that declares
//! `actions.media.microphone` reaches it by calling `getUserMedia()` on its
//! own page, exactly as it would in a browser that had granted the
//! permission. What this module does is make that call succeed only when it
//! should: WebKitGTK's `enable-media-stream` setting is written only when the
//! microphone is [`MicrophoneAccess::Allowed`] — declared in the manifest and
//! not revoked in this installation's `data/config.json` (plan 070) — and a
//! `permission-request` handler is connected on **every** window regardless — wry itself connects none, so
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
//! **The grant is reported, not assumed.** The install's closure reports on
//! a channel whether it really wrote the setting and connected the handler,
//! and [`await_grant`] — with [`MICROPHONE_GRANT_DEADLINE`] — is what turns
//! that report into `TFS_MEDIA_MICROPHONE` (decision 007 §5). Only the
//! off-main-thread caller may wait on it: the closure runs on the main
//! thread, so a main-thread wait could only deadlock on the very report it
//! is waiting for.
//!
//! **A running capture is visible, and a revoked one never starts.** The
//! access this module is handed ([`MicrophoneAccess`]) is one value computed
//! once per launch from the manifest and `data/config.json`'s `revoked` key
//! (plan 070): a revoked microphone follows the undeclared rule — nothing is
//! written, nothing is granted, and the denial names the revocation. An
//! allowed one is visible while it runs: the same closure connects
//! `microphone-capture-state-notify` and sets the toplevel's title from
//! [`capture_title`] (`Microphone on — <product name>`, one `hub.log` line
//! per change), writing the `gtk::HeaderBar` tao builds under Wayland too,
//! and a second `web-process-terminated` handler — the signal `crash.rs`
//! owns, touching nothing of the crash page — puts the plain name back so a
//! dead web process cannot leave the indicator stuck.
//!
//! **Exactly two request types can ever be allowed**, both only while the
//! microphone is declared and the requesting page is the app's own origin: a
//! `UserMediaPermissionRequest` for an audio device and not a video device
//! (WebKit offers no partial grant on a combined request, so audio+video is
//! refused as a whole), and a `DeviceInfoPermissionRequest` — without it,
//! `enumerateDevices()` returns no labels and the app cannot let a person
//! choose between a built-in microphone and a headset. Every other
//! permission kind is denied unconditionally.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use tauri::Url;

use crate::window::{same_origin, AppOriginSlot};

/// How long [`await_grant`] waits for the `with_webview` closure's report
/// before answering `false`: long enough for a queued closure on a live
/// event loop to run, short enough that a stalled one cannot hold the
/// sidecar's start hostage (`main::serve` waits before the sidecar exists).
pub const MICROPHONE_GRANT_DEADLINE: Duration = Duration::from_secs(2);

/// Wait for the closure's report, off the main thread only. `None` is the
/// dispatch failure case — the closure was never even scheduled, so there is
/// nothing to wait for. A report answers whatever it said; no report — never
/// scheduled, timed out, or a closed channel — answers `false`, with the one
/// `hub.log` line that says so —
/// `TFS_MEDIA_MICROPHONE` mirrors what was actually granted (decision 007
/// §5), so an unconfirmed grant counts as no grant at all.
pub fn await_grant(receiver: Option<Receiver<bool>>, deadline: Duration) -> bool {
    use std::sync::mpsc::RecvTimeoutError;

    let Some(receiver) = receiver else {
        eprintln!(
            "tfsapp-hub: warning: the microphone grant could not be scheduled; \
             TFS_MEDIA_MICROPHONE=0"
        );
        return false;
    };
    match receiver.recv_timeout(deadline) {
        Ok(granted) => granted,
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
            eprintln!(
                "tfsapp-hub: warning: the microphone grant was not confirmed within {} s; \
                 TFS_MEDIA_MICROPHONE=0",
                deadline.as_secs()
            );
            false
        }
    }
}

/// The state of this window's microphone capture, WebKitGTK's own
/// `MediaCaptureState` reduced to what [`capture_title`] needs. A local
/// three-variant enum on purpose: the platform type needs a running
/// WebKitGTK to construct, and the mapping is one function at the wiring's
/// single point ([`capture_state_of`]) rather than a test dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    /// No capture is held — also the answer for any platform value this
    /// enum does not name yet.
    None,
    /// A capture stream is live.
    Active,
    /// A capture is held but muted.
    Muted,
}

/// The window title for a capture state (plan 070): the visible half of
/// decision 007's weakest point, bought back. A running capture reads
/// `Microphone on — <product name>`, a muted one says so, and any stop —
/// including a dead web process — returns the plain product name.
pub fn capture_title(product_name: &str, state: CaptureState) -> String {
    match state {
        CaptureState::Active => format!("Microphone on — {product_name}"),
        CaptureState::Muted => format!("Microphone muted — {product_name}"),
        CaptureState::None => product_name.to_string(),
    }
}

/// Map the platform's capture state onto [`CaptureState`]. The only place
/// `webkit2gtk::MediaCaptureState` is named outside the handler itself.
fn capture_state_of(state: webkit2gtk::MediaCaptureState) -> CaptureState {
    match state {
        webkit2gtk::MediaCaptureState::Active => CaptureState::Active,
        webkit2gtk::MediaCaptureState::Muted => CaptureState::Muted,
        // `None`, the hidden `__Unknown`, and anything a newer WebKit adds
        // that this enum has not named yet: no capture this window knows how
        // to describe.
        _ => CaptureState::None,
    }
}

/// The toplevel GTK window this webview lives in, which is where a title
/// can be set at all. Not unit-tested: constructing a real webview needs
/// the running WebKitGTK the pure functions above exist to keep out.
fn toplevel_gtk_window(webview: &webkit2gtk::WebView) -> Option<gtk::Window> {
    use gtk::prelude::WidgetExt;
    use webkit2gtk::glib::Cast;

    webview.toplevel()?.downcast::<gtk::Window>().ok()
}

/// The first `HeaderBar` in `widget`'s own subtree. Tauri's Wayland
/// backend (tao 0.35) installs the window's titlebar as an `EventBox`
/// holding a `HeaderBar`, so the search is general rather than one
/// hard-coded `EventBox` step; a window without any `HeaderBar` is the
/// server-side-decorations case, which [`set_indicator_title`]'s plain
/// call already covers.
fn find_header_bar(widget: &gtk::Widget) -> Option<gtk::HeaderBar> {
    use gtk::prelude::{Cast, ContainerExt};

    if let Some(header) = widget.downcast_ref::<gtk::HeaderBar>() {
        return Some(header.clone());
    }
    widget
        .downcast_ref::<gtk::Container>()
        .into_iter()
        .flat_map(|container| container.children())
        .find_map(|child| find_header_bar(&child))
}

/// Set this webview's window title to `title`, through every layer that
/// can own the visible one. `gtk_window_set_title` first: the window's own
/// title property, and the only thing a window-manager-drawn title bar
/// ever reads. Then tao's Wayland titlebar: its `HeaderBar` carries a
/// snapshot of the title taken when the window was built and never follows
/// the window's own (tao 0.35 — tauri issue 13749), so on a Wayland launch
/// the window property alone updates a title nobody sees. Both layers get
/// the same string, so whichever one draws, it draws one truth.
///
/// The one failure worth a line: no toplevel GTK window at all, which is
/// the one way the indicator can be silently lost.
fn set_indicator_title(webview: &webkit2gtk::WebView, title: &str) {
    use gtk::prelude::{GtkWindowExt, HeaderBarExt};

    match toplevel_gtk_window(webview) {
        Some(window) => {
            window.set_title(title);
            if let Some(header) = window.titlebar().as_ref().and_then(find_header_bar) {
                header.set_title(Some(title));
            }
        }
        None => {
            eprintln!(
                "tfsapp-hub: warning: the webview has no GTK toplevel window; the \
                 microphone indicator cannot set the title"
            );
        }
    }
}

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

/// What one launch may do with the microphone, computed once in `main`
/// from the manifest's declaration and this installation's
/// `data/config.json` (plan 070): an undeclared app meets the platform with
/// no capture at all, a declared-and-revoked one the same, and only
/// `Allowed` ever grants — `TFS_MEDIA_MICROPHONE` mirrors the value that
/// actually ran, never what the manifest asked for (decision 007 §5, its
/// dated revision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneAccess {
    /// `actions.media.microphone` is absent: the capability is *absent*, not
    /// denied — `enable-media-stream` is not written (decision 007 §3).
    Undeclared,
    /// Declared, but `"revoked": {"media": {"microphone": true}}` in this
    /// installation's `data/config.json`. Follows §3's own rule: the setting
    /// is not written either, so a revoked app meets the platform with no
    /// capture, like an undeclared one — but its denials say *why*.
    Revoked,
    /// Declared and not revoked: the only value that ever grants.
    Allowed,
}

impl MicrophoneAccess {
    /// Pure: `Undeclared` wins over `Revoked`, because an app that declared
    /// nothing has nothing to revoke — the two failures share a platform
    /// state but not a cause, and `hub.log` names the right one.
    pub fn of(declared: bool, revoked: bool) -> Self {
        match (declared, revoked) {
            (true, true) => MicrophoneAccess::Revoked,
            (true, false) => MicrophoneAccess::Allowed,
            (false, _) => MicrophoneAccess::Undeclared,
        }
    }
}

/// The pure decision the `permission-request` handler applies on every
/// window (decision 007): allow only an audio-only capture request or a
/// device-info request, and only while the app's microphone is
/// [`MicrophoneAccess::Allowed`] and the requesting page is the app's own
/// origin. Everything else — a video or combined request, any other
/// permission kind, an undeclared app, a revoked one, a page that is not
/// (or not yet) the app's own origin — is denied.
pub fn decide(
    kind: MediaPermissionKind,
    access: MicrophoneAccess,
    page_is_app_origin: bool,
) -> bool {
    if access != MicrophoneAccess::Allowed || !page_is_app_origin {
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
    access: MicrophoneAccess,
    page_is_app_origin: bool,
    audio: bool,
    video: bool,
) -> &'static str {
    match access {
        MicrophoneAccess::Undeclared => "this app does not declare actions.media.microphone",
        MicrophoneAccess::Revoked => "the microphone is revoked in data/config.json",
        MicrophoneAccess::Allowed if !page_is_app_origin => {
            "the request was not on the app's own origin"
        }
        MicrophoneAccess::Allowed if video => {
            "the request included video, and this group grants audio only"
        }
        MicrophoneAccess::Allowed if !audio => "the request was not for an audio device",
        MicrophoneAccess::Allowed => "denied",
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

/// Install this window's media grant and, with it, the capture indicator:
/// the `enable-media-stream` setting when the microphone is
/// [`MicrophoneAccess::Allowed`], the `permission-request` handler
/// unconditionally (plan 050 step 2, plan 070 step 1), and — on a window
/// whose grant really applied — the title handlers of plan 070 step 2.
/// Called after every window `create_splash_window` and `create_app_window`
/// build, on both the splash and every later window — the splash and the
/// app share one webview (`architecture/09`), so this is what keeps a page
/// that is not yet the app's own origin from ever getting a grant, and what
/// turns wry's absent-handler default into an explicit, logged deny for an
/// app that declared nothing or had it revoked.
///
/// `product_name` is the same title the window was created with, so the
/// indicator's plain form is exactly the pre-capture title: a capture
/// prefixes it, any stop — including a dead web process — restores it.
///
/// Returns whatever `WebviewWindow::with_webview`'s dispatch returned, with
/// the receiver the closure will report on. `Ok` means the closure was
/// queued onto the GTK main thread, not that it has run yet —
/// `with_webview` is fire-and-forget, so the queue is not the grant. The
/// closure sends `true` once it has written the setting *and* connected the
/// handler, `false` otherwise, and [`await_grant`] — called off the main
/// thread only, never from the `setup` closure or the second-instance
/// callback that share the main thread the closure itself needs — turns
/// that report, or its absence within a deadline, into the value
/// `TFS_MEDIA_MICROPHONE` reports (CONTRACT.md §3, §8). `Err` means the
/// runtime could not even schedule it (the window is already gone, or the
/// event loop is shutting down): `await_grant(None, ..)` answers `false`
/// for that receiver. A second-instance window's receiver is simply
/// dropped — the backend it joins already has its environment.
pub fn install_permission_handler<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    product_name: String,
    access: MicrophoneAccess,
    app_origin: AppOriginSlot,
) -> tauri::Result<Receiver<bool>> {
    let (grant_report, receiver) = std::sync::mpsc::sync_channel(1);
    window.with_webview(move |webview| {
        use webkit2gtk::{SettingsExt, WebViewExt};

        let webview = webview.inner();
        // The grant is what really ran here: a webview with no `settings()`
        // keeps WebKitGTK's default of no capture at all, and an app whose
        // microphone is not `Allowed` is left alone on purpose — undeclared
        // and revoked follow §3's same rule. Said out loud only in the
        // first case — the second is the contract's own normal path.
        let granted = access == MicrophoneAccess::Allowed
            && match webview.settings() {
                Some(settings) => {
                    settings.set_enable_media_stream(true);
                    true
                }
                None => {
                    eprintln!(
                        "tfsapp-hub: warning: the webview has no settings; the microphone \
                         is not granted"
                    );
                    false
                }
            };
        webview.connect_permission_request(move |webview, request| {
            use webkit2gtk::PermissionRequestExt;

            let current_page = webview.uri().and_then(|uri| Url::parse(&uri).ok());
            let is_app_origin = page_is_app_origin(app_origin.get(), current_page.as_ref());
            let kind = classify_request(request);
            if decide(kind, access, is_app_origin) {
                request.allow();
            } else {
                if let MediaPermissionKind::UserMedia { audio, video } = kind {
                    eprintln!(
                        "tfsapp-hub: denying a user-media permission request: {}",
                        user_media_denial_reason(access, is_app_origin, audio, video)
                    );
                }
                request.deny();
            }
            true
        });
        // The indicator (plan 070 step 2) — only on a window whose grant
        // really applied: an undeclared or revoked app never holds a
        // capture, so handlers here would be ones that can only ever say
        // "stopped". No capture can predate these handlers: this same
        // closure is what enables media streams, so the first notify is
        // always the first capture. The title is read from the live state
        // each time, not inferred from the order of events.
        if granted {
            let capture_product_name = product_name.clone();
            webview.connect_microphone_capture_state_notify(move |webview| {
                let state = capture_state_of(webview.microphone_capture_state());
                set_indicator_title(webview, &capture_title(&capture_product_name, state));
                eprintln!(
                    "tfsapp-hub: microphone capture: {}",
                    match state {
                        CaptureState::Active => "active",
                        CaptureState::Muted => "muted",
                        CaptureState::None => "stopped",
                    }
                );
            });
            // A second handler on the same signal `crash.rs` already owns:
            // this one never touches the crash page, only the title — a web
            // process dying mid-capture is the one path the notify handler
            // cannot report, and without this the indicator would outlive
            // the page it was reporting on.
            let plain_product_name = product_name;
            webview.connect_web_process_terminated(move |webview, _reason| {
                set_indicator_title(webview, &plain_product_name);
            });
        }
        // Sent once, after everything is connected — the grant is only
        // whole when both halves are. A send error means the receiver is
        // gone (the window closed before the closure ran, or the caller
        // never waited): there is nobody left to tell.
        let _ = grant_report.send(granted);
    })?;
    Ok(receiver)
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
