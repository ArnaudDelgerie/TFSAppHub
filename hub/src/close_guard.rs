//! The close-guard state model: one in-memory registry of "this window's
//! document has unsaved work" and "this app instance has vulnerable backend
//! work" markers, plus the close-decision bookkeeping that decides whether a
//! window-close needs to ask a person first.
//!
//! This module is deliberately free of Tauri, GTK and HTTP: it is the part of
//! plan 055 whose *concurrency* decisions have to be testable without a
//! graphical runtime, so everything it owns is plain in-memory state behind one
//! mutex. The transports — Tauri's IPC channel for the webview, the loopback
//! bridge for PHP — sit on top of it in step 2, and `lifecycle` turns the
//! decisions into native confirmations in step 3.
//!
//! **Two namespaces, and nothing can cross them.** Frontend guards belong to
//! one document incarnation in one window; backend guards belong to this
//! running app instance. The maps are separate by construction, so no request
//! from either transport can remove the other side's protection — and an app
//! cannot even *name* another app's state, because a bridge holds one process's
//! state and the IPC commands derive the window from Tauri's caller, never
//! from a supplied label.
//!
//! **The document incarnation token, and why it is host-generated.** A guard
//! belongs to a *document*, not a window label: a label survives reloads and is
//! even reusable after a window is destroyed, and a guard that outlived its
//! document (or could be touched by the previous document's late, in-flight
//! calls) would be worse than no guard at all — it could erase a successor's
//! protection. So every committed main-frame load rotates an opaque
//! [`DocumentContext`] ([`CloseGuardState::rotate_context`], wired to Tauri's
//! `on_page_load` `Started` event — `LoadEvent::Committed` in wry/WebKitGTK,
//! which fires when the new document is created, before its own scripts can
//! run). The page fetches the current token
//! ([`CloseGuardState::context`]) and presents it with every register/remove
//! call; a call presenting anything but the window's *current* token is a stale
//! call from a previous document and is refused — it cannot touch a successor's
//! guards.
//!
//! Guards are erased at the *fetch*, not at the load-finished callback:
//! [`CloseGuardState::context`] drops every guard registered under an older
//! incarnation of the calling window, which is exactly "the previous document
//! is gone" — and the successor's own guards, registered under the token it
//! just fetched, are never erased by it. A load that never commits (a cancelled
//! or blocked navigation) never rotates and never erases; a same-document
//! navigation (`pushState`) does not commit and keeps the guards; a failed
//! load or a dead renderer leaves the old incarnation's guards in place until
//! the window is destroyed — the conservative policy, because a close warning
//! the user can dismiss costs nothing and a silently-erased guard is data loss.
//!
//! **Bounded, explicit, no silent eviction.** Guard IDs are app-chosen
//! (`1..=128` bytes of UTF-8, non-empty — `editor:<document-id>`,
//! `export:<job-id>`), idempotent per owner, and capped per namespace
//! ([`MAX_FRONTEND_GUARDS_PER_WINDOW`], [`MAX_BACKEND_GUARDS`]). Exhaustion is
//! an explicit [`GuardError::TooManyGuards`], never an eviction of someone
//! else's active guard. There is no global clear operation, and nothing is
//! persisted or exported.
//!
//! **Shutdown commitment is one-way.** [`CloseGuardState::commit_shutdown`] is
//! called once teardown has begun; after it, every registration or removal
//! answers [`GuardError::Closing`] (an explicit failure, so a job starting
//! during teardown learns the truth rather than installing a guard nobody will
//! see) and any pending close decision is invalidated — a termination signal
//! tears the app down without waiting on a dialog.

// The observability helpers below (`frontend_guards`, `backend_guards`,
// `revision`, `is_closing`) are read by tests and by step 4's validation
// tooling rather than by the production call sites; they stay public so the
// state can be inspected without a graphical runtime.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

/// A guard ID's maximum size, in bytes of UTF-8. Guards are identifiers, not
/// messages: the confirmation dialog is host-authored, so there is no text
/// field to grow.
pub const MAX_GUARD_ID_BYTES: usize = 128;

