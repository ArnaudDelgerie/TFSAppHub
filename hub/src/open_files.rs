//! The pending open-files request queue: the in-memory state that carries a
//! "Open with" or CLI file handoff to the app window that will consume it,
//! plus the thin IPC receiver the app's webview reads and acknowledges
//! through.
//!
//! **The queue is the source of truth; the event is a bell.** One invocation
//! — one `tfsapp-hub open <id> -- <file>...`, one desktop "Open with"
//! selection — creates exactly one request: an opaque id the hub generates
//! plus the ordered paths that invocation carried. Requests live here,
//! assigned to the window that will receive them, until the app acknowledges
//! them; the Tauri event ([`OPEN_FILES_EVENT`]) only tells that one window to
//! read, and carries no paths. A notification that fails to emit is
//! diagnosed and costs latency, nothing else — the next notification or the
//! receiver's own startup retrieves everything still pending.
//!
//! **Requests are replayable until acknowledged, never exactly-once.** A
//! reload re-exposes the same ids, so the app's acceptance must be idempotent
//! by request id before it acks; acking the same id twice succeeds (the
//! per-window tombstone below is what makes the second ack a success rather
//! than an error), and a request the window was never assigned is refused
//! with `unknown_request`. Nothing is persisted: the queue lives exactly as
//! long as this hub process does, and a window's unacknowledged requests
//! transfer to a surviving eligible window when theirs is destroyed.
//!
//! Like `close_guard.rs`, this module keeps the state model free of Tauri and
//! GTK so its concurrency and replay decisions are testable without a
//! graphical runtime: everything it owns is plain in-memory state behind one
//! mutex, and the transports — Tauri's IPC channel for the receiver, the
//! event for notification — sit on top of it. The window identity always
//! comes from Tauri's caller, never from an app-supplied label, so a window
//! can only ever read or acknowledge its own requests.
//!
//! **Bounds are finite and overflow is loud.** [`MAX_PENDING_REQUESTS`] per
//! app (one process serves one app) and [`MAX_PATHS_PER_REQUEST`] per
//! request; past either bound the enqueueing caller refuses the invocation
//! with a visible diagnostic and enqueues nothing. A queued request is never
//! evicted to make room.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tauri::Manager;

/// How many unacknowledged requests this app can hold at once, across every
/// window and the pre-launch pool. Overflow is an explicit refusal at the
/// enqueueing boundary, never an eviction of a queued request.
pub const MAX_PENDING_REQUESTS: usize = 64;

/// How many paths one request may carry. One "Open with" selection of a
/// whole directory's worth of files is the intended ceiling, not a challenge.
pub const MAX_PATHS_PER_REQUEST: usize = 64;

/// A request id's maximum size, in bytes of UTF-8 — the contract's
/// `invalid_id` boundary. Ids are host-generated well under it; the bound
/// exists so an app cannot be made to ack an unbounded string.
pub const MAX_REQUEST_ID_BYTES: usize = 128;

/// How many acknowledged ids one window remembers, so a repeated ack (the
/// reload-replay case the contract documents) succeeds rather than reading as
/// `unknown_request`. A very late duplicate ack past this horizon is
/// indistinguishable from a bug and says so.
const ACKNOWLEDGED_HISTORY: usize = MAX_PENDING_REQUESTS;

/// The notification event: emitted to the selected window only, carrying no
/// payload. The name is one `EventName`-legal string (alphanumerics plus
/// `-`, `/`, `:`, `_`).
pub const OPEN_FILES_EVENT: &str = "tfsapp://open-files-pending";

/// One pending request: the hub-generated opaque id, plus the ordered paths
/// the invocation carried. The order is the invocation's order — it is what
/// the app's own "restore session" logic consumes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PendingRequest {
    pub id: String,
    pub paths: Vec<String>,
    /// The enqueue order, never serialized — the receiver's wire shape is
    /// id and paths alone. It is what keeps a window's queue oldest-first
    /// when a destroyed window's requests merge into a survivor's.
    #[serde(skip)]
    pub sequence: u64,
}

