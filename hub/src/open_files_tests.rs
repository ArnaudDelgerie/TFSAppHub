//! The queue's own decisions, tested as plain state — plus the thin
//! window-resolving IPC layer through the mock runtime, the same split
//! `close_guard_tests.rs` uses. The boundaries that stay beyond the mock
//! runtime are plan 056 step 5's native validation: the focus hook, the
//! `Destroyed` reassignment wiring and the real event delivery to a webview
//! do not fire on mock windows, so the tests drive the state model directly
//! instead.

use super::*;

fn state() -> OpenFilesState {
    OpenFilesState::new()
}

fn paths_of(requests: &[PendingRequest]) -> Vec<Vec<String>> {
    requests
        .iter()
        .map(|request| request.paths.clone())
        .collect()
}

// --- enqueue, retained reads, replay ----------------------------------------

#[test]
fn an_enqueued_request_is_read_again_until_it_is_acknowledged() {
    let queue = state();
    let id = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();

    // Reading removes nothing: a reload that re-reads sees the same id.
    let first = queue.pending("main");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, id);
    assert_eq!(queue.pending("main"), first, "a read is not a removal");

    queue.ack("main", &id).expect("acknowledgement");
    assert!(queue.pending("main").is_empty());
}

#[test]
fn an_acknowledgement_is_idempotent_within_the_window_that_made_it() {
    let queue = state();
    let id = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();

    queue.ack("main", &id).expect("the first ack");
    // The reload-replay case the contract documents: the app accepted,
    // reloaded, re-read the same id, accepted again and acked again.
    queue
        .ack("main", &id)
        .expect("the second ack is a success, not an error");
    assert!(queue.pending("main").is_empty());
}

#[test]
fn an_id_the_window_was_never_assigned_is_refused() {
    let queue = state();
    let id = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();

    assert_eq!(queue.ack("other", &id), Err(QueueError::UnknownRequest));
    assert_eq!(
        queue.ack("main", "never-issued"),
        Err(QueueError::UnknownRequest)
    );
    // The refused acks removed nothing.
    assert_eq!(queue.pending("main").len(), 1);
}

#[test]
fn a_very_late_duplicate_ack_past_the_tombstone_horizon_says_so() {
    let queue = state();

    // Fill and acknowledge enough requests to push the first id out of the
    // bounded history, then ack it again: indistinguishable from a bug, and
    // the answer names it rather than pretending a success.
    let first = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();
    queue.ack("main", &first).expect("acknowledgement");
    for _ in 0..ACKNOWLEDGED_HISTORY {
        let id = queue
            .enqueue(vec!["/tmp/b.md".to_string()], Some("main"))
            .unwrap();
        queue.ack("main", &id).expect("acknowledgement");
    }
    assert_eq!(queue.ack("main", &first), Err(QueueError::UnknownRequest));
}

#[test]
fn an_empty_or_oversized_id_is_invalid() {
    let queue = state();
    let id = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();

    assert_eq!(queue.ack("main", ""), Err(QueueError::InvalidId));
    assert_eq!(
        queue.ack("main", &"x".repeat(MAX_REQUEST_ID_BYTES + 1)),
        Err(QueueError::InvalidId)
    );
    // The legal boundary itself still works.
    let boundary = "y".repeat(MAX_REQUEST_ID_BYTES);
    assert_eq!(
        queue.ack("main", &boundary),
        Err(QueueError::UnknownRequest)
    );
    assert_eq!(id.len(), 64, "hex ids stay well under the boundary");
}

#[test]
fn repeating_the_same_open_creates_two_independent_requests() {
    let queue = state();
    let first = queue
        .enqueue(vec!["/tmp/same.md".to_string()], Some("main"))
        .unwrap();
    let second = queue
        .enqueue(vec!["/tmp/same.md".to_string()], Some("main"))
        .unwrap();

    assert_ne!(first, second, "one invocation is one request, always");
    let pending = queue.pending("main");
    assert_eq!(
        paths_of(&pending),
        vec![
            vec!["/tmp/same.md".to_string()],
            vec!["/tmp/same.md".to_string()]
        ],
        "oldest first, both alive until each is acknowledged"
    );

    // Acknowledging one leaves the other standing.
    queue.ack("main", &first).expect("acknowledgement");
    let remaining = queue.pending("main");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, second);
}