/// How many frontend guards one window can hold, counting every incarnation's
/// leftovers (at most the current one and one unreaped previous one — the fetch
/// erases the rest). Exhaustion is [`GuardError::TooManyGuards`], never
/// eviction.
pub const MAX_FRONTEND_GUARDS_PER_WINDOW: usize = 16;

/// How many backend guards this app instance can hold.
pub const MAX_BACKEND_GUARDS: usize = 16;

/// A fresh opaque document token. Random 256-bit hex — generated by the host,
/// never the page, so a previous document cannot *guess* its successor's token
/// and cannot forge a registration for it. The counter fallback keeps the API
/// infallible on the practical impossibility of the system RNG refusing: it is
/// still unique per process, only no longer unguessable.
fn fresh_token(counter: &AtomicU64) -> String {
    match tfsapp_core::app_secret::random_secret_hex() {
        Ok(hex) => hex,
        Err(_) => format!("d{}", counter.fetch_add(1, Ordering::Relaxed)),
    }
}

/// The host-controlled identity of one document incarnation in one window.
///
/// Opaque to the app: its only meaning is "present this back with every
/// register/remove call, and the hub decides whether it is still current".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocumentContext(String);

impl DocumentContext {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a register or remove call was refused. The token strings are the
/// contract's stable machine-readable error codes, shared by the IPC commands
/// and the bridge routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardError {
    /// Empty, or beyond [`MAX_GUARD_ID_BYTES`].
    InvalidId,
    /// The namespace's cap is reached; an existing guard is never evicted.
    TooManyGuards,
    /// The presented [`DocumentContext`] is not the calling window's current
    /// one: a stale call from a previous document incarnation.
    StaleDocument,
    /// Shutdown has committed; new guard operations are refused.
    Closing,
}

impl GuardError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId => "invalid_id",
            Self::TooManyGuards => "too_many_guards",
            Self::StaleDocument => "stale_document",
            Self::Closing => "closing",
        }
    }
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.code())
    }
}

/// One window's frontend half: the current incarnation's token, plus the
/// guards of every incarnation that has not been erased yet — the current one
/// and, between a commit and the successor's first context fetch, the previous
/// one. Keyed by the context the guards were registered under, so the fetch
/// can erase exactly the dead incarnations' guards and nothing else.
#[derive(Debug, Default)]
struct WindowEntry {
    context: Option<DocumentContext>,
    guards: BTreeMap<DocumentContext, BTreeSet<String>>,
}

/// The one close decision that may be in flight. One at a time, across every
/// window of this app: repeated clicks and near-simultaneous closes must not
/// stack dialogs or spawn multiple teardowns, and a second close can only
/// begin once this one has resolved (confirmed, cancelled or invalidated).
#[derive(Debug)]
struct PendingDecision {
    /// The one-time key a dialog callback presents when it answers, so a
    /// stale or duplicate callback cannot authorise anything.
    token: String,
    /// The window whose close the decision covers.
    window: String,
    /// The frontend guard IDs the displayed warning covered.
    frontend: BTreeSet<String>,
    /// Whether the warning covered the backend guards too — true only when
    /// this close would stop the shared backend.
    backend_relevant: bool,
    /// The backend guard IDs the displayed warning covered, when relevant.
    backend: BTreeSet<String>,
    /// Set when the decision's window was destroyed while the decision was
    /// open. `drop_window` is the one destroy signal this model has, and a
    /// destroyed window's callback must not authorise anything — not even
    /// the close it was asked about, which can no longer happen.
    destroyed: bool,
}

#[derive(Debug, Default)]
struct Inner {
    windows: BTreeMap<String, WindowEntry>,
    backend: BTreeSet<String>,
    pending: Option<PendingDecision>,
    closing: bool,
    /// Bumped on every mutation, so callers can log *that* guard state moved
    /// without logging what it holds — the guard IDs themselves stay private.
    revision: u64,
}

/// The shared registry, managed once per launch before any window exists and
/// before PHP starts, and reached by the IPC commands, the bridge threads and
/// `lifecycle` alike. Cheap to clone behind an `Arc` at the transport seams.
#[derive(Debug, Default)]
pub struct CloseGuardState {
    inner: Mutex<Inner>,
    counter: AtomicU64,
}