/// Why a queue operation was refused. The two codes the receiver can meet
/// cross the IPC boundary verbatim (`invalid_id`, `unknown_request`); the
/// three overflow shapes belong to the enqueueing caller's diagnostic, which
/// is why they spell their condition out in words instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// Empty id, or beyond [`MAX_REQUEST_ID_BYTES`].
    InvalidId,
    /// The id names no request this window was ever assigned.
    UnknownRequest,
    /// An enqueue with no paths at all — a file handoff carries files.
    #[cfg_attr(not(test), allow(dead_code))]
    // Constructed by `enqueue`; step 3's CLI is its first production caller.
    EmptyRequest,
    /// The queue already holds [`MAX_PENDING_REQUESTS`] unacknowledged
    /// requests. Nothing was evicted and nothing was enqueued.
    #[cfg_attr(not(test), allow(dead_code))]
    // Constructed by `enqueue`; step 3's CLI is its first production caller.
    TooManyRequests,
    /// The batch carries more than [`MAX_PATHS_PER_REQUEST`] paths.
    #[cfg_attr(not(test), allow(dead_code))]
    // Constructed by `enqueue`; step 3's CLI is its first production caller.
    TooManyPaths,
}

impl QueueError {
    /// The contract's stable machine-readable code, for the errors the
    /// receiver can meet. The overflow shapes never cross IPC.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId => "invalid_id",
            Self::UnknownRequest => "unknown_request",
            Self::EmptyRequest => "empty_request",
            Self::TooManyRequests => "too_many_requests",
            Self::TooManyPaths => "too_many_paths",
        }
    }
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId | Self::UnknownRequest => write!(formatter, "{}", self.code()),
            Self::EmptyRequest => write!(
                formatter,
                "a file request must carry at least one path (CONTRACT.md §7)"
            ),
            Self::TooManyRequests => write!(
                formatter,
                "{} unacknowledged file requests are already pending; the app must \
                 acknowledge some before more are accepted (CONTRACT.md §7)",
                MAX_PENDING_REQUESTS
            ),
            Self::TooManyPaths => write!(
                formatter,
                "a file request may carry at most {} paths (CONTRACT.md §7)",
                MAX_PATHS_PER_REQUEST
            ),
        }
    }
}

/// A fresh opaque request id. Random hex — generated by the host, never the
/// app — with the same counter fallback `close_guard.rs` keeps for a system
/// RNG that refuses.
#[cfg_attr(not(test), allow(dead_code))] // Called by `enqueue`; step 3's CLI is its first production caller.
fn fresh_request_id(counter: &AtomicU64) -> String {
    match tfsapp_core::app_secret::random_secret_hex() {
        Ok(hex) => hex,
        Err(_) => format!("r{}", counter.fetch_add(1, Ordering::Relaxed)),
    }
}

/// Where one live-instance arrival landed: the new request's id, plus the
/// window that will read it — `None` when it waits in the pre-launch pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrivalPlacement {
    pub id: String,
    pub window: Option<String>,
}

/// One window's half of the queue: its unacknowledged requests, oldest first,
/// plus the bounded tombstones that keep a repeated ack a success.
#[derive(Debug, Default)]
struct WindowQueue {
    pending: VecDeque<PendingRequest>,
    acknowledged: VecDeque<String>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Requests waiting for the app's first eligible document — the arrivals
    /// that landed while the splash was still the only window. The launch
    /// claims them for the window it navigates to the backend.
    unassigned: VecDeque<PendingRequest>,
    /// The window the launch designated as the app's first document, set
    /// atomically with the pool drain by [`OpenFilesState::handoff_to_first_document`].
    /// After that transition an arrival with no eligible window lands on this
    /// window — never in a pool nothing will drain again (audit 019, finding 1).
    first_document: Option<String>,
    windows: BTreeMap<String, WindowQueue>,
    /// Focus recency, for target selection: a counter bumped on every focus
    /// event, so the most recently focused window wins and the tie-break is
    /// the deterministic label order below.
    focus: BTreeMap<String, u64>,
    focus_counter: u64,
}

/// The shared queue, managed once per launch before any window exists and
/// reached by the IPC commands, the launch, and the window-event hooks alike.
/// Cheap to clone behind an `Arc` at the transport seams.
#[derive(Debug, Default)]
pub struct OpenFilesState {
    inner: Mutex<Inner>,
    #[cfg_attr(not(test), allow(dead_code))]
    // Read by `enqueue`; step 3's CLI is its first production caller.
    counter: AtomicU64,
}

