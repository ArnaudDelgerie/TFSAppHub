//! The state model's unit tests — plan 055 step 1's list, one behaviour each:
//! independent IDs and owners, idempotence, count/size limits, stale document
//! calls, window-label reuse, guard additions during confirmation,
//! simultaneous closes, callback invalidation, and registration after
//! shutdown commitment. No Tauri, no GTK, no sockets: every concurrency rule
//! here is testable because the module above is plain state.

use super::*;

fn state() -> CloseGuardState {
    CloseGuardState::new()
}

/// A `BTreeSet` from a list of `&str` — the one shape the assertions below
/// compare against.
fn ids(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn registered(state: &CloseGuardState, window: &str) -> BTreeSet<String> {
    state.frontend_guards(window)
}

#[test]
fn a_guard_register_and_remove_round_trip_per_document() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("the current document registers");
    assert_eq!(registered(&state, "main"), ids(&["editor:42"]));

    state
        .frontend_remove("main", &context, "editor:42")
        .expect("the same document removes");
    assert!(registered(&state, "main").is_empty());
}

#[test]
fn registration_is_idempotent_for_the_same_owner_and_id() {
    let state = state();
    let context = state.context("main");
    for _ in 0..3 {
        state
            .frontend_register("main", &context, "editor:42")
            .expect("re-registering is a success, not a duplicate");
    }
    assert_eq!(registered(&state, "main"), ids(&["editor:42"]));
}

#[test]
fn removing_an_absent_id_succeeds_harmlessly() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_remove("main", &context, "never-registered")
        .expect("removing an absent id is not an error");
    state
        .backend_remove("never-registered")
        .expect("and neither is removing an absent backend guard");
}

#[test]
fn two_windows_with_the_same_guard_id_stay_independent() {
    let state = state();
    let main = state.context("main");
    let second = state.context("main-2");
    state
        .frontend_register("main", &main, "editor:42")
        .expect("registration");
    state
        .frontend_register("main-2", &second, "editor:42")
        .expect("the same id in another window is another guard");

    // Removing in one window cannot remove the other's protection.
    state
        .frontend_remove("main", &main, "editor:42")
        .expect("removal");
    assert!(registered(&state, "main").is_empty());
    assert_eq!(registered(&state, "main-2"), ids(&["editor:42"]));
}

#[test]
fn frontend_and_backend_namespaces_never_touch_each_other() {
    let state = state();
    let context = state.context("main");

    state
        .frontend_register("main", &context, "export:1")
        .expect("registration");
    state
        .backend_register("export:1")
        .expect("the backend may use the same string for its own guard");

    // The frontend's remove does not reach the backend's guard...
    state
        .frontend_remove("main", &context, "export:1")
        .expect("removal");
    assert_eq!(state.backend_guards(), ids(&["export:1"]));
    assert!(registered(&state, "main").is_empty());

    // ...and the backend's remove does not resurrect the frontend's.
    state.backend_remove("export:1").expect("removal");
    assert!(state.backend_guards().is_empty());
}

#[test]
fn a_stale_document_cannot_touch_its_successors_guards() {
    let state = state();
    let old = state.context("main");

    // The old document registers, then a committed load rotates the context.
    state
        .frontend_register("main", &old, "editor:42")
        .expect("registration");
    let fresh = state.rotate_context("main");
    assert_ne!(old, fresh, "rotation issues a new token");

    // The successor registers its own guard under the new token.
    let current = state.context("main");
    assert_eq!(current, fresh, "the fetch returns the rotated context");
    state
        .frontend_register("main", &current, "editor:42")
        .expect("the new document registers");

    // A late in-flight call from the old document is refused either way.
    assert_eq!(
        state.frontend_register("main", &old, "late"),
        Err(GuardError::StaleDocument)
    );
    assert_eq!(
        state.frontend_remove("main", &old, "editor:42"),
        Err(GuardError::StaleDocument)
    );

    // The successor's guard survived both attempts.
    assert_eq!(registered(&state, "main"), ids(&["editor:42"]));
}

#[test]
fn the_context_fetch_erases_exactly_the_dead_incarnations_guards() {
    let state = state();
    let old = state.context("main");
    state
        .frontend_register("main", &old, "editor:42")
        .expect("registration");

    // A committed load, and the successor fetches before registering
    // anything of its own: only the dead incarnation is erased.
    state.rotate_context("main");
    state.context("main");
    assert!(registered(&state, "main").is_empty());

    // A reload after the successor registered erases the successor's guards
    // only when *its* successor fetches — never at load-finished, and never
    // the guards the fetching page itself registered.
    let current = state.context("main");
    state
        .frontend_register("main", &current, "editor:43")
        .expect("registration");
    let reloaded = state.rotate_context("main");
    assert_eq!(
        registered(&state, "main"),
        ids(&["editor:43"]),
        "until the new document fetches, the old warning is kept — conservatively"
    );
    state.context("main");
    assert_ne!(reloaded, current);
    assert!(
        registered(&state, "main").is_empty(),
        "the fetch that hands the new document its context also erases the old one's"
    );
}