/// Why the closing window's guards, as they are *right now*, do or do not
/// require asking a person — the decision `lifecycle` acts on in step 3.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseFlow {
    /// Nothing to protect: the close proceeds with no dialog.
    Allow,
    /// Exactly these guards are at stake, and one native confirmation is
    /// required before the close may proceed. `token` is the one-time key the
    /// dialog's answer must present back.
    Confirm {
        token: String,
        frontend: BTreeSet<String>,
        backend: BTreeSet<String>,
    },
    /// Another window's close decision is already in flight. Serialized on
    /// purpose: a second dialog stacked on the first is how two closes both
    /// conclude they are the last one. The caller vetoes this close (the
    /// window stays open, guards untouched) — the person can close it again
    /// once the pending dialog has resolved.
    Busy,
}

/// What applying a dialog's answer did. Only `Approved` authorises a close,
/// and it does so once — the pending decision is consumed by every variant
/// that names an outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The close may proceed, for the window the decision was opened for. The
    /// caller learns again whether this close stops the shared backend, so a
    /// topology change between dialog and answer is honoured rather than
    /// assumed.
    Approved { stop_backend: bool },
    /// Refused (or the dialog failed/dismissed). Guards are kept; the window
    /// stays open; nothing else happens.
    Cancelled,
    /// No pending decision matches the presented token: a stale or repeated
    /// callback, which never grants permission to close anything.
    Stale,
    /// The decision's window was destroyed while its dialog was open. The
    /// close it was asking about can no longer happen; nothing is authorised.
    Invalidated,
    /// The guards at stake changed since the dialog was shown — a new
    /// relevant guard appeared, or this close became the backend-stopping
    /// one. The decision is consumed; the displayed warning cannot be applied
    /// to a situation it did not describe, and the caller must open a fresh
    /// one (or close cleanly, if nothing needs protection any more).
    NeedsFreshDecision,
}

