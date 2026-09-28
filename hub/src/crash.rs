//! Recovering from a dead `WebKitWebProcess` (plan 058).
//!
//! Nothing in the hub listened for WebKit's `web-process-terminated` signal
//! before this: a renderer crash — a bad GStreamer decoder, a GPU driver, a
//! WebKit memory limit — left the window showing whatever the dead process
//! had last painted, with no way back short of closing the app. This module
//! installs one handler per window, on **every** window `create_splash_window`
//! and `create_app_window` build, mirroring `media.rs`'s own split: [`decide`]
//! is the pure table the unit tests exhaust, [`install_crash_recovery_handler`]
//! is what reaches into `webkit2gtk` to connect the real signal.
//!
//! **A termination the hub asked for itself is not a crash.**
//! `TerminatedByApi` is logged and left alone — nothing in this tree calls
//! `webkit_web_view_terminate_web_process` today, but the signal fires the
//! same way for it, and treating it as a crash would show the crash page
//! over a termination the hub itself caused. `Crashed` and
//! `ExceededMemoryLimit` — and any reason a future WebKitGTK adds, since
//! [`webkit2gtk::WebProcessTerminationReason`] is `#[non_exhaustive]` — are
//! recovered from: logged, and the dead document's close-guard state is
//! released ([`crate::close_guard::CloseGuardState::end_document`]) so a
//! guard held by a page that no longer exists cannot block closing the
//! window. Showing the crash page itself is plan 058 step 2's addition.

use webkit2gtk::WebProcessTerminationReason;

/// What [`install_crash_recovery_handler`] does about a termination reason —
/// the pure decision the unit tests exhaust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationAction {
    /// The web process died on its own: log it, and release the dead
    /// document's close-guard state.
    Recover,
    /// The hub asked for this itself: log it, touch nothing else.
    Ignore,
}

/// Decide [`TerminationAction`] from WebKit's own reason. A reason this crate
/// does not yet have a name for falls to [`TerminationAction::Recover`] — the
/// safe default for a signal that fired because the process is, in fact,
/// gone, rather than silently trusting an unrecognised reason to be benign.
pub fn decide(reason: WebProcessTerminationReason) -> TerminationAction {
    match reason {
        WebProcessTerminationReason::TerminatedByApi => TerminationAction::Ignore,
        _ => TerminationAction::Recover,
    }
}

/// The word [`install_crash_recovery_handler`]'s `hub.log` line uses for
/// `reason` — never the `Debug` spelling, which names an internal enum
/// variant rather than reading as a sentence fragment.
fn reason_label(reason: WebProcessTerminationReason) -> &'static str {
    match reason {
        WebProcessTerminationReason::Crashed => "crashed",
        WebProcessTerminationReason::ExceededMemoryLimit => "exceeded its memory limit",
        WebProcessTerminationReason::TerminatedByApi => "was terminated by the hub itself",
        _ => "terminated for a reason this hub does not recognise yet",
    }
}

/// Install this window's crash-recovery handler: `web-process-terminated`,
/// connected through `with_webview` exactly like
/// [`crate::media::install_permission_handler`], on both the splash and
/// every later window — the two windows share one webview across hand-over
/// (`architecture/09`), so this one install covers the window's whole life.
///
/// Returns whatever `WebviewWindow::with_webview`'s dispatch returned. `Ok`
/// means the closure was queued onto the GTK main thread, not that it has run
/// yet — the same caveat `media::install_permission_handler` documents, for
/// the same reason: `with_webview` is fire-and-forget.
pub fn install_crash_recovery_handler<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    close_guards: crate::close_guard::SharedCloseGuards,
) -> tauri::Result<()> {
    let label = window.label().to_string();
    window.with_webview(move |webview| {
        use webkit2gtk::WebViewExt;

        let webview = webview.inner();
        webview.connect_web_process_terminated(move |webview, reason| {
            use webkit2gtk::WebViewExt;

            let uri = webview.uri();
            eprintln!(
                "tfsapp-hub: window {label:?}'s web process {} while it was showing {}",
                reason_label(reason),
                uri.as_deref().unwrap_or("no page yet"),
            );
            if let TerminationAction::Recover = decide(reason) {
                close_guards.end_document(&label);
            }
        });
    })
}

#[cfg(test)]
#[path = "crash_tests.rs"]
mod tests;