/// The managed shape: one queue per launch, created in `open_window` before
/// any window exists and managed into Tauri before the splash is built.
pub type SharedOpenFiles = Arc<OpenFilesState>;

impl OpenFilesState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Not poisoned by any code in this tree: the critical sections are
        // panic-free collections.
        self.inner
            .lock()
            .expect("the open-files state mutex is not poisoned")
    }

    /// How many unacknowledged requests the queue holds, across every window
    /// and the pre-launch pool — the cap [`QueueError::TooManyRequests`]
    /// guards, and the test seam that proves an overflow refuses without
    /// evicting.
    #[cfg_attr(not(test), allow(dead_code))] // Step 3's CLI diagnostics are its first production caller.
    pub fn total_pending(&self) -> usize {
        let inner = self.lock();
        inner.unassigned.len()
            + inner
                .windows
                .values()
                .map(|queue| queue.pending.len())
                .sum::<usize>()
    }

    /// Enqueue one request: `paths` in the invocation's order, for `target`
    /// when an eligible window exists, or into the pre-launch pool when the
    /// app's first document does not exist yet. Answers the new request's id.
    ///
    /// The whole batch is one unit — the caller validates the paths
    /// themselves before this call, and this only enforces the queue's own
    /// bounds, refusing with nothing enqueued.
    #[cfg_attr(not(test), allow(dead_code))] // Step 3's CLI and second-instance handoff are its first production callers.
    pub fn enqueue(&self, paths: Vec<String>, target: Option<&str>) -> Result<String, QueueError> {
        self.enqueue_arrival(paths, target)
            .map(|placement| placement.id)
    }

    /// Enqueue one live-instance arrival and answer where it landed: the new
    /// request's id plus the window that will read it — `None` when it waits
    /// in the pre-launch pool for the hand-off to claim.
    ///
    /// `eligible` is the window [`select_target_window`] found, when it found
    /// one. When it did not, the destination is decided here, under the same
    /// lock that owns the pool drain: the designated first document if the
    /// hand-off already ran, the pool only while it has not. That is the
    /// one place the two decisions — "where does this arrival go" and "the
    /// pool is closed" — cannot interleave (audit 019, finding 1).
    pub fn enqueue_arrival(
        &self,
        paths: Vec<String>,
        eligible: Option<&str>,
    ) -> Result<ArrivalPlacement, QueueError> {
        let mut inner = self.lock();
        if paths.is_empty() {
            return Err(QueueError::EmptyRequest);
        }
        if paths.len() > MAX_PATHS_PER_REQUEST {
            return Err(QueueError::TooManyPaths);
        }
        if inner.unassigned.len()
            + inner
                .windows
                .values()
                .map(|queue| queue.pending.len())
                .sum::<usize>()
            >= MAX_PENDING_REQUESTS
        {
            return Err(QueueError::TooManyRequests);
        }
        let sequence = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = fresh_request_id(&self.counter);
        let request = PendingRequest {
            id: id.clone(),
            paths,
            sequence,
        };
        // Before the hand-off the pool is the right place; after it, this
        // fallback is what an arrival racing the transition falls to.
        let target = eligible
            .map(str::to_string)
            .or_else(|| inner.first_document.clone());
        match &target {
            Some(window) => inner
                .windows
                .entry(window.clone())
                .or_default()
                .pending
                .push_back(request),
            None => inner.unassigned.push_back(request),
        }
        Ok(ArrivalPlacement { id, window: target })
    }

    /// The calling window's unacknowledged requests, oldest first. Reading
    /// removes nothing: a request leaves only through [`Self::ack`], which is
    /// what makes a reload re-expose the same id.
    pub fn pending(&self, window: &str) -> Vec<PendingRequest> {
        self.lock()
            .windows
            .get(window)
            .map(|queue| queue.pending.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Acknowledge a request: remove it after the app has accepted it. A
    /// repeated ack of the same id succeeds — the window remembers its
    /// acknowledged ids, bounded by [`ACKNOWLEDGED_HISTORY`] — while an id
    /// this window was never assigned is [`QueueError::UnknownRequest`],
    /// because one window can never consume another's requests.
    pub fn ack(&self, window: &str, id: &str) -> Result<(), QueueError> {
        if id.is_empty() || id.len() > MAX_REQUEST_ID_BYTES {
            return Err(QueueError::InvalidId);
        }
        let mut inner = self.lock();
        let Some(queue) = inner.windows.get_mut(window) else {
            return Err(QueueError::UnknownRequest);
        };
        let position = queue.pending.iter().position(|request| request.id == id);
        match position {
            Some(position) => {
                queue.pending.remove(position);
                queue.acknowledged.push_back(id.to_string());
                if queue.acknowledged.len() > ACKNOWLEDGED_HISTORY {
                    queue.acknowledged.pop_front();
                }
                Ok(())
            }
            None if queue.acknowledged.iter().any(|seen| seen == id) => Ok(()),
            None => Err(QueueError::UnknownRequest),
        }
    }

    /// The launch's hand-off: the splash window is becoming the app's first
    /// document, so it is designated as the destination for arrivals that
    /// find no eligible window, and everything still in the pre-launch pool
    /// moves to it — one atomic transition. Answers how many pooled requests
    /// moved, so the caller notifies once, only when there is something to
    /// read.
    ///
    /// This is the boundary audit 019's finding 1 closes: once the pool is
    /// closed, no arrival can wait in it for a drain that already happened.
    /// The transition runs before the navigation it designates the window
    /// for, and [`Self::enqueue_arrival`]'s in-lock fallback covers an
    /// arrival that read the queue before this lock and enqueues after it.
    pub fn handoff_to_first_document(&self, window: &str) -> usize {
        let mut inner = self.lock();
        inner.first_document = Some(window.to_string());
        let moved = inner.unassigned.len();
        if moved == 0 {
            return 0;
        }
        let drained: Vec<_> = inner.unassigned.drain(..).collect();
        let queue = inner.windows.entry(window.to_string()).or_default();
        queue.pending.extend(drained);
        moved
    }

    /// The window the launch designated as the app's first document, once the
    /// hand-off has run — the fallback destination for an arrival that finds
    /// no eligible window while the first document's navigation is settling.
    pub fn first_document(&self) -> Option<String> {
        self.lock().first_document.clone()
    }

    /// Transfer a destroyed window's unacknowledged requests to a surviving
    /// eligible window. Answers how many moved, so the caller notifies the
    /// survivor; the destroyed window's entry — tombstones included — is
    /// gone either way, because a later window reusing its label is a new
    /// owner that inherits nothing.
    pub fn reassign(&self, from: &str, to: &str) -> usize {
        let mut inner = self.lock();
        let moved = inner.windows.remove(from).map_or_else(
            || 0,
            |mut queue| {
                let moved = queue.pending.len();
                // Merge by enqueue order, not by arrival at this window: a
                // survivor's own request can be older than the ones it
                // inherits, and the queue stays oldest-first either way.
                let mut merged: Vec<_> = inner
                    .windows
                    .entry(to.to_string())
                    .or_default()
                    .pending
                    .drain(..)
                    .chain(queue.pending.drain(..))
                    .collect();
                merged.sort_by_key(|request| request.sequence);
                inner
                    .windows
                    .entry(to.to_string())
                    .or_default()
                    .pending
                    .extend(merged);
                moved
            },
        );
        moved
    }

    /// Drop a destroyed window's entry outright, for the destruction that
    /// leaves no eligible survivor: the requests die with the process's own
    /// queue semantics rather than being handed to a window that cannot
    /// exist.
    pub fn drop_window(&self, window: &str) {
        self.lock().windows.remove(window);
    }

    /// Record that `window` gained focus — the recency the target selector
    /// reads. Called from the window-event hook for every window of the app,
    /// whatever its document, because focus is a window fact, not a
    /// document one.
    pub fn note_focused(&self, window: &str) {
        let mut inner = self.lock();
        let rank = inner.focus_counter + 1;
        inner.focus_counter = rank;
        inner.focus.insert(window.to_string(), rank);
    }

    /// The focus recency of `window` — 0 for a window nobody has seen
    /// focused, which is also every window's rank before the first focus
    /// event reaches the hook.
    pub fn focus_rank(&self, window: &str) -> u64 {
        self.lock().focus.get(window).copied().unwrap_or(0)
    }
}

/// The most recently focused window among `candidates`, tie-broken by label
/// order — the deterministic fallback the contract's targeting rule names.
/// Focus is a hint, not a permission: the caller has already filtered the
/// candidates to eligible windows, so this only orders them.
pub fn select_target_among<'a>(
    state: &OpenFilesState,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    candidates
        .into_iter()
        .max_by(|a, b| {
            state
                .focus_rank(a)
                .cmp(&state.focus_rank(b))
                .then_with(|| b.cmp(a))
        })
        .map(str::to_string)
}