impl CloseGuardState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Not poisoned by any code in this tree: the critical sections are
        // panic-free maps and sets.
        self.inner
            .lock()
            .expect("the close-guard state mutex is not poisoned")
    }

    /// Assign a fresh document context to `window`: the committed-load hook,
    /// called by the window integration when a new main-frame document is
    /// created. Previous incarnations' guards stay put until the successor
    /// fetches its context — a cancelled navigation commits nothing and erases
    /// nothing.
    pub fn rotate_context(&self, window: &str) -> DocumentContext {
        let mut inner = self.lock();
        let entry = inner.windows.entry(window.to_string()).or_default();
        let context = DocumentContext(fresh_token(&self.counter));
        entry.context = Some(context.clone());
        inner.revision += 1;
        context
    }

    /// The calling window's current document context, fetched once per
    /// document at startup. This is the *only* place a previous incarnation's
    /// guards are erased: a page that just asked for its context is, by
    /// definition, the new document, and every older incarnation of this
    /// window is dead. The guards the caller registers after this fetch are
    /// held under the context it receives, so the fetch itself can never
    /// erase them.
    pub fn context(&self, window: &str) -> DocumentContext {
        let mut inner = self.lock();
        let entry = inner.windows.entry(window.to_string()).or_default();
        if entry.context.is_none() {
            entry.context = Some(DocumentContext(fresh_token(&self.counter)));
        }
        let context = entry.context.clone().expect("a context was just ensured");
        let stale: Vec<DocumentContext> = entry
            .guards
            .keys()
            .filter(|registered| **registered != context)
            .cloned()
            .collect();
        if !stale.is_empty() {
            for dead in stale {
                entry.guards.remove(&dead);
            }
            inner.revision += 1;
        }
        context
    }

    /// Register (or idempotently re-register) a frontend guard for the
    /// document presenting `context` in `window`. `window` comes from the IPC
    /// caller, never from the page; `context` is what the page presents back
    /// from [`CloseGuardState::context`].
    pub fn frontend_register(
        &self,
        window: &str,
        context: &DocumentContext,
        id: &str,
    ) -> Result<(), GuardError> {
        let mut inner = self.lock();
        if inner.closing {
            return Err(GuardError::Closing);
        }
        validate_guard_id(id)?;
        let entry = inner
            .windows
            .get_mut(window)
            .ok_or(GuardError::StaleDocument)?;
        if entry.context.as_ref() != Some(context) {
            return Err(GuardError::StaleDocument);
        }
        if !entry
            .guards
            .get(context)
            .is_some_and(|ids| ids.contains(id))
            && total_frontend_guards(entry) >= MAX_FRONTEND_GUARDS_PER_WINDOW
        {
            return Err(GuardError::TooManyGuards);
        }
        if entry
            .guards
            .entry(context.clone())
            .or_default()
            .insert(id.to_string())
        {
            inner.revision += 1;
        }
        Ok(())
    }

    /// Remove a frontend guard. Removing an absent ID is a success — a task
    /// that finished before its removal call, or an app clearing state it
    /// never set, has nothing to recover from.
    pub fn frontend_remove(
        &self,
        window: &str,
        context: &DocumentContext,
        id: &str,
    ) -> Result<(), GuardError> {
        let mut inner = self.lock();
        if inner.closing {
            return Err(GuardError::Closing);
        }
        let entry = inner
            .windows
            .get_mut(window)
            .ok_or(GuardError::StaleDocument)?;
        if entry.context.as_ref() != Some(context) {
            return Err(GuardError::StaleDocument);
        }
        if let Some(current) = entry.guards.get_mut(context) {
            if current.remove(id) {
                inner.revision += 1;
            }
        }
        Ok(())
    }

    /// Register (or idempotently re-register) a backend guard for this app
    /// instance. Reached only through the authenticated bridge; the IPC side
    /// has no route to this half.
    pub fn backend_register(&self, id: &str) -> Result<(), GuardError> {
        let mut inner = self.lock();
        if inner.closing {
            return Err(GuardError::Closing);
        }
        validate_guard_id(id)?;
        if !inner.backend.contains(id) && inner.backend.len() >= MAX_BACKEND_GUARDS {
            return Err(GuardError::TooManyGuards);
        }
        if inner.backend.insert(id.to_string()) {
            inner.revision += 1;
        }
        Ok(())
    }

    /// Remove a backend guard, harmlessly when absent.
    pub fn backend_remove(&self, id: &str) -> Result<(), GuardError> {
        let mut inner = self.lock();
        if inner.closing {
            return Err(GuardError::Closing);
        }
        if inner.backend.remove(id) {
            inner.revision += 1;
        }
        Ok(())
    }

    /// A window was destroyed: every guard it holds goes with it, whatever
    /// incarnation registered them. A later window reusing the same label
    /// starts from nothing — and its fresh context means the old window's
    /// late, in-flight calls cannot touch it (see [`DocumentContext`]).
    pub fn drop_window(&self, window: &str) {
        let mut inner = self.lock();
        if inner.windows.remove(window).is_some() {
            inner.revision += 1;
        }
        // The pending decision is kept rather than consumed, so its dialog's
        // late answer gets the honest `Invalidated` rather than a `Stale` that
        // would read like a bug in the callback plumbing.
        if let Some(pending) = inner.pending.as_mut() {
            if pending.window == window {
                pending.destroyed = true;
                inner.revision += 1;
            }
        }
    }

    /// Every frontend guard `window` still holds, across every incarnation.
    /// Conservative on purpose: a previous incarnation whose successor has
    /// not fetched yet is still a close worth warning about.
    // Test and validation eyes only: no production call site reads the
    // registry's contents or counters directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn frontend_guards(&self, window: &str) -> BTreeSet<String> {
        frontend_guards_of(&self.lock(), window)
    }

    /// Every backend guard this app instance holds.
    // Test and validation eyes only: no production call site reads the
    // registry's contents or counters directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn backend_guards(&self) -> BTreeSet<String> {
        self.lock().backend.clone()
    }

    /// Decide whether closing `window` needs a person's confirmation.
    /// `backend_would_stop` is the caller's topology answer — true only when
    /// this close takes the last window and so stops the shared backend; the
    /// backend's guards never make a surviving-window close prompt.
    pub fn begin_close(&self, window: &str, backend_would_stop: bool) -> CloseFlow {
        let mut inner = self.lock();
        // Teardown already committed: no dialog may stand in its way, and the
        // close itself is about to be overtaken by `destroy_windows` anyway.
        if inner.closing {
            return CloseFlow::Allow;
        }
        if inner.pending.is_some() {
            return CloseFlow::Busy;
        }
        let frontend = frontend_guards_of(&inner, window);
        let backend = if backend_would_stop {
            inner.backend.clone()
        } else {
            BTreeSet::default()
        };
        if frontend.is_empty() && backend.is_empty() {
            return CloseFlow::Allow;
        }
        let token = fresh_token(&self.counter);
        inner.pending = Some(PendingDecision {
            token: token.clone(),
            window: window.to_string(),
            frontend: frontend.clone(),
            backend_relevant: backend_would_stop,
            backend: backend.clone(),
            destroyed: false,
        });
        CloseFlow::Confirm {
            token,
            frontend,
            backend,
        }
    }

    /// Apply a dialog's answer to the pending decision it names.
    /// `backend_would_stop_now` is the topology *at answer time* — the caller
    /// rechecks which window is last, because a nearly-simultaneous close of
    /// another window may have changed it.
    pub fn resolve_close(
        &self,
        token: &str,
        approved: bool,
        backend_would_stop_now: bool,
    ) -> Resolution {
        let mut inner = self.lock();
        let Some(pending) = inner.pending.take_if(|pending| pending.token == token) else {
            return Resolution::Stale;
        };
        // The window this decision was opened for is gone: its close can no
        // longer happen, and a destroyed window's callback certainly cannot
        // authorise closing anything else.
        if pending.destroyed {
            return Resolution::Invalidated;
        }
        if !approved {
            // Guards are kept, the pending slot is free, nothing else moves.
            return Resolution::Cancelled;
        }
        // Recheck, in the same critical section, that the warning still
        // describes what is at stake. A guard that *disappeared* during the
        // dialog only means the warning over-warned; a guard that *appeared*
        // means the confirmation was never given for it. Comparing sets
        // rather than revisions keeps an idempotent re-registration from
        // endlessly invalidating a decision.
        let frontend_now = frontend_guards_of(&inner, &pending.window);
        if !pending.frontend.is_superset(&frontend_now) {
            return Resolution::NeedsFreshDecision;
        }
        if backend_would_stop_now {
            if !pending.backend_relevant || !pending.backend.is_superset(&inner.backend) {
                return Resolution::NeedsFreshDecision;
            }
            return Resolution::Approved { stop_backend: true };
        }
        // The backend no longer stops on this close (another window
        // appeared, or the pending close was for a window that was not the
        // last one to begin with): the approval covers closing this window,
        // and the backend keeps running.
        Resolution::Approved {
            stop_backend: false,
        }
    }

    /// Commit shutdown: one-way, set by `lifecycle` before teardown begins.
    /// Every later guard operation answers [`GuardError::Closing`], and any
    /// pending decision is invalidated — a signal-initiated shutdown does not
    /// wait on a dialog, and a dialog answering afterwards is stale by
    /// construction.
    pub fn commit_shutdown(&self) {
        let mut inner = self.lock();
        inner.closing = true;
        if inner.pending.take().is_some() {
            inner.revision += 1;
        }
        inner.revision += 1;
    }

    /// Whether shutdown has committed. Teardown paths use this to know that
    /// no further confirmation may be opened.
    // Test and validation eyes only: no production call site reads the
    // registry's contents or counters directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_closing(&self) -> bool {
        self.lock().closing
    }

    /// The mutation revision — observability and tests. It counts moves, not
    /// contents: a caller can log "guard state changed" without logging what
    /// it holds.
    // Test and validation eyes only: no production call site reads the
    // registry's contents or counters directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn revision(&self) -> u64 {
        self.lock().revision
    }
}

