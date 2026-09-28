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

/// The receiver of an app that declared `ipc`, opting into directories or
/// not; `Receiver::default()` spells the undeclared case.
fn receiver(directories: bool) -> Receiver {
    Receiver {
        declared: true,
        directories,
    }
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

    // The launch's hand-off, once the first app document exists: it claims
    // the pool and closes it in the same transition.
    assert_eq!(queue.handoff_to_first_document("main"), 1);
    assert_eq!(queue.pending("main")[0].id, id);
    assert_eq!(
        queue.handoff_to_first_document("main"),
        0,
        "the pool is empty now"
    );

    // And once the hand-off has run, no arrival can wait in that pool again:
    // an enqueue with no eligible window falls to the designated document.
    let racing = queue
        .enqueue_arrival(vec!["/tmp/late.md".to_string()], None)
        .expect("the racing arrival is queued");
    assert_eq!(racing.window, Some("main".to_string()));
    assert!(
        queue
            .pending("main")
            .iter()
            .any(|request| request.id == racing.id),
        "the racing arrival reached the first document, not the pool"
    );
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

// --- the enqueueing boundary ----------------------------------------------

#[test]
fn the_receiver_is_resolved_from_the_manifest_pair() {
    let parse = |actions: &str| {
        let contents = format!(
            "{{ \"product_name\": \"T\", \"identifier\": \"dev.local.t\", \
             \"project_name\": \"t\", \"app_version\": \"0.6.0\", \"actions\": {actions} }}"
        );
        let loaded =
            crate::manifest::parse(std::path::Path::new("/t/tfsapp.config.json"), &contents)
                .unwrap_or_else(|error| panic!("must parse: {error}"));
        Receiver::of(&loaded.manifest)
    };

    // Absent, file-only, directory-opted-in: the two questions every
    // boundary asks, answered from the manifest each caller already loaded.
    assert_eq!(parse(r#"{"secrets": {"ipc": true}}"#), Receiver::default());
    assert_eq!(parse(r#"{"open_files": {"ipc": true}}"#), receiver(false));
    assert_eq!(
        parse(r#"{"open_files": {"ipc": true, "directories": true}}"#),
        receiver(true)
    );
}

#[test]
fn a_batch_of_existing_regular_files_validates() {
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes résumé.md");
    std::fs::write(&file, "x").expect("a written file");

    // Spaces and Unicode are ordinary path bytes, for either opt-in state —
    // a receiver that did not opt into directories still takes every file.
    validate_batch(&[file.display().to_string()], false)
        .expect("an existing regular file validates");
    validate_batch(&[file.display().to_string()], true)
        .expect("a directory receiver still takes files");
}

#[test]
fn a_batch_is_validated_whole_before_anything_is_enqueued() {
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let existing = file.display().to_string();

    // Each refusal names the offending path and the reason.
    let relative = validate_batch(&["notes.md".to_string()], false).unwrap_err();
    assert!(
        relative.contains("notes.md") && relative.contains("absolute"),
        "{relative}"
    );
    let missing =
        validate_batch(&[base.path().join("gone.md").display().to_string()], false).unwrap_err();
    assert!(missing.contains("gone.md"), "{missing}");
    let directory = validate_batch(&[base.path().display().to_string()], false).unwrap_err();
    assert!(directory.contains("is a directory"), "{directory}");
    let oversized = validate_batch(&vec![existing; MAX_PATHS_PER_REQUEST + 1], false).unwrap_err();
    assert!(oversized.contains("at most"), "{oversized}");
}

#[test]
fn a_directory_is_admitted_only_for_a_receiver_that_opted_in() {
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let directory = base.path().display().to_string();
    let mixed = vec![file.display().to_string(), directory.clone()];

    // A file-only receiver refuses the directory, naming it and the opt-in.
    let refused = validate_batch(std::slice::from_ref(&directory), false).unwrap_err();
    assert!(
        refused.contains("is a directory") && refused.contains("directories"),
        "{refused}"
    );
    // A bad member refuses the whole mixed batch: the first failing path is
    // the diagnostic, and nothing behind it is admitted either.
    let refused_mixed = validate_batch(&mixed, false).unwrap_err();
    assert!(refused_mixed.contains("is a directory"), "{refused_mixed}");

    // With the opt-in the same batches validate whole: a directory counts
    // as one path, and a mixed file/directory batch is ordinary.
    validate_batch(&[directory], true).expect("an existing directory validates");
    validate_batch(&mixed, true).expect("a mixed batch validates");
}

#[test]
fn special_files_are_refused_whatever_the_opt_in() {
    // `/dev/null` is a character device: not a regular file, not a
    // directory, and no receiver opt-in changes that.
    let refused = validate_batch(&["/dev/null".to_string()], true).unwrap_err();
    assert!(
        refused.contains("/dev/null") && refused.contains("neither a regular file nor a directory"),
        "{refused}"
    );
}

#[test]
fn symlinks_are_followed_for_files_and_directories_alike() {
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let sub = base.path().join("sub");
    std::fs::create_dir(&sub).expect("a directory");
    let file_link = base.path().join("file-link.md");
    std::os::unix::fs::symlink(&file, &file_link).expect("a file symlink");
    let dir_link = base.path().join("dir-link");
    std::os::unix::fs::symlink(&sub, &dir_link).expect("a directory symlink");

    // `std::fs::metadata` follows links, as it does for files today: the
    // link to a file is a file for either receiver, the link to a directory
    // is a directory — admitted only with the opt-in.
    validate_batch(&[file_link.display().to_string()], false)
        .expect("a symlink to a file is a file");
    let refused = validate_batch(&[dir_link.display().to_string()], false).unwrap_err();
    assert!(refused.contains("is a directory"), "{refused}");
    validate_batch(&[dir_link.display().to_string()], true)
        .expect("a symlink to a directory is a directory");
}

#[test]
fn an_arrival_during_startup_waits_in_the_pre_launch_pool() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &[]);
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let path = file.display().to_string();

    // No `sidecar::Launch` managed: the app's first document does not exist
    // yet, so the arrival waits rather than targets a window.
    assert_eq!(
        deliver_arrival(
            app.handle(),
            &guards,
            receiver(false),
            std::slice::from_ref(&path)
        ),
        ArrivalOutcome::Delivered { window: None },
        "the splash era pools the arrival"
    );
    assert_eq!(queue.total_pending(), 1);
    // The launch's hand-off is what delivers it.
    assert_eq!(queue.handoff_to_first_document("main"), 1);
}

#[test]
fn an_arrival_on_a_running_app_targets_its_window() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    app.manage(crate::sidecar::Launch {
        url: "tauri://localhost".to_string(),
        product_name: "Demo".to_string(),
        splash_bg: None,
        splash_text: None,
    });
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    app.manage(guards.clone());
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let path = file.display().to_string();

    let outcome = deliver_arrival(app.handle(), &guards, receiver(false), &[path]);
    assert_eq!(
        outcome,
        ArrivalOutcome::Delivered {
            window: Some("main".to_string())
        },
        "the running app's window receives"
    );
    assert_eq!(
        paths_of(&queue.pending("main")),
        vec![vec![file.display().to_string()]],
        "despite what select_target_window made of the mock window's origin"
    );
}

#[test]
fn a_directory_arrival_is_delivered_to_a_receiver_that_opted_in() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    app.manage(crate::sidecar::Launch {
        url: "tauri://localhost".to_string(),
        product_name: "Demo".to_string(),
        splash_bg: None,
        splash_text: None,
    });
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    app.manage(guards.clone());
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let mixed = vec![
        file.display().to_string(),
        base.path().display().to_string(),
    ];

    // A mixed file/directory batch is one request to a receiver that opted
    // in — the directory counts as one path, never enumerated, and the order
    // is the invocation's own.
    let outcome = deliver_arrival(app.handle(), &guards, receiver(true), &mixed);
    assert_eq!(
        outcome,
        ArrivalOutcome::Delivered {
            window: Some("main".to_string())
        },
        "the opted-in receiver's window receives the mixed batch"
    );
    assert_eq!(
        paths_of(&queue.pending("main")),
        vec![mixed],
        "the directory path is carried verbatim, once, in its batch's order"
    );
}

#[test]
fn a_directory_arrival_is_refused_whole_for_a_file_only_receiver() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    app.manage(crate::sidecar::Launch {
        url: "tauri://localhost".to_string(),
        product_name: "Demo".to_string(),
        splash_bg: None,
        splash_text: None,
    });
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    app.manage(guards.clone());
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let mixed = vec![
        file.display().to_string(),
        base.path().display().to_string(),
    ];

    // The directory member refuses the whole mixed batch — the valid file in
    // it is not enqueued either, so the receiver never sees a half-batch.
    let refused = deliver_arrival(app.handle(), &guards, receiver(false), &mixed);
    assert!(
        matches!(refused, ArrivalOutcome::Refused(ref diagnostic) if diagnostic.contains("is a directory")),
        "said {refused:?}"
    );
    assert_eq!(queue.total_pending(), 0, "nothing was enqueued");
}

#[test]
fn a_directory_arrival_during_startup_waits_in_the_pre_launch_pool() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &[]);
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    let base = tempfile::tempdir().expect("a temp dir");
    let directory = base.path().display().to_string();

    // The cold-start path is the same one files take: no eligible window yet,
    // so the arrival pools and the launch's hand-off claims it.
    assert_eq!(
        deliver_arrival(
            app.handle(),
            &guards,
            receiver(true),
            std::slice::from_ref(&directory)
        ),
        ArrivalOutcome::Delivered { window: None },
        "the splash era pools the directory arrival"
    );
    assert_eq!(
        paths_of(&queue.pending("main")),
        Vec::<Vec<String>>::new(),
        "nothing reaches a window before the hand-off"
    );
    assert_eq!(queue.handoff_to_first_document("main"), 1);
    assert_eq!(
        paths_of(&queue.pending("main")),
        vec![vec![directory]],
        "the hand-off delivers the directory path once"
    );
}