#[test]
fn a_request_keeps_its_paths_in_the_invocations_order() {
    let queue = state();
    let ordered = vec![
        "/tmp/z.md".to_string(),
        "/tmp/a.md".to_string(),
        "/tmp/m.md".to_string(),
    ];
    queue.enqueue(ordered.clone(), Some("main")).unwrap();

    assert_eq!(paths_of(&queue.pending("main")), vec![ordered]);
}

// --- bounds ------------------------------------------------------------------

#[test]
fn an_empty_batch_is_refused_rather_than_enqueued_as_nothing() {
    let queue = state();
    assert_eq!(
        queue.enqueue(vec![], Some("main")),
        Err(QueueError::EmptyRequest)
    );
    assert_eq!(queue.total_pending(), 0);
}

#[test]
fn a_batch_past_the_path_cap_is_refused_whole() {
    let queue = state();
    let batch: Vec<String> = (0..=MAX_PATHS_PER_REQUEST)
        .map(|n| format!("/tmp/f{n}.md"))
        .collect();
    assert_eq!(
        queue.enqueue(batch, Some("main")),
        Err(QueueError::TooManyPaths)
    );
    assert_eq!(queue.total_pending(), 0, "nothing was enqueued");

    let boundary: Vec<String> = (0..MAX_PATHS_PER_REQUEST)
        .map(|n| format!("/tmp/f{n}.md"))
        .collect();
    queue
        .enqueue(boundary, Some("main"))
        .expect("the cap itself is legal");
}

#[test]
fn overflow_refuses_and_never_evicts() {
    let queue = state();
    let mut ids = Vec::new();
    for n in 0..MAX_PENDING_REQUESTS {
        ids.push(
            queue
                .enqueue(vec![format!("/tmp/f{n}.md")], Some("main"))
                .unwrap(),
        );
    }
    assert_eq!(queue.total_pending(), MAX_PENDING_REQUESTS);

    assert_eq!(
        queue.enqueue(vec!["/tmp/overflow.md".to_string()], Some("main")),
        Err(QueueError::TooManyRequests)
    );
    assert_eq!(queue.total_pending(), MAX_PENDING_REQUESTS);
    assert_eq!(queue.pending("main").len(), MAX_PENDING_REQUESTS);
    // The first request is still the first: nothing was evicted to make room.
    assert_eq!(queue.pending("main")[0].id, ids[0]);

    // Acknowledging one makes room for exactly one more.
    queue.ack("main", &ids[0]).expect("acknowledgement");
    queue
        .enqueue(vec!["/tmp/after-ack.md".to_string()], Some("main"))
        .expect("room was made");
    assert_eq!(queue.total_pending(), MAX_PENDING_REQUESTS);
}

// --- two windows, and the pre-launch pool -------------------------------------

#[test]
fn two_windows_never_see_each_others_requests() {
    let queue = state();
    let main = queue
        .enqueue(vec!["/tmp/main.md".to_string()], Some("main"))
        .unwrap();
    let second = queue
        .enqueue(vec!["/tmp/second.md".to_string()], Some("main-2"))
        .unwrap();

    assert_eq!(
        paths_of(&queue.pending("main")),
        vec![vec!["/tmp/main.md".to_string()]]
    );
    assert_eq!(
        paths_of(&queue.pending("main-2")),
        vec![vec!["/tmp/second.md".to_string()]]
    );

    // Each window acks its own; neither can ack the other's.
    queue.ack("main", &main).expect("acknowledgement");
    assert_eq!(queue.ack("main", &second), Err(QueueError::UnknownRequest));
    assert_eq!(queue.ack("main-2", &main), Err(QueueError::UnknownRequest));
    queue.ack("main-2", &second).expect("acknowledgement");
    assert!(queue.pending("main").is_empty());
    assert!(queue.pending("main-2").is_empty());
}