// --- the window side of targeting -------------------------------------------
//
// Which window a request belongs to is a Tauri question — existence, origin,
// the close-guard topology — answered here once for both callers: the
// enqueueing boundary (the launch, a second instance) and the `Destroyed`
// reassignment hook. Eligibility is the contract's rule: a real window of
// this app on the backend's own origin — never the splash, never a bundled
// asset page — whose close has not committed. The most recently focused
// eligible window wins; focus is tracked in the state model above.

/// Scheme, host and port all matching — the same comparison the navigation
/// policy makes, restated locally because it is a different question: not
/// "may this navigate" but "is this document the app's".
fn same_origin(a: &tauri::Url, b: &tauri::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The eligible window a new request should target, or `None` while the app's
/// first document does not exist yet (the launch is still on its splash, so
/// the request waits in the pre-launch pool). `exclude` names a window the
/// caller knows is going away — the destroyed one the reassignment hook is
/// finding a survivor for.
pub fn select_target_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    exclude: Option<&str>,
) -> Option<String> {
    use tauri::Manager;

    // The backend origin exists only once the launch is up; before that no
    // window is on it, which is the whole answer.
    let launch = app.try_state::<crate::sidecar::Launch>()?;
    let origin = tauri::Url::parse(&launch.url).ok()?;
    let committed_closing = app
        .try_state::<crate::close_guard::SharedCloseGuards>()
        .map(|guards| guards.committed_closing_windows());

    let state = app.try_state::<SharedOpenFiles>()?;
    let candidates: Vec<String> = app
        .webview_windows()
        .into_keys()
        .filter(|label| {
            Some(label.as_str()) != exclude
                && !committed_closing
                    .as_ref()
                    .is_some_and(|windows| windows.contains(label))
                && app
                    .get_webview_window(label)
                    .and_then(|window| window.url().ok())
                    .is_some_and(|url| same_origin(&url, &origin))
        })
        .collect();
    select_target_among(&state, candidates.iter().map(String::as_str))
}

