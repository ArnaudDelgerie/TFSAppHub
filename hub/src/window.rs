//! The app's windows, and the policy every one of them enforces.
//!
//! Ported from the station's `window.rs`. The navigation policy is CONTRACT.md
//! §4 and comes across unchanged — a link to somewhere else opens in the user's
//! browser rather than replacing the app inside its own frame, a `javascript:`
//! or `file:` target is refused outright, and no in-app popup is ever created.
//! §4 is canonical for both hosts and the hub cannot deliver it more weakly, so
//! the only sound relationship between the two files is sameness.
//!
//! **Two windows, in this order, and the order is the whole point.** The splash
//! is created first, before the sidecar is spawned and long before `/healthz`
//! answers, and the *same* window is later navigated to the backend. Opening the
//! app window only once the backend was healthy would leave the screen empty for
//! the length of a cold start — which, with the container rebuilt from scratch
//! on every launch, is tens of seconds of a user wondering whether their double
//! click registered. The station learned this and inverted its own order for it;
//! the hub inherits the conclusion rather than rediscovering it.
//!
//! The splash is the hub's own bundled page, not the app's `splash_path`. That
//! is a real gap and it is stated where it is met — see [`splash_style`].

use std::sync::{Arc, OnceLock};

use tauri::{webview::NewWindowResponse, Url, WebviewUrl, WebviewWindowBuilder};

/// Where a navigation or new-window request should end up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationTarget {
    /// The running backend's own origin, or the webview's bundled-asset one:
    /// let it through.
    Internal,
    /// Another `http`/`https` origin: cancel it here and open it in the user's
    /// own browser, where they have their bookmarks, their sessions and their
    /// address bar.
    ExternalWeb,
    /// Anything else — `javascript:`, `file:`, `data:`, `blob:`, a custom
    /// scheme: cancel it and say so. Nothing is opened.
    Blocked,
}

/// The write-once carrier for the app's own origin.
///
/// The splash window is built before the backend's port is known, so the policy
/// closures it is given at construction cannot capture the origin as a value.
/// They capture this instead — cheap to clone, read fresh on every navigation —
/// and the launch fills it the moment the port is settled.
pub type AppOriginSlot = Arc<OnceLock<Url>>;

pub fn new_app_origin_slot() -> AppOriginSlot {
    Arc::new(OnceLock::new())
}

/// Publish `url` as the app's origin, if it parses and nothing has claimed the
/// slot yet. Idempotent by construction, so every call site that knows the
/// backend URL can just call it rather than tracking whether it already did.
pub fn publish_app_origin(slot: &AppOriginSlot, url: &str) {
    if let Ok(parsed) = Url::parse(url) {
        let _ = slot.set(parsed);
    }
}

/// The webview's own bundled-asset origin — the splash's, before any backend
/// exists. True independently of whether the app origin is known yet, which is
/// exactly what lets the splash page load at all.
fn is_bundled_asset_origin(url: &Url) -> bool {
    url.scheme() == "tauri"
        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
        || url.as_str() == "about:blank"
}