#[test]
fn an_arrival_before_any_window_exists_waits_in_the_pool() {
    let queue = state();
    let id = queue
        .enqueue(vec!["/tmp/splash.md".to_string()], None)
        .unwrap();

    assert!(queue.pending("main").is_empty(), "no window owns it yet");
    assert_eq!(queue.total_pending(), 1);

    // The launch's claim, once the first app document exists.
    assert_eq!(queue.claim_unassigned("main"), 1);
    assert_eq!(queue.pending("main")[0].id, id);
    assert_eq!(queue.claim_unassigned("main"), 0, "the pool is empty now");
}

#[test]
fn an_arrival_while_the_consumer_processes_lands_in_the_same_window() {
    let queue = state();
    let first = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();

    // The consumer's read/accept/ack cycle, with an arrival landing in the
    // middle of it: the new request is simply next in the same queue.
    let read = queue.pending("main");
    let second = queue
        .enqueue(vec!["/tmp/b.md".to_string()], Some("main"))
        .unwrap();
    queue.ack("main", &read[0].id).expect("acknowledgement");

    let next = queue.pending("main");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, second);
    assert_ne!(second, first);
}

// --- destruction and reassignment ---------------------------------------------

#[test]
fn a_destroyed_targets_requests_transfer_to_the_surviving_window() {
    let queue = state();
    let moved = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();
    let survivor = queue
        .enqueue(vec!["/tmp/b.md".to_string()], Some("main-2"))
        .unwrap();

    assert_eq!(queue.reassign("main", "main-2"), 1);
    assert!(queue.pending("main").is_empty());
    // Oldest first: the transferred request precedes the survivor's own.
    assert_eq!(
        queue
            .pending("main-2")
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        vec![moved.clone(), survivor]
    );

    // The survivor can acknowledge what it inherited.
    queue
        .ack("main-2", &moved)
        .expect("the inherited request is its own now");
    assert_eq!(queue.pending("main-2").len(), 1);
}

#[test]
fn a_window_reusing_a_destroyed_label_inherits_nothing() {
    let queue = state();
    let id = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();
    queue.ack("main", &id).expect("acknowledgement");
    // One more request, then the window dies with it unacknowledged.
    let pending = queue
        .enqueue(vec!["/tmp/b.md".to_string()], Some("main"))
        .unwrap();

    assert_eq!(queue.reassign("main", "main-2"), 1);
    queue.drop_window("main-2");

    // A later window fills the `main` label gap: a new owner, with neither
    // the old tombstones (a repeated ack of the old id is unknown again)
    // nor any pending request.
    assert!(queue.pending("main").is_empty());
    assert_eq!(queue.ack("main", &id), Err(QueueError::UnknownRequest));
    assert_eq!(
        queue.ack("main", &pending),
        Err(QueueError::UnknownRequest),
        "the destroyed window's requests are gone with it"
    );
}

#[test]
fn dropping_a_window_with_no_survivor_loses_only_its_own_requests() {
    let queue = state();
    let dropped = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();
    queue
        .enqueue(vec!["/tmp/b.md".to_string()], Some("main-2"))
        .unwrap();

    queue.drop_window("main");

    assert!(queue.pending("main").is_empty());
    assert_eq!(
        queue.ack("main", &dropped),
        Err(QueueError::UnknownRequest),
        "the dropped window's namespace is gone"
    );
    assert_eq!(
        queue.pending("main-2").len(),
        1,
        "the survivor keeps its own"
    );
    assert_eq!(queue.total_pending(), 1);
}

// --- target selection ----------------------------------------------------------

#[test]
fn the_most_recently_focused_window_is_the_target() {
    let queue = state();
    queue.note_focused("main");
    queue.note_focused("main-2");
    queue.note_focused("main");

    assert_eq!(
        select_target_among(&queue, ["main", "main-2"]).as_deref(),
        Some("main")
    );
    assert_eq!(
        select_target_among(&queue, ["main-2", "main"]).as_deref(),
        Some("main")
    );
}

#[test]
fn without_focus_history_the_smallest_label_wins_deterministically() {
    let queue = state();
    assert_eq!(
        select_target_among(&queue, ["main-3", "main", "main-2"]).as_deref(),
        Some("main")
    );
    assert_eq!(queue.focus_rank("main"), 0);
    assert_eq!(select_target_among(&queue, Vec::<&str>::new()), None);
}

// --- the IPC surface, through the mock runtime ---------------------------------

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn shared_queue() -> SharedOpenFiles {
    std::sync::Arc::new(OpenFilesState::new())
}