#[test]
fn a_cancelled_navigation_keeps_the_current_guards() {
    // Nothing here rotates on navigation *intent*: the only rotation hook is
    // the committed load, so a cancelled or blocked navigation, and a
    // same-document `pushState`, leave the guards alone by construction.
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    // No rotation happened: the same context is still current.
    assert_eq!(state.context("main"), context);
    assert_eq!(registered(&state, "main"), ids(&["editor:42"]));
}

#[test]
fn a_reused_window_label_is_a_new_owner() {
    let state = state();
    let old = state.context("main");
    state
        .frontend_register("main", &old, "editor:42")
        .expect("registration");

    // The window is destroyed, and a later window takes the same label.
    state.drop_window("main");
    assert!(registered(&state, "main").is_empty());
    let fresh = state.context("main");
    assert_ne!(old, fresh, "a reused label does not inherit a token");

    // A late in-flight call from the destroyed window's document — even one
    // that correctly guesses nothing, presenting its own old token — is
    // refused for the successor.
    assert_eq!(
        state.frontend_register("main", &old, "editor:42"),
        Err(GuardError::StaleDocument)
    );
    assert_eq!(
        state.frontend_remove("main", &old, "editor:42"),
        Err(GuardError::StaleDocument)
    );
    assert!(registered(&state, "main").is_empty());
}

#[test]
fn guard_ids_are_bounded_in_size_and_never_empty() {
    let state = state();
    let context = state.context("main");
    let exactly_max = "x".repeat(MAX_GUARD_ID_BYTES);
    state
        .frontend_register("main", &context, &exactly_max)
        .expect("an id of exactly the maximum size is valid");
    assert_eq!(
        state.frontend_register("main", &context, &"x".repeat(MAX_GUARD_ID_BYTES + 1)),
        Err(GuardError::InvalidId)
    );
    assert_eq!(
        state.frontend_register("main", &context, ""),
        Err(GuardError::InvalidId)
    );
    assert_eq!(state.backend_register(""), Err(GuardError::InvalidId));
}

#[test]
fn guard_exhaustion_rejects_explicitly_and_never_ejects() {
    let state = state();
    let context = state.context("main");
    for index in 0..MAX_FRONTEND_GUARDS_PER_WINDOW {
        let id = format!("editor:{index}");
        state
            .frontend_register("main", &context, &id)
            .expect("registration up to the cap");
    }
    assert_eq!(
        state.frontend_register("main", &context, "editor:overflow"),
        Err(GuardError::TooManyGuards)
    );

    // An idempotent re-registration is not consumption.
    state
        .frontend_register("main", &context, "editor:0")
        .expect("re-registering an existing id stays a success at the cap");

    // The cap never silently removes someone else's guard.
    assert_eq!(
        registered(&state, "main").len(),
        MAX_FRONTEND_GUARDS_PER_WINDOW
    );

    // The backend namespace has its own cap, untouched by the frontend's.
    for index in 0..MAX_BACKEND_GUARDS {
        state
            .backend_register(&format!("job:{index}"))
            .expect("registration up to the cap");
    }
    assert_eq!(
        state.backend_register("job:overflow"),
        Err(GuardError::TooManyGuards)
    );
}

#[test]
fn a_clean_close_needs_no_confirmation() {
    let state = state();
    assert_eq!(state.begin_close("main", true), CloseFlow::Allow);
    assert_eq!(state.begin_close("main", false), CloseFlow::Allow);
}

#[test]
fn a_guarded_close_opens_one_decision_with_a_snapshot() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    state.backend_register("export:1").expect("registration");

    // The backend guards count only when this close would stop the backend.
    match state.begin_close("main", false) {
        CloseFlow::Confirm {
            frontend, backend, ..
        } => {
            assert_eq!(frontend, ids(&["editor:42"]));
            assert!(backend.is_empty());
        }
        other => panic!("expected a confirmation, got {other:?}"),
    }

    // The pending decision serializes every other close across the app's
    // windows — repeated clicks included.
    assert_eq!(state.begin_close("main", true), CloseFlow::Busy);
    assert_eq!(state.begin_close("main-2", false), CloseFlow::Busy);
}

#[test]
fn cancelling_keeps_the_guards_and_frees_the_pending_slot() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };

    assert_eq!(
        state.resolve_close(&token, false, false),
        Resolution::Cancelled
    );
    assert_eq!(
        registered(&state, "main"),
        ids(&["editor:42"]),
        "cancellation preserves the running app and its guards"
    );

    // The slot is free again: the next close can ask afresh.
    assert!(matches!(
        state.begin_close("main", false),
        CloseFlow::Confirm { .. }
    ));
}

#[test]
fn a_stale_callback_never_grants_permission_to_close() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };

    // A wrong token, and a repeated token, are both refused and both leave
    // the pending decision in place for the real callback.
    assert_eq!(
        state.resolve_close("not-the-token", true, false),
        Resolution::Stale
    );
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
    // The approval was consumed: a replayed confirm is now stale.
    assert_eq!(state.resolve_close(&token, true, false), Resolution::Stale);
}

