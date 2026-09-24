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

// --- close commitment and reservations (plan 055 step 5, audit 016) ----------
//
// The three rules the audit's first two findings asked for: a close commits
// in the same critical section as its final check, the commitment reserves
// the window until its destruction is *observed* (a posted destruction is
// not an observed one), and the reservation is released only by that
// observation or by an effect that failed while the window stayed usable.

#[test]
fn an_unguarded_close_reserves_its_window_until_the_destruction_is_observed() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    // Nothing at stake anymore: the close commits at once, and reserves.
    state
        .frontend_remove("main", &context, "editor:42")
        .expect("removal");
    assert_eq!(state.begin_close("main", false), CloseFlow::Allow);
    assert_eq!(state.committed_closing_windows(), ids(&["main"]));

    // The committed window's document namespace is closed: the person
    // already approved closing it, so a guard that appeared now could never
    // earn its own confirmation.
    assert_eq!(
        state.frontend_register("main", &context, "editor:43"),
        Err(GuardError::Closing)
    );
    // A repeated close request is not re-decided: no second dialog, no
    // `Busy` — the close is already happening.
    assert_eq!(state.begin_close("main", false), CloseFlow::Allow);

    // The destruction is the one release that matches a destruction
    // actually happening — and a reused label is a new owner.
    state.drop_window("main");
    assert!(state.committed_closing_windows().is_empty());
    let fresh = state.context("main");
    state
        .frontend_register("main", &fresh, "editor:44")
        .expect("a reused label is a new owner");
}

#[test]
fn an_approval_reserves_its_window_until_the_destruction_is_observed() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };

    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
    assert_eq!(state.committed_closing_windows(), ids(&["main"]));
    assert_eq!(
        state.frontend_register("main", &context, "editor:43"),
        Err(GuardError::Closing),
        "the person already approved closing this document"
    );
    assert_eq!(
        state.frontend_remove("main", &context, "editor:42"),
        Err(GuardError::Closing)
    );

    // The destruction releases the reservation and the guards together.
    state.drop_window("main");
    assert!(state.committed_closing_windows().is_empty());
}

#[test]
fn an_unguarded_last_close_commits_shutdown_in_the_same_transition() {
    // Audit 016, finding 2, the no-initial-guards half: the last-window
    // close used to authorise the stop and release the mutex without
    // committing `closing`, so a registration landing in between was
    // accepted and then ignored by a teardown that revalidates nothing.
    let state = state();
    assert_eq!(state.begin_close("main", true), CloseFlow::Allow);
    assert!(state.is_closing());
    assert_eq!(
        state.backend_register("export:1"),
        Err(GuardError::Closing),
        "a registration is either before the transition — and then part of \
         the decision it never prompted, because there was nothing at \
         stake — or refused, never accepted and ignored"
    );
    assert_eq!(state.committed_closing_windows(), ids(&["main"]));
}

#[test]
fn an_approved_final_close_commits_shutdown_before_the_effect_runs() {
    // Audit 016, finding 2, the guarded half: `is_closing()` used to stay
    // false until the teardown thread ran, leaving a window where a bridge
    // call received a success for a guard nobody would ever warn about.
    let state = state();
    state.backend_register("job:old").expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("a guarded close asks");
    };

    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::Approved { stop_backend: true }
    );
    assert!(
        state.is_closing(),
        "the commitment is atomic with the final check"
    );
    assert_eq!(state.backend_register("job:new"), Err(GuardError::Closing));
    assert_eq!(
        state.backend_remove("job:old"),
        Err(GuardError::Closing),
        "the whole namespace is closed once shutdown has committed"
    );
}

#[test]
fn a_failed_close_effect_releases_the_reservation_of_a_usable_window() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );

    // The destruction could not even be posted and the window remained
    // usable: the close is not committed after all.
    state.release_close("main");
    assert!(state.committed_closing_windows().is_empty());
    state
        .frontend_register("main", &context, "editor:43")
        .expect("a usable window's document registers again");
}