fn validate_guard_id(id: &str) -> Result<(), GuardError> {
    if id.is_empty() || id.len() > MAX_GUARD_ID_BYTES {
        return Err(GuardError::InvalidId);
    }
    Ok(())
}

/// Every guard `window` still holds, across every incarnation — the shared
/// core of the public read and of both close-decision paths.
fn frontend_guards_of(inner: &Inner, window: &str) -> BTreeSet<String> {
    inner
        .windows
        .get(window)
        .map(|entry| entry.guards.values().flatten().cloned().collect())
        .unwrap_or_default()
}

fn total_frontend_guards(entry: &WindowEntry) -> usize {
    entry.guards.values().map(|ids| ids.len()).sum()
}

// --- the IPC surface --------------------------------------------------------
//
// The webview reaches the frontend namespace through three Tauri commands,
// granted at runtime only when the manifest declares `actions.close_guard.ipc`
// (`window.rs`'s grant table; the static capability grants nothing). The
// commands are a thin layer: they resolve the calling window the same way
// `secrets.rs` does — from Tauri's caller, never from a supplied label — and
// delegate every decision to the state model above. The core helpers are
// generic over the runtime so the mock runtime can exercise them; the
// `#[tauri::command]` wrappers pin the production runtime, exactly like the
// secret commands.