#[test]
fn a_new_guard_during_confirmation_requires_a_fresh_decision() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm {
        token, frontend, ..
    } = state.begin_close("main", false)
    else {
        panic!("expected a confirmation");
    };
    assert_eq!(frontend, ids(&["editor:42"]));

    // While the dialog is open, another guard appears in the same window.
    state
        .frontend_register("main", &context, "editor:43")
        .expect("registration");

    // The displayed warning did not cover it: the approval cannot apply.
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::NeedsFreshDecision
    );
    // Both guards are still there, and a fresh decision sees both.
    let CloseFlow::Confirm { frontend, .. } = state.begin_close("main", false) else {
        panic!("expected a fresh confirmation");
    };
    assert_eq!(frontend, ids(&["editor:42", "editor:43"]));
}

#[test]
fn an_idempotent_re_registration_does_not_invalidate_a_decision() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };

    // The same page re-asserts its own guard — no new guard, no change.
    state
        .frontend_register("main", &context, "editor:42")
        .expect("idempotent registration");

    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        },
        "the decision is not endlessly invalidated by a no-op"
    );
}

#[test]
fn a_guard_removed_during_confirmation_still_applies_the_approval() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };

    // The job finished while its dialog was open: the warning only
    // over-warned, so the approval applies — a *smaller* at-stake set is
    // never a reason to ask again.
    state
        .frontend_remove("main", &context, "editor:42")
        .expect("removal");
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
}

#[test]
fn a_destroyed_window_invalidates_its_own_decision() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };

    state.drop_window("main");
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Invalidated
    );
    // And the slot is free: the destroyed window consumed its own decision.
    assert_eq!(state.begin_close("main-2", false), CloseFlow::Allow);
}

#[test]
fn a_becoming_last_window_requires_a_fresh_decision() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    state.backend_register("export:1").expect("registration");

    // The close opens on a window that is not the last one: backend guards
    // are not at stake.
    let CloseFlow::Confirm { token, backend, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };
    assert!(backend.is_empty());

    // By the time the dialog answers, this close *would* stop the backend —
    // a warning that never covered the backend cannot authorise stopping it.
    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::NeedsFreshDecision
    );

    // The fresh decision covers both halves, and approves a backend stop.
    let CloseFlow::Confirm { token, backend, .. } = state.begin_close("main", true) else {
        panic!("expected a fresh confirmation");
    };
    assert_eq!(backend, ids(&["export:1"]));
    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::Approved { stop_backend: true }
    );
}

#[test]
fn a_new_window_frees_the_backend_from_an_approved_close() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    state.backend_register("export:1").expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("expected a confirmation");
    };

    // Another window appeared while the dialog was open: this close no
    // longer stops the backend, and the backend's guards are not rechecked —
    // the topology change only narrows what the approval does.
    state.context("main-2");
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
}

#[test]
fn a_new_backend_guard_during_a_backend_relevant_confirmation_is_fresh() {
    let state = state();
    state.backend_register("export:1").expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("expected a confirmation");
    };

    state.backend_register("export:2").expect("registration");
    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::NeedsFreshDecision
    );
}

#[test]
fn registration_after_shutdown_commitment_fails_with_closing() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");

    state.commit_shutdown();
    assert!(state.is_closing());

    assert_eq!(
        state.frontend_register("main", &context, "editor:43"),
        Err(GuardError::Closing)
    );
    assert_eq!(
        state.frontend_remove("main", &context, "editor:42"),
        Err(GuardError::Closing)
    );
    assert_eq!(state.backend_register("export:1"), Err(GuardError::Closing));
    assert_eq!(state.backend_remove("export:1"), Err(GuardError::Closing));

    // Nothing is carried across a committed shutdown either.
    assert_eq!(
        registered(&state, "main"),
        ids(&["editor:42"]),
        "guards stay readable until the process ends — they just cannot move"
    );
}

#[test]
fn a_committed_shutdown_invalidates_pending_decisions_and_bypasses_confirmation() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("expected a confirmation");
    };

    // A signal-initiated shutdown does not wait on the dialog.
    state.commit_shutdown();
    assert_eq!(state.resolve_close(&token, true, false), Resolution::Stale);
    // And once shutdown has committed, a close decision never opens a dialog.
    assert_eq!(state.begin_close("main", false), CloseFlow::Allow);
}

#[test]
fn the_revision_counts_mutations_without_exposing_contents() {
    let state = state();
    let before = state.revision();

    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("idempotence");
    let after_register = state.revision();

    assert!(after_register > before, "a registration moves the revision");
    state
        .frontend_remove("main", &context, "editor:42")
        .expect("removal");
    state
        .frontend_remove("main", &context, "editor:42")
        .expect("harmless absence");
    let after_remove = state.revision();
    assert_eq!(
        after_remove,
        after_register + 1,
        "only the real mutations moved it — the no-ops did not"
    );
}