/// Scheme, host and port all matching. A scheme mismatch on the same host is
/// deliberately *not* the same origin, exactly as a browser would have it.
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Decide where `target` should go, given the app origin (`None` while the
/// splash has not learned the backend URL yet).
///
/// An unknown-origin `http`/`https` target is `ExternalWeb` rather than
/// `Internal`: before the origin is known the only navigation that can
/// legitimately happen is to the bundled assets, which the first branch already
/// covers regardless.
pub fn classify_navigation(app_origin: Option<&Url>, target: &Url) -> NavigationTarget {
    if is_bundled_asset_origin(target) {
        return NavigationTarget::Internal;
    }
    if let Some(origin) = app_origin {
        if same_origin(origin, target) {
            return NavigationTarget::Internal;
        }
    }
    match target.scheme() {
        "http" | "https" => NavigationTarget::ExternalWeb,
        _ => NavigationTarget::Blocked,
    }
}

/// Attach the shared policy to `builder`, so both windows enforce byte-for-byte
/// the same rules.
///
/// `on_new_window` denies even an `Internal` target — it just does so without
/// the browser side effect. The app has no in-app destination for a popup, so
/// "open link in new window" and `target="_blank"` resolve to the same two
/// outcomes as an ordinary navigation, minus a second window nobody asked for.
///
/// Tauri's native drag-drop handler is disabled: it intercepts the OS drop to
/// deliver an event to Rust, which also stops a dropped file from ever landing
/// on an `<input type="file">`, and JavaScript cannot repair that. The hub
/// registers no drag-drop handler and exposes none, so the interception would
/// buy nothing and break uploads.
fn with_window_policy<'a, R: tauri::Runtime, M: tauri::Manager<R>>(
    builder: WebviewWindowBuilder<'a, R, M>,
    app_origin: AppOriginSlot,
) -> WebviewWindowBuilder<'a, R, M> {
    let new_window_origin = app_origin.clone();
    builder
        .disable_drag_drop_handler()
        .on_navigation(
            move |target| match classify_navigation(app_origin.get(), target) {
                NavigationTarget::Internal => true,
                NavigationTarget::ExternalWeb => {
                    tfsapp_core::browser::open_in_system_browser(target);
                    false
                }
                NavigationTarget::Blocked => {
                    eprintln!("tfsapp-hub: refusing navigation to {target}");
                    false
                }
            },
        )
        .on_new_window(move |target, _features| {
            match classify_navigation(new_window_origin.get(), &target) {
                NavigationTarget::Internal => {}
                NavigationTarget::ExternalWeb => {
                    tfsapp_core::browser::open_in_system_browser(&target)
                }
                NavigationTarget::Blocked => {
                    eprintln!("tfsapp-hub: refusing new window to {target}")
                }
            }
            NewWindowResponse::Deny
        })
}

/// One `actions` group's runtime IPC identity: the capability name Tauri's ACL
/// uses internally, and the permission (declared in `permissions/<group>.toml`)
/// it grants when the app's manifest turns that group's `ipc` on.
pub struct ActionIpcGrant {
    capability_identifier: &'static str,
    permission: &'static str,
}

pub const SECRETS_IPC_GRANT: ActionIpcGrant = ActionIpcGrant {
    capability_identifier: "actions-secrets",
    permission: "allow-secrets",
};
pub const UPDATE_IPC_GRANT: ActionIpcGrant = ActionIpcGrant {
    capability_identifier: "actions-update",
    permission: "allow-update",
};

/// The table the launch walks, pairing each group's `ipc` flag with the grant it
/// activates. A third group is one entry here, not another `if` at the call
/// site — and the pairing is what keeps the groups independent, so one group's
/// grant can never imply another's.
pub type ActionIpcGrantEntry = (fn(&crate::manifest::ActionsConfig) -> bool, ActionIpcGrant);

pub const ACTION_IPC_GRANTS: &[ActionIpcGrantEntry] = &[
    (|actions| actions.secrets.ipc, SECRETS_IPC_GRANT),
    (|actions| actions.update.ipc, UPDATE_IPC_GRANT),
];

/// Build one group's runtime capability grant.
///
/// `main*` rather than a literal `main`, because a relaunch adds `main-2`,
/// `main-3`, … and a capability scoped to the first window alone would refuse
/// every window after it. `remote` covers the backend's own origin, which is
/// where the app's pages are actually served from — the port is not known when
/// this is built, hence the wildcard.
pub fn action_capability(grant: &ActionIpcGrant) -> tauri::ipc::CapabilityBuilder {
    tauri::ipc::CapabilityBuilder::new(grant.capability_identifier)
        .window("main*")
        .remote("http://127.0.0.1:*".to_string())
        .permission(grant.permission)
}