/// The managed shape: one state per launch, shared by the IPC commands, the
/// bridge's request threads and `lifecycle`. Created in `open_window` before
/// any window exists, managed into Tauri before the splash is built, and
/// handed to `sidecar::start` before PHP is spawned.
pub type SharedCloseGuards = std::sync::Arc<CloseGuardState>;

/// The error the commands answer when no state backs the calling window —
/// which cannot happen in a launch that built its windows after managing the
/// state, and is the honest answer everywhere else.
const NO_STATE: &str = "unavailable";

/// `close_guard_context`'s answer: the calling window's current document
/// context, opaque to the app.
#[derive(Debug, serde::Serialize)]
pub struct CloseGuardDocument {
    pub context: String,
}

use tauri::Manager;

/// The state the calling window's app was launched with, or `None` when this
/// process never managed one — the "unavailable to app code" rule of the
/// contract rather than a panic.
fn state_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
) -> Option<tauri::State<'_, SharedCloseGuards>> {
    window.app_handle().try_state::<SharedCloseGuards>()
}

/// The current document context of the calling window — the one command the
/// page calls before any other in this group, and the only place the previous
/// document's guards are erased (see [`CloseGuardState::context`]).
pub fn context_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
) -> Result<String, &'static str> {
    let state = state_for_window(window).ok_or(NO_STATE)?;
    Ok(state.context(window.label()).as_str().to_string())
}

/// Register a frontend guard for the document presenting `context` in the
/// calling window.
pub fn register_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    context: &str,
    id: &str,
) -> Result<(), &'static str> {
    let state = state_for_window(window).ok_or(NO_STATE)?;
    state
        .frontend_register(window.label(), &DocumentContext(context.to_string()), id)
        .map_err(|error| error.code())
}

/// Remove a frontend guard, harmlessly when absent.
pub fn remove_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    context: &str,
    id: &str,
) -> Result<(), &'static str> {
    let state = state_for_window(window).ok_or(NO_STATE)?;
    state
        .frontend_remove(window.label(), &DocumentContext(context.to_string()), id)
        .map_err(|error| error.code())
}

#[tauri::command]
pub fn close_guard_context(window: tauri::Window) -> Result<CloseGuardDocument, &'static str> {
    context_for_window(&window).map(|context| CloseGuardDocument { context })
}

#[tauri::command]
pub fn close_guard_register(
    window: tauri::Window,
    context: String,
    id: String,
) -> Result<(), &'static str> {
    register_for_window(&window, &context, &id)
}

#[tauri::command]
pub fn close_guard_remove(
    window: tauri::Window,
    context: String,
    id: String,
) -> Result<(), &'static str> {
    remove_for_window(&window, &context, &id)
}

#[cfg(test)]
#[path = "close_guard_tests.rs"]
mod tests;