#[test]
fn a_failed_close_effect_never_uncommits_a_final_shutdown() {
    let state = state();
    state.backend_register("export:1").expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::Approved { stop_backend: true }
    );

    // Once the final close or a signal has committed a shutdown, every
    // window is going away: no release may count one back as a survivor.
    state.release_close("main");
    assert!(state.is_closing());
    assert_eq!(state.backend_register("export:2"), Err(GuardError::Closing));
    assert_eq!(
        state.begin_close("main", true),
        CloseFlow::Allow,
        "a committed shutdown bypasses confirmation on every later close"
    );
}

#[test]
fn a_committed_close_leaves_unrelated_surviving_windows_usable() {
    let state = state();
    let main = state.context("main");
    let second = state.context("main-2");
    state
        .frontend_register("main", &main, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );

    // `main`'s close is committed and its namespace closed; the survivor's
    // document never noticed and keeps full control of its own guards.
    assert_eq!(
        state.frontend_register("main", &main, "editor:43"),
        Err(GuardError::Closing)
    );
    state
        .frontend_register("main-2", &second, "editor:7")
        .expect("an unrelated surviving window stays usable");
    state
        .frontend_remove("main-2", &second, "editor:7")
        .expect("and keeps full control of its own guards");
}

// --- document-identity binding (plan 055 step 6, audit 016 finding 3) -------
//
// A decision — and the close it commits — belongs to the document
// incarnation it was decided on. A replacement during a confirmation makes
// the answer apply to nothing, whatever guard IDs the successor reuses or
// registers; and a destruction queued for one document is never redirected
// onto its successor at the effect's application boundary.

#[test]
fn a_replacement_during_confirmation_invalidates_even_with_the_same_guard_id() {
    // Audit 016, finding 3's own reproduction: the successor re-registers
    // the very guard ID the warning covered, and the old dialog's approval
    // used to apply — closing a document nobody decided about.
    let state = state();
    let old = state.context("main");
    state
        .frontend_register("main", &old, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("a guarded close asks");
    };

    // The page is replaced while the dialog stands: a committed load
    // rotates, the successor fetches its context, and it re-registers the
    // same ID under its own identity.
    state.rotate_context("main");
    let fresh = state.context("main");
    assert_ne!(old, fresh);
    state
        .frontend_register("main", &fresh, "editor:42")
        .expect("the successor registers its own guard");

    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::Invalidated,
        "the approval was shown about a document the window no longer holds"
    );

    // The replayed answer is now stale, and the successor's guard — its
    // own, under its own incarnation — survived the refused approval.
    assert_eq!(state.resolve_close(&token, true, true), Resolution::Stale);
    assert_eq!(registered(&state, "main"), ids(&["editor:42"]));

    // A fresh close request starts a decision of its own, honestly about
    // the successor's guard.
    assert!(matches!(
        state.begin_close("main", true),
        CloseFlow::Confirm { .. }
    ));
}

#[test]
fn a_replacement_during_confirmation_invalidates_even_with_no_successor_guards() {
    // The finding's second shape: the successor registers nothing at all.
    // The empty recheck used to pass the approval through — closing the
    // successor with no decision ever shown about it.
    let state = state();
    let old = state.context("main");
    state
        .frontend_register("main", &old, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };

    state.rotate_context("main");
    state.context("main");
    assert!(
        registered(&state, "main").is_empty(),
        "the fetch erased the replaced document's guards"
    );

    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Invalidated
    );
    assert_eq!(
        state.begin_close("main", false),
        CloseFlow::Allow,
        "nothing is at stake anymore, so the successor closes cleanly"
    );
}

#[test]
fn a_unchanged_document_never_invalidates_on_its_own_registration() {
    // The mirror of the replacement rules: the same document idempotently
    // re-asserting its own guard — or registering a new one, which must ask
    // again rather than silently apply — is never a replacement.
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };

    state
        .frontend_register("main", &context, "editor:42")
        .expect("the idempotent re-registration");
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
}