/// The script that dresses the hub's splash page in the app's own colours.
///
/// **What the hub honours, and what it does not.** `splash_bg` and `splash_text`
/// are honoured: the bundled page reads them as CSS variables, so an app's
/// splash carries its own palette and its own name. A whole custom
/// `splash_path` page is **not** — the station bakes that file into its frontend
/// bundle at build time, and the hub has no per-app build step to bake anything
/// into. The app's file is inside the installed snapshot, which is reachable
/// neither over HTTP (the sidecar is not up yet, and the file sits outside
/// `public/`) nor from the webview's own origin.
///
/// So the hub falls back to the nearest thing it can serve, and says so at
/// launch rather than leaving the author to notice their splash never appears —
/// the family rule for a value a host cannot honour. Serving the app's real
/// splash needs a custom URI scheme over the snapshot, which is its own plan.
pub fn splash_style(
    product_name: &str,
    splash_bg: Option<&str>,
    splash_text: Option<&str>,
) -> String {
    let mut script = format!(
        "document.addEventListener('DOMContentLoaded', function () {{
             var status = document.getElementById('splash-status');
             if (status) {{ status.textContent = {}; }}
         }});",
        serde_json::Value::from(format!("Starting {product_name}…"))
    );
    for (property, value) in [("--splash-bg", splash_bg), ("--splash-text", splash_text)] {
        if let Some(value) = value {
            script.push_str(&format!(
                "document.documentElement.style.setProperty({}, {});",
                serde_json::Value::from(property),
                serde_json::Value::from(value)
            ));
        }
    }
    script
}

/// The cold-start window: the hub's bundled page, on the webview's own origin.
///
/// `WebviewUrl::App` rather than the backend URL, because there is no backend
/// yet — pointing a window at a port nothing is listening on is how a launch
/// shows a connection error instead of a spinner. The launch navigates this same
/// window once `/healthz` answers, which is also why it carries the app window's
/// title and sizing already: nothing should visibly jump at the hand-over.
pub fn create_splash_window<R: tauri::Runtime>(
    app: &impl tauri::Manager<R>,
    title: &str,
    initialization_script: &str,
    app_origin: &AppOriginSlot,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    with_window_policy(
        WebviewWindowBuilder::new(
            app,
            next_window_label(app),
            WebviewUrl::App("index.html".into()),
        )
        .initialization_script(initialization_script),
        app_origin.clone(),
    )
    .title(title)
    .inner_size(1100.0, 760.0)
    .min_inner_size(800.0, 560.0)
    .build()
}

/// A further window on an already-running backend — what a second `open` of the
/// same app resolves to.
///
/// Publishes the origin before building: the policy has to know it before the
/// window's first navigation, not after.
pub fn create_app_window(
    app: &tauri::AppHandle,
    url: &str,
    title: &str,
    app_origin: &AppOriginSlot,
) -> tauri::Result<tauri::WebviewWindow> {
    publish_app_origin(app_origin, url);
    with_window_policy(
        WebviewWindowBuilder::new(
            app,
            next_window_label(app),
            WebviewUrl::External(url.parse().expect("a valid local backend URL")),
        ),
        app_origin.clone(),
    )
    .title(title)
    .inner_size(1100.0, 760.0)
    .min_inner_size(800.0, 560.0)
    .build()
}

/// The lowest-numbered free `main`/`main-N` label. Tauri forbids duplicates, and
/// a relaunch has to be able to add windows to an app already running.
pub fn next_window_label<R: tauri::Runtime>(app: &impl tauri::Manager<R>) -> String {
    next_window_label_among(|label| app.get_webview_window(label).is_some())
}

/// Pure core of [`next_window_label`], so the search — gap-filling included,
/// when a lower `main-N` has since closed — is testable without an `AppHandle`.
pub fn next_window_label_among(mut is_taken: impl FnMut(&str) -> bool) -> String {
    if !is_taken("main") {
        return "main".to_string();
    }
    let mut n = 2;
    loop {
        let label = format!("main-{n}");
        if !is_taken(&label) {
            return label;
        }
        n += 1;
    }
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