/// Raise the target a file-bearing arrival selected, best-effort: the window
/// manager may refuse the focus steal, and the request is delivered either
/// way — this is presentation, never delivery.
pub fn present_target_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>, window: &str) {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window(window) {
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

// --- the enqueueing boundary ------------------------------------------------
//
// Three callers admit batches — the parent `open` before it detaches its
// child, the child re-validating the argv it was handed, and the live
// instance's second-instance callback — and they ask the same questions in
// the same order: does this app declare the receiver at all, and does every
// path name a local, existing, regular file? The answers are diagnostics,
// printed by whoever is in a position to print them, because nothing is
// enqueued until the whole batch passes.

/// Whether `manifest` declares the receiver a file batch can be delivered
/// to — `actions.open_files.ipc`, the one transport this group has. A
/// nonempty `file_associations` declaration is already refused at parse time
/// without it (CONTRACT.md §2), so this is the single question left at the
/// boundary.
pub fn receiver_declared(manifest: &crate::manifest::Manifest) -> bool {
    manifest.actions.open_files.ipc
}

/// Validate a whole incoming batch before anything is enqueued: one
/// diagnostic for the first path that fails, naming it and why. Local,
/// existing, regular files only — a declaration of what will be delivered,
/// never a readability guarantee: deletion, permission changes and
/// unsuitable content remain the receiver's to handle when it opens what it
/// accepted.
pub fn validate_batch(paths: &[String]) -> Result<(), String> {
    if paths.len() > MAX_PATHS_PER_REQUEST {
        return Err(format!(
            "a file request may carry at most {MAX_PATHS_PER_REQUEST} paths (CONTRACT.md §7)"
        ));
    }
    for path in paths {
        let file = std::path::Path::new(path);
        if !file.is_absolute() {
            return Err(format!(
                "{path} is not an absolute path — the hub does not reinterpret a relative \
                 one behind its caller's back"
            ));
        }
        match std::fs::metadata(file) {
            Err(error) => {
                return Err(format!("{path} is not an existing local file: {error}"));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "{path} is not a regular file — directories and devices are not delivered"
                ));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// What one arrival in the live instance came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrivalOutcome {
    /// The request is queued: for the window it names — already notified and
    /// presented — or for the app's first document, with the launch's
    /// hand-off doing the notifying.
    Delivered { window: Option<String> },
    /// Nothing was enqueued; the string is the diagnostic to report.
    Refused(String),
}

/// One arrival's delivery in the live instance: the whole body of the
/// single-instance callback's file branch except the scheduling and the
/// reporting. Runs on the event loop, where every close decision and
/// commitment also runs, so the shutdown admission it re-reads here is the
/// boundary the contract names — an arrival suspended across a committed
/// shutdown finds the commitment when it finally runs, and delivers
/// nothing. A file-bearing arrival never opens a window; the no-file branch
/// the caller falls back to is the only one that ever does.
pub fn deliver_arrival<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    guards: &crate::close_guard::SharedCloseGuards,
    declared: bool,
    paths: &[String],
) -> ArrivalOutcome {
    if guards.is_closing() {
        return ArrivalOutcome::Refused(
            "this instance is shutting down; the file request was discarded with it".to_string(),
        );
    }
    if !declared {
        return ArrivalOutcome::Refused(
            "this app declares no open_files receiver, so the files were not delivered".to_string(),
        );
    }
    if let Err(diagnostic) = validate_batch(paths) {
        return ArrivalOutcome::Refused(diagnostic);
    }
    // Where the arrival goes is decided with the queue, not from a launch
    // snapshot read earlier (audit 019, finding 1). An eligible window wins;
    // failing that, the designated first document — its navigation may still
    // be settling, which is exactly the window a startup arrival must reach —
    // and only while no hand-off has run at all does the pre-launch pool
    // hold the request. Never a window opened here: the splash cannot
    // consume app requests.
    let target = match select_target_window(app, None) {
        Some(window) => Some(window),
        None => match designated_target(app, guards) {
            Designated::Window(window) => Some(window),
            Designated::Gone => {
                return ArrivalOutcome::Refused(
                    "no window of this app can receive files right now".to_string(),
                )
            }
            Designated::NotYet => None,
        },
    };
    match enqueue_arrival(app, paths, target.as_deref()) {
        Ok(placement) => {
            // Outside the state lock, and only for a request that landed in a
            // window: the pre-launch pool is announced by the launch's
            // hand-off instead.
            if let Some(window) = &placement.window {
                notify(app, window);
                present_target_window(app, window);
            }
            ArrivalOutcome::Delivered {
                window: placement.window,
            }
        }
        Err(diagnostic) => ArrivalOutcome::Refused(diagnostic),
    }
}

/// What the launch's hand-off left to target when no eligible window exists:
/// the designated first document, that designation gone with its window, or
/// nothing designated yet — the app's first document does not exist and the
/// pre-launch pool is the right place.
enum Designated {
    Window(String),
    Gone,
    NotYet,
}

fn designated_target<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    guards: &crate::close_guard::SharedCloseGuards,
) -> Designated {
    use tauri::Manager;
    let Some(state) = app.try_state::<SharedOpenFiles>() else {
        return Designated::NotYet;
    };
    match state.first_document() {
        Some(label) => {
            let exists = app.get_webview_window(&label).is_some();
            let committed = guards.committed_closing_windows().contains(&label);
            if exists && !committed {
                Designated::Window(label)
            } else {
                Designated::Gone
            }
        }
        None => Designated::NotYet,
    }
}

/// The last step of an admitted arrival. Answers the diagnostic when the
/// queue refused — nothing was evicted and nothing was enqueued.
fn enqueue_arrival<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    paths: &[String],
    target: Option<&str>,
) -> Result<ArrivalPlacement, String> {
    use tauri::Manager;
    let state = app
        .try_state::<SharedOpenFiles>()
        .ok_or_else(|| NO_STATE.to_string())?;
    state
        .enqueue_arrival(paths.to_vec(), target)
        .map_err(|error| error.to_string())
}

