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
//! The splash shows the app's own `splash_path` page, over [`SPLASH_SCHEME`],
//! when one is declared and readable; otherwise it falls back to the hub's
//! own bundled page — see [`splash_style`] and [`resolve_splash_source`].

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use tauri::{
    http::{header, Response, StatusCode},
    webview::NewWindowResponse,
    Url, WebviewUrl, WebviewWindowBuilder,
};

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
///
/// [`SPLASH_SCHEME`] is `Internal` only while `app_origin` is still `None` —
/// the splash page itself may need to load its own `img`/`link` requests
/// against its snapshot root before hand-over. Once `publish_app_origin` has
/// filled the slot, the same target falls through to the final `match` like
/// any other non-`http(s)` scheme and is `Blocked`: the running app has no
/// legitimate reason to reach its own splash snapshot a second time, and
/// giving the scheme a standing exemption after hand-over would be a second,
/// permanent way to read that root instead of the one the splash window uses
/// once. (Settled in this plan's overview, "The scheme's reach is bounded by
/// navigation policy, not by unregistering it.")
pub fn classify_navigation(app_origin: Option<&Url>, target: &Url) -> NavigationTarget {
    if is_bundled_asset_origin(target) {
        return NavigationTarget::Internal;
    }
    if app_origin.is_none() && target.scheme() == SPLASH_SCHEME {
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

/// The script that dresses the hub's own fallback splash page in the app's
/// colours — used only on [`SplashSource::Fallback`], never alongside the
/// app's own `splash_path` page.
///
/// **What the hub honours, and where.** `splash_bg` and `splash_text` are
/// honoured on the fallback page only: it reads them as CSS variables, so an
/// app that has not (or could not) supply its own page still gets its own
/// palette and its own name. An app that declares a working `splash_path`
/// gets that page verbatim, over [`SPLASH_SCHEME`] and confined to the
/// snapshot root (`register_splash_scheme`, `resolve_splash_source`) — it
/// styles itself, inline, since CONTRACT.md documents it as one
/// self-contained file.
///
/// The fallback stays reachable for a `splash_path` that is undeclared,
/// missing, unreadable, or resolves outside the snapshot root — the family
/// rule for a value a host cannot honour: fall back to the nearest thing, and
/// say so (`main::serve`'s warning).
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

/// The scheme an app's real `splash_path` is served over (plan 025). Recognised
/// by [`classify_navigation`] once wired there (step 4).
pub const SPLASH_SCHEME: &str = "tfsapp-splash";

/// The CSP on every response [`register_splash_scheme`] serves.
///
/// CONTRACT.md §4 documents the sidecar's own default (`default-src 'self'`,
/// scoped to that origin's own assets) — this is not that policy loosened to
/// a new origin, it is stricter, because the case is narrower: `splash_path`
/// is documented as one self-contained file, inline CSS/JS only, no external
/// assets (CONTRACT.md line 196), and it runs before the sidecar exists, so
/// it has no API or session to call. `default-src 'none'` starts from
/// nothing; `style-src`/`script-src 'unsafe-inline'` admit exactly the inline
/// CSS/JS the format allows (same containment argument as §4's own
/// `'unsafe-inline'` — it cannot reach the network or read another origin
/// with everything else at `'none'`); `img-src data:` admits inlined images,
/// the only kind a single self-contained file can carry. `base-uri`,
/// `form-action` and `frame-ancestors` are pinned to `'none'`/refused rather
/// than left to `default-src`'s fallback, so a page that tries to reach past
/// its own markup fails visibly instead of silently doing nothing.
const SPLASH_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; \
     script-src 'unsafe-inline'; img-src data:; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

/// Resolve `request_path` (a URI scheme request's raw, `/`-prefixed path)
/// against `canonical_root`, refusing anything that would land outside it once
/// symlinks and `..` segments are resolved — `canonical_root.join` never sees
/// a leading `/` (all of them are trimmed first), so it can never fall into
/// `Path::join`'s own absolute-path-replaces-base behaviour either.
///
/// A missing file and an escape attempt both fall through to `None`: the
/// caller has nothing more specific to say about either, and saying more
/// would tell a hostile page which one it was.
fn resolve_within(canonical_root: &Path, request_path: &str) -> Option<PathBuf> {
    let relative = request_path.trim_start_matches('/');
    let resolved = std::fs::canonicalize(canonical_root.join(relative)).ok()?;
    (resolved == canonical_root || resolved.starts_with(canonical_root)).then_some(resolved)
}

/// Register [`SPLASH_SCHEME`] on `builder`, read-only and scoped to
/// `snapshot_root` — this process's installed snapshot or live project
/// directory (`LaunchSpec::app_dir`), resolved well before `Builder` exists
/// (`main::prepare`, called from `main::open_window` ahead of
/// `tauri::Builder::default()`). One hub process serves one app, so the
/// scheme closes over one root for its whole life and can never reach another
/// app's tree — by construction, not by a check bolted on after the fact.
///
/// `snapshot_root` failing to canonicalise makes every request refused rather
/// than panicking a handler that runs for the rest of the process's life;
/// every caller has already confirmed it is a directory (`open::resolve`,
/// `dev::resolve`), so this is a defensive fallback, not an expected path.
///
/// `#[allow(dead_code)]`: not called until `main::open_window` wires it into
/// the splash window's creation (plan 025 step 2) — this step only proves the
/// scheme safe in isolation.
///
/// Every response — success or 404 alike — carries [`SPLASH_CSP`]. There is
/// no HTTP request here for an app-set header to override (CONTRACT.md §4's
/// override rule is specific to the sidecar's own responses), and no author
/// gets a chance to set one either, so the host's policy is the only one a
/// splash page ever gets.
#[allow(dead_code)]
pub fn register_splash_scheme<R: tauri::Runtime>(
    builder: tauri::Builder<R>,
    snapshot_root: &Path,
) -> tauri::Builder<R> {
    let canonical_root = std::fs::canonicalize(snapshot_root).ok();
    builder.register_uri_scheme_protocol(SPLASH_SCHEME, move |_ctx, request| {
        canonical_root
            .as_deref()
            .and_then(|root| resolve_within(root, request.uri().path()))
            .and_then(|path| std::fs::read(path).ok())
            .map(|bytes| {
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .header(header::CONTENT_SECURITY_POLICY, SPLASH_CSP)
                    .body(bytes)
                    .expect("a valid response")
            })
            .unwrap_or_else(|| {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::CONTENT_SECURITY_POLICY, SPLASH_CSP)
                    .body(Vec::new())
                    .expect("a valid empty response")
            })
    })
}

/// Where the splash window should point: the app's own `splash_path`, or the
/// hub's bundled fallback when there is none to honour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplashSource {
    /// `splash_path` was declared, resolves inside the snapshot root, and was
    /// confirmed openable — this is the URL to navigate to, over
    /// [`SPLASH_SCHEME`].
    App(Url),
    /// No `splash_path` declared, or one that could not be honoured (missing,
    /// unreadable, or outside the snapshot root). [`create_splash_window`]
    /// reads this the same way either way; telling the two apart for the
    /// warning is [`resolve_splash_source`]'s caller's job (`main::serve`),
    /// since only it also has `manifest.splash_path` to compare against.
    Fallback,
}