#[test]
fn a_committed_shutdown_refuses_an_arrival_without_enqueueing() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    app.manage(guards.clone());
    guards.commit_shutdown();
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");

    let refused = deliver_arrival(
        app.handle(),
        &guards,
        receiver(false),
        &[file.display().to_string()],
    );
    assert!(
        matches!(refused, ArrivalOutcome::Refused(ref diagnostic) if diagnostic.contains("shutting down")),
        "said {refused:?}"
    );
    assert_eq!(queue.total_pending(), 0, "nothing was enqueued");
}

#[test]
fn an_undeclared_receiver_or_invalid_batch_is_refused_before_enqueueing() {
    let queue = shared_queue();
    let app = app_with_windows(&queue, &["main"]);
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");
    let path = file.display().to_string();

    let undeclared = deliver_arrival(
        app.handle(),
        &guards,
        Receiver::default(),
        std::slice::from_ref(&path),
    );
    assert!(
        matches!(undeclared, ArrivalOutcome::Refused(ref diagnostic) if diagnostic.contains("open_files receiver")),
        "said {undeclared:?}"
    );
    let invalid = deliver_arrival(
        app.handle(),
        &guards,
        receiver(false),
        &["notes.md".to_string()],
    );
    assert!(
        matches!(invalid, ArrivalOutcome::Refused(ref diagnostic) if diagnostic.contains("absolute")),
        "said {invalid:?}"
    );
    assert_eq!(queue.total_pending(), 0, "nothing was enqueued either way");
}