#[test]
fn a_first_context_fetch_is_not_a_replacement_for_a_document_free_decision() {
    // A backend-only decision opens on a window whose page never fetched a
    // context — no frontend guard, so no document identity was involved.
    // The page fetching its context *while the dialog stands* is a first
    // fetch, not a replacement: nothing is invalidated by gaining an
    // identity that the decision never depended on.
    let state = state();
    state.backend_register("export:1").expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", true) else {
        panic!("a backend-only close asks");
    };

    state.context("main");
    assert_eq!(
        state.resolve_close(&token, true, true),
        Resolution::Approved { stop_backend: true }
    );
    assert!(state.committed_close_applies("main"));
}

#[test]
fn the_queued_close_applies_only_while_its_incarnation_holds() {
    // The application boundary: the destruction queued for an approved
    // close may only run while the window still holds the document the
    // approval was shown about.
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
    assert!(state.committed_close_applies("main"));

    // A replacement lands between the answer and the application: the
    // queued close must not be redirected onto the successor, and the
    // reservation is released so the window is usable again.
    state.rotate_context("main");
    let fresh = state.context("main");
    assert!(!state.committed_close_applies("main"));
    assert!(state.committed_closing_windows().is_empty());
    state
        .frontend_register("main", &fresh, "editor:43")
        .expect("the successor's document registers again");

    // And a later legitimate close of the successor starts, and applies,
    // its own decision.
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );
    assert!(state.committed_close_applies("main"));
}

#[test]
fn a_committed_shutdown_never_releases_a_mismatched_close() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );

    // A signal commits the whole-app shutdown; a replacement that lands
    // afterwards cannot turn the mismatch into a survivor — every window
    // is going away.
    state.commit_shutdown();
    state.rotate_context("main");
    assert!(!state.committed_close_applies("main"));
    assert_eq!(
        state.committed_closing_windows(),
        ids(&["main"]),
        "a committed shutdown releases nothing"
    );
    assert!(state.is_closing());
}

#[test]
fn a_repeated_close_after_a_replacement_is_decided_fresh() {
    // The repeated-request rule meets the identity rule: a committed close
    // keeps absorbing repeated requests while it still addresses the
    // window's current document — but once that document was replaced,
    // the next request releases the stale reservation and is decided
    // fresh, so the close it commits addresses the successor.
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );

    // While the reservation addresses the current document, a repeated
    // request is the already-happening close.
    assert_eq!(state.begin_close("main", false), CloseFlow::Allow);
    assert!(state.committed_close_applies("main"));

    // The destruction has not been observed yet, and the document is
    // replaced: the successor's own registrations stay refused — the
    // window's close is committed, so nothing in it can earn a
    // confirmation — and the queued effect will never address them.
    state.rotate_context("main");
    let fresh = state.context("main");
    assert_eq!(
        state.frontend_register("main", &fresh, "editor:43"),
        Err(GuardError::Closing)
    );
    assert!(!state.committed_close_applies("main"));

    // A repeated close request is a fresh decision about the successor,
    // not the inherited one: nothing is at stake, so it commits cleanly —
    // and the close it commits addresses the current document.
    assert_eq!(state.begin_close("main", false), CloseFlow::Allow);
    assert!(
        state.committed_close_applies("main"),
        "the fresh commitment carries the successor's incarnation"
    );
}

#[test]
fn a_destroyed_window_s_reused_label_never_inherits_the_queued_close() {
    let state = state();
    let context = state.context("main");
    state
        .frontend_register("main", &context, "editor:42")
        .expect("registration");
    let CloseFlow::Confirm { token, .. } = state.begin_close("main", false) else {
        panic!("a guarded close asks");
    };
    assert_eq!(
        state.resolve_close(&token, true, false),
        Resolution::Approved {
            stop_backend: false
        }
    );

    // The destruction is observed before the queued effect could run, and
    // a later window takes the same label: the reservation died with the
    // window it was decided on, and the successor owes it nothing.
    state.drop_window("main");
    assert!(!state.committed_close_applies("main"));
    let fresh = state.context("main");
    assert_ne!(context, fresh, "a reused label is a new owner");
    state
        .frontend_register("main", &fresh, "editor:42")
        .expect("the successor registers under its own identity");
    assert!(matches!(
        state.begin_close("main", false),
        CloseFlow::Confirm { .. }
    ));
}

