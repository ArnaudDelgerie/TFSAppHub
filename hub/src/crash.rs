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
//! recovered from: logged, the dead document's close-guard state is released
//! ([`crate::close_guard::CloseGuardState::end_document`]) so a guard held by
//! a page that no longer exists cannot block closing the window, and the
//! hub's own crash page ([`render_crash_page`]) replaces whatever the dead
//! process last painted.
//!
//! **Showing the page is not a navigation.** `load_alternate_html` displays
//! content *for* the URI that died without issuing a real load, so the
//! navigation policy in `window.rs` never sees it and a reload still lands on
//! the real page. The Reload link's `href` is that same dead URI — clicking
//! it is an ordinary link navigation, so it goes through the very same
//! `classify_navigation`/`on_page_load` machinery as any other load, with no
//! separate reload path to keep in sync with those. There is deliberately no
//! automatic reload: whatever crashed the process may crash again on the same
//! input, so the user's click is what breaks that loop.

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

/// Render the hub's own crash page: inline, self-contained HTML shown in
/// place of a dead web process. Colours follow the same `--splash-bg` /
/// `--splash-text` CSS custom properties and fallback hexes as the bundled
/// splash page (`hub/dist/index.html`), so a crash reads as the same host
/// speaking, not a different surface — a declared colour is emitted as a
/// custom-property declaration, an absent one leaves the fallback in the
/// `var(..., #hex)` rules alone. `reload_uri` becomes the Reload link's
/// `href`, verbatim but escaped.
fn render_crash_page(
    product_name: &str,
    splash_bg: Option<&str>,
    splash_text: Option<&str>,
    reload_uri: &str,
) -> String {
    let mut vars = String::new();
    if let Some(bg) = splash_bg {
        vars.push_str(&format!("--splash-bg:{};", escape_html(bg)));
    }
    if let Some(text) = splash_text {
        vars.push_str(&format!("--splash-text:{};", escape_html(text)));
    }
    format!(
        "<!doctype html>\
<html><head><meta charset=\"utf-8\">\
<style>\
:root{{{vars}}}\
body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:var(--splash-bg,#1e1e1e);color:var(--splash-text,#e0e0e0);font-family:sans-serif;}}\
main{{display:flex;flex-direction:column;align-items:center;gap:16px;text-align:center;}}\
a.reload{{color:inherit;border:1px solid currentColor;padding:8px 20px;border-radius:4px;\
text-decoration:none;}}\
</style></head>\
<body><main>\
<span>{name} stopped responding.</span>\
<a class=\"reload\" href=\"{uri}\">Reload</a>\
</main></body></html>",
        name = escape_html(product_name),
        uri = escape_html(reload_uri),
    )
}

/// Minimal HTML escaping for the two contexts [`render_crash_page`] ever
/// interpolates into — a text node and a double-quoted attribute value.
/// Nothing else in this crate escapes markup: `desktop::escape_value` escapes
/// for the Desktop Entry Specification, a different format entirely, and
/// there is no templating crate among this crate's dependencies to reuse.
fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Install this window's crash-recovery handler: `web-process-terminated`,
/// connected through `with_webview` exactly like
/// [`crate::media::install_permission_handler`], on both the splash and
/// every later window — the two windows share one webview across hand-over
/// (`architecture/09`), so this one install covers the window's whole life.
/// `product_name`/`splash_bg`/`splash_text` are the same three values
/// `window::splash_style` renders the cold-start page with.
///
/// Returns whatever `WebviewWindow::with_webview`'s dispatch returned. `Ok`
/// means the closure was queued onto the GTK main thread, not that it has run
/// yet — the same caveat `media::install_permission_handler` documents, for
/// the same reason: `with_webview` is fire-and-forget.
pub fn install_crash_recovery_handler<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    close_guards: crate::close_guard::SharedCloseGuards,
    product_name: String,
    splash_bg: Option<String>,
    splash_text: Option<String>,
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
                // No URI yet means the crash landed before any page ever
                // committed — there is nothing to show the page "for" and
                // nothing for Reload to point at, so this rare window is
                // left to the user closing it, same as before this plan.
                if let Some(uri) = uri.as_deref() {
                    let page = render_crash_page(
                        &product_name,
                        splash_bg.as_deref(),
                        splash_text.as_deref(),
                        uri,
                    );
                    webview.load_alternate_html(&page, uri, None);
                }
            }
        });
    })
}

#[cfg(test)]
#[path = "crash_tests.rs"]
mod tests;