// --- the IPC receiver -------------------------------------------------------
//
// Two Tauri commands, granted at runtime only when the manifest declares
// `actions.open_files.ipc` (`window.rs`'s grant table; the static capability
// grants nothing). The commands are a thin layer, exactly like the close-guard
// ones: they resolve the calling window from Tauri's caller — never from an
// app-supplied label — and delegate every decision to the state model above.
// `closing` comes from the shared close-guard state every launch manages
// before its first window, read here without coupling the receiver's
// capability to that group: a window whose close has committed, or a process
// whose shutdown has, answers `closing` because the requests it could still
// acknowledge are going away with it.

/// The error the commands answer when no queue backs the calling window —
/// which cannot happen in a launch that managed the state before its first
/// window, and is the honest answer everywhere else.
const NO_STATE: &str = "unavailable";

/// The error when this window's close, or whole-app shutdown, has committed.
const CLOSING: &str = "closing";

/// `open_files_pending`'s answer: the calling window's unacknowledged
/// requests, oldest first, each with its id and its ordered paths.
#[derive(Debug, serde::Serialize)]
pub struct PendingAnswer {
    pub requests: Vec<PendingRequest>,
}

fn window_is_closing<R: tauri::Runtime>(window: &tauri::Window<R>) -> bool {
    let Some(guards) = window
        .app_handle()
        .try_state::<crate::close_guard::SharedCloseGuards>()
    else {
        // A launch always manages the close-guard state before its first
        // window, so no commitment can exist without it.
        return false;
    };
    guards.is_closing() || guards.committed_closing_windows().contains(window.label())
}