// --- the IPC surface, through the mock runtime ------------------------------
//
// The state model above answers every decision; these exercise the thin
// window-resolving layer around it, the same way `secrets_tests` does. Two
// boundaries of the production wiring stay beyond the mock runtime and are
// plan 055 step 4's native validation: the `on_page_load` rotation hook and
// `Destroyed` cleanup do not fire on mock windows, so the tests rotate and
// drop through the state model directly instead.

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn shared_state() -> SharedCloseGuards {
    std::sync::Arc::new(CloseGuardState::new())
}

/// A mock app managing `guards`, with one window — the shape a launch's
/// `open_window` leaves behind.
fn app_with_window(guards: &SharedCloseGuards) -> tauri::Window<tauri::test::MockRuntime> {
    let app = tauri::test::mock_app();
    app.manage(guards.clone());
    WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
        .build()
        .expect("a window")
        .as_ref()
        .window()
}

#[test]
fn a_window_fetches_its_context_then_registers_and_removes_through_it() {
    let guards = shared_state();
    let window = app_with_window(&guards);

    let context = context_for_window(&window).expect("a context");
    register_for_window(&window, &context, "editor:42").expect("registration");
    assert!(guards.frontend_guards("main").contains("editor:42"));

    remove_for_window(&window, &context, "editor:42").expect("removal");
    assert!(guards.frontend_guards("main").is_empty());
}

#[test]
fn a_stale_document_call_cannot_re_register_after_the_context_rotates() {
    let guards = shared_state();
    let window = app_with_window(&guards);

    let old = context_for_window(&window).expect("a context");
    register_for_window(&window, &old, "editor:42").expect("registration");

    // The rotation the `on_page_load` hook performs on a committed load.
    guards.rotate_context("main");

    assert_eq!(
        register_for_window(&window, &old, "editor:42").unwrap_err(),
        "stale_document"
    );
    assert_eq!(
        remove_for_window(&window, &old, "editor:42").unwrap_err(),
        "stale_document"
    );
    // The successor's own context is the one that reaches the namespace.
    let fresh = context_for_window(&window).expect("a context");
    register_for_window(&window, &fresh, "editor:42").expect("registration");
    assert!(guards.frontend_guards("main").contains("editor:42"));
}

#[test]
fn a_window_without_managed_state_is_answered_unavailable() {
    // The mock app manages nothing here — the shape no real launch has, and
    // the honest answer for every other caller.
    let app = tauri::test::mock_app();
    let window: tauri::Window<tauri::test::MockRuntime> =
        WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
            .build()
            .expect("a window")
            .as_ref()
            .window();

    assert_eq!(context_for_window(&window).unwrap_err(), "unavailable");
    assert_eq!(
        register_for_window(&window, "any-context", "editor:42").unwrap_err(),
        "unavailable"
    );
    assert_eq!(
        remove_for_window(&window, "any-context", "editor:42").unwrap_err(),
        "unavailable"
    );
}

#[test]
fn two_apps_states_never_reach_each_other_through_their_windows() {
    // Each launch manages its own state before its first window; a window is
    // the whole address, so one app's document can never register into — or
    // remove from — another app's namespace.
    let first_guards = shared_state();
    let second_guards = shared_state();
    let first = app_with_window(&first_guards);
    let second = app_with_window(&second_guards);

    let first_context = context_for_window(&first).expect("a context");
    register_for_window(&first, &first_context, "editor:42").expect("registration");
    let second_context = context_for_window(&second).expect("a context");
    register_for_window(&second, &second_context, "editor:42").expect("registration");

    assert!(first_guards.frontend_guards("main").contains("editor:42"));
    assert!(second_guards.frontend_guards("main").contains("editor:42"));

    // Removing through one window leaves the other app's guard standing.
    remove_for_window(&first, &first_context, "editor:42").expect("removal");
    assert!(first_guards.frontend_guards("main").is_empty());
    assert!(second_guards.frontend_guards("main").contains("editor:42"));
}