fn app_with_windows(
    queue: &SharedOpenFiles,
    labels: &[&str],
) -> tauri::App<tauri::test::MockRuntime> {
    let app = tauri::test::mock_app();
    app.manage(queue.clone());
    for label in labels {
        WebviewWindowBuilder::new(
            &app,
            label.to_string(),
            WebviewUrl::App("index.html".into()),
        )
        .build()
        .expect("a window");
    }
    app
}

fn window_of(
    app: &tauri::App<tauri::test::MockRuntime>,
    label: &str,
) -> tauri::Window<tauri::test::MockRuntime> {
    app.get_webview_window(label)
        .expect("the window was just built")
        .as_ref()
        .window()
}

#[test]
fn a_window_reads_and_acknowledges_through_the_ipc_layer() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    let window = window_of(&app, "main");

    let id = queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();
    let answer = pending_for_window(&window).expect("the queue answers its own window");
    assert_eq!(answer.requests.len(), 1);
    assert_eq!(answer.requests[0].id, id);

    ack_for_window(&window, &id).expect("acknowledgement");
    let answer = pending_for_window(&window).expect("the queue answers its own window");
    assert!(answer.requests.is_empty());
}

#[test]
fn a_window_without_managed_state_is_answered_unavailable() {
    // The shape no real launch has — no queue managed — and the honest answer
    // for every other caller.
    let app = tauri::test::mock_app();
    let window: tauri::Window<tauri::test::MockRuntime> =
        WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
            .build()
            .expect("a window")
            .as_ref()
            .window();

    assert_eq!(pending_for_window(&window).unwrap_err(), "unavailable");
    assert_eq!(ack_for_window(&window, "any").unwrap_err(), "unavailable");
}

#[test]
fn the_ipc_layer_answers_closing_for_a_committed_window() {
    let queue = shared_queue();
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    let app = tauri::test::mock_app();
    app.manage(queue.clone());
    app.manage(guards.clone());
    WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
        .build()
        .expect("a window");
    WebviewWindowBuilder::new(&app, "main-2", WebviewUrl::App("index.html".into()))
        .build()
        .expect("a window");

    queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .expect("enqueue");

    // An unguarded close commits its window: the receiver namespace closes
    // with it, whatever the queue still holds for it.
    assert_eq!(
        guards.begin_close("main", false),
        crate::close_guard::CloseFlow::Allow
    );

    let closing = window_of(&app, "main");
    assert_eq!(pending_for_window(&closing).unwrap_err(), "closing");
    assert_eq!(ack_for_window(&closing, "any").unwrap_err(), "closing");

    // The survivor's namespace is untouched by the other window's commitment.
    let survivor = window_of(&app, "main-2");
    pending_for_window(&survivor).expect("the survivor reads normally");
}

#[test]
fn a_whole_app_shutdown_commitment_closes_every_windows_receiver() {
    let queue = shared_queue();
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    let app = app_with_windows(&queue, &["main"]);
    app.manage(guards.clone());
    let window = window_of(&app, "main");

    guards.commit_shutdown();

    assert_eq!(pending_for_window(&window).unwrap_err(), "closing");
    assert_eq!(ack_for_window(&window, "any").unwrap_err(), "closing");
}

// --- the notification -----------------------------------------------------------

#[test]
fn an_emit_failure_is_diagnosed_and_retains_the_queue() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    queue
        .enqueue(vec!["/tmp/a.md".to_string()], Some("main"))
        .unwrap();

    // A failed emit — here, an event name the event system refuses — changes
    // nothing about the queue it was going to announce: the requests stay
    // queued for the next notification or the receiver's own startup.
    let invalid = "not a valid event name!";
    assert!(
        invalid.chars().any(|c| c.is_whitespace()),
        "the fixture is invalid"
    );
    assert!(
        emit_to_window(app.handle(), "main", invalid, ()).is_err(),
        "the event system refuses the name"
    );
    assert_eq!(queue.pending("main").len(), 1);

    // The real notification path on the same state succeeds and touches
    // nothing either way.
    notify(app.handle(), "main");
    assert_eq!(queue.pending("main").len(), 1);
}