impl SplashSource {
    fn webview_url(&self) -> WebviewUrl {
        match self {
            SplashSource::App(url) => WebviewUrl::External(url.clone()),
            SplashSource::Fallback => WebviewUrl::App("index.html".into()),
        }
    }
}

/// Decide [`SplashSource`] for `splash_path` against `snapshot_root` — the
/// same root [`register_splash_scheme`] is confined to, so a page this
/// resolves to is always one the running scheme can actually serve.
///
/// Reuses [`resolve_within`] for the confinement check, on the *undecoded*
/// `splash_path` string itself (prefixed with `/` to match a scheme request's
/// shape) — one path, checked once here and re-resolved once more per request
/// by the scheme's own handler, always in agreement because both go through
/// the same function. `splash_path` reaching this function unencoded is why
/// the scheme never percent-decodes a request path either (see
/// `resolve_within`): a name with characters that would need it is already
/// outside what CONTRACT.md documents `splash_path` as (one self-contained
/// file, ordinary path).
///
/// Openability is confirmed with `File::open` rather than reading the whole
/// file — existence and permission are what matter here, not content; the
/// scheme's handler reads the real bytes when the webview actually requests
/// it.
pub fn resolve_splash_source(snapshot_root: &Path, splash_path: Option<&str>) -> SplashSource {
    let Some(splash_path) = splash_path else {
        return SplashSource::Fallback;
    };
    let Ok(canonical_root) = std::fs::canonicalize(snapshot_root) else {
        return SplashSource::Fallback;
    };
    let request_path = format!("/{splash_path}");
    let Some(resolved) = resolve_within(&canonical_root, &request_path) else {
        return SplashSource::Fallback;
    };
    if std::fs::File::open(&resolved).is_err() {
        return SplashSource::Fallback;
    }
    match Url::parse(&format!("{SPLASH_SCHEME}://localhost{request_path}")) {
        Ok(url) => SplashSource::App(url),
        Err(_) => SplashSource::Fallback,
    }
}

/// The cold-start window: the app's own `splash_path` when [`SplashSource`]
/// resolved one, the hub's bundled page on the webview's own origin
/// otherwise.
///
/// `WebviewUrl::App` rather than the backend URL for the fallback, because
/// there is no backend yet — pointing a window at a port nothing is
/// listening on is how a launch shows a connection error instead of a
/// spinner. The launch navigates this same window once `/healthz` answers,
/// which is also why it carries the app window's title and sizing already:
/// nothing should visibly jump at the hand-over.
///
/// `fallback_script` — [`splash_style`]'s output — is only attached on the
/// fallback path: it dresses the hub's own page in the app's colours and
/// name, which means nothing to a page the app authored itself.
pub fn create_splash_window<R: tauri::Runtime>(
    app: &impl tauri::Manager<R>,
    title: &str,
    fallback_script: &str,
    app_origin: &AppOriginSlot,
    splash_source: &SplashSource,
) -> tauri::Result<tauri::WebviewWindow<R>> {
    let mut builder =
        WebviewWindowBuilder::new(app, next_window_label(app), splash_source.webview_url());
    if *splash_source == SplashSource::Fallback {
        builder = builder.initialization_script(fallback_script);
    }
    with_window_policy(builder, app_origin.clone())
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