/// The calling window's unacknowledged requests. Reading removes nothing;
/// acknowledgement is [`ack_for_window`].
pub fn pending_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
) -> Result<PendingAnswer, &'static str> {
    let state = window
        .app_handle()
        .try_state::<SharedOpenFiles>()
        .ok_or(NO_STATE)?;
    if window_is_closing(window) {
        return Err(CLOSING);
    }
    Ok(PendingAnswer {
        requests: state.pending(window.label()),
    })
}

/// Acknowledge one request by id, after the app has accepted it. Idempotent
/// per window; a request this window was never assigned is refused.
pub fn ack_for_window<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    id: &str,
) -> Result<(), &'static str> {
    let state = window
        .app_handle()
        .try_state::<SharedOpenFiles>()
        .ok_or(NO_STATE)?;
    if window_is_closing(window) {
        return Err(CLOSING);
    }
    state.ack(window.label(), id).map_err(|error| error.code())
}

#[tauri::command]
pub fn open_files_pending(window: tauri::Window) -> Result<PendingAnswer, &'static str> {
    pending_for_window(&window)
}

#[tauri::command]
pub fn open_files_ack(window: tauri::Window, id: String) -> Result<(), &'static str> {
    ack_for_window(&window, &id)
}

// --- the notification -------------------------------------------------------

/// Emit `event` to `window` only, payload-free by construction: the caller
/// passes `()`, which serialises to `null`. The target is the one label the
/// event system matches a `WebviewWindow`-targeted listener against — see
/// [`OPEN_FILES_EVENT`]'s contract section for the receiver's matching
/// `listen` form.
fn emit_to_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    window: &str,
    event: &str,
    payload: impl serde::Serialize + Clone,
) -> tauri::Result<()> {
    use tauri::Emitter;
    app.emit_to(tauri::EventTarget::webview_window(window), event, payload)
}

/// Ring the selected window's bell: pending requests exist for it. Called
/// after every queue update — enqueue, claim, reassignment — and always
/// *outside* the state lock, which it never touches: an emit failure is
/// diagnosed and the requests stay queued, because the next notification or
/// the receiver's own startup retrieves everything still pending.
pub fn notify<R: tauri::Runtime>(app: &tauri::AppHandle<R>, window: &str) {
    if let Err(error) = emit_to_window(app, window, OPEN_FILES_EVENT, ()) {
        eprintln!(
            "tfsapp-hub: cannot notify {window} of pending file requests: {error} — the \
             requests stay queued; the next notification or the receiver's own startup \
             will retrieve them."
        );
    }
}

#[cfg(test)]
#[path = "open_files_tests.rs"]
mod tests;