#[test]
fn a_running_app_with_no_eligible_window_refuses_the_arrival() {
    let queue = shared_queue();
    // The app is running (hand-off done, Launch managed) but the only window
    // is committed closing: there is nowhere to deliver, and the pre-launch
    // pool would only strand the request behind a hand-off that already
    // happened.
    let app = app_with_windows(&queue, &["main"]);
    app.manage(crate::sidecar::Launch {
        url: "tauri://localhost".to_string(),
        product_name: "Demo".to_string(),
        splash_bg: None,
        splash_text: None,
    });
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    app.manage(guards.clone());
    assert_eq!(queue.handoff_to_first_document("main"), 0);
    assert_eq!(
        guards.begin_close("main", false),
        crate::close_guard::CloseFlow::Allow
    );
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");

    let refused = deliver_arrival(
        app.handle(),
        &guards,
        receiver(false),
        &[file.display().to_string()],
    );
    assert!(
        matches!(refused, ArrivalOutcome::Refused(ref diagnostic) if diagnostic.contains("no window")),
        "said {refused:?}"
    );
    assert_eq!(queue.total_pending(), 0);
}

#[test]
fn an_arrival_while_the_first_document_settles_reaches_it() {
    let queue = shared_queue();
    // Audit 019, finding 1's second half: `Launch` is published and the
    // hand-off has designated the window, but its navigation has not settled
    // — origin selection finds nothing eligible. The arrival reaches the
    // designated window instead of being refused.
    let app = app_with_windows(&queue, &["main"]);
    app.manage(crate::sidecar::Launch {
        url: "http://127.0.0.1:8123".to_string(),
        product_name: "Demo".to_string(),
        splash_bg: None,
        splash_text: None,
    });
    let guards: crate::close_guard::SharedCloseGuards =
        std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    app.manage(guards.clone());
    assert_eq!(queue.handoff_to_first_document("main"), 0);
    let base = tempfile::tempdir().expect("a temp dir");
    let file = base.path().join("notes.md");
    std::fs::write(&file, "x").expect("a written file");

    let outcome = deliver_arrival(
        app.handle(),
        &guards,
        receiver(false),
        &[file.display().to_string()],
    );
    assert_eq!(
        outcome,
        ArrivalOutcome::Delivered {
            window: Some("main".to_string())
        },
        "the designated first document receives while its origin settles"
    );
    assert_eq!(queue.pending("main").len(), 1);
    assert_eq!(queue.total_pending(), 1, "nothing waits in the pool");
}
