use std::collections::BTreeSet;

use tauri::Url;

use super::{
    action_capability_is_local, classify_navigation, declared_action_ipc_grants,
    next_window_label_among, resolve_splash_source, resolve_within, second_instance_action,
    splash_style, NavigationTarget, SecondInstanceAction, SplashSource,
};

fn url(text: &str) -> Url {
    Url::parse(text).expect("a parseable URL")
}

// --- classify_navigation -------------------------------------------------
//
// CONTRACT.md §4's policy, ported from the station. The table is what makes the
// two hosts' answers the same; the hub cannot deliver §4 more weakly.

#[test]
fn the_backend_s_own_origin_navigates_in_place() {
    let origin = url("http://127.0.0.1:8123");

    assert_eq!(
        classify_navigation(Some(&origin), &url("http://127.0.0.1:8123/orders/4")),
        NavigationTarget::Internal
    );
}

#[test]
fn another_app_s_port_on_the_same_host_is_not_the_same_origin() {
    let origin = url("http://127.0.0.1:8123");

    // The case the hub creates and the station never could: two apps on one
    // loopback host, told apart by port alone. Reading these as one origin is
    // exactly how one app would navigate into another.
    assert_eq!(
        classify_navigation(Some(&origin), &url("http://127.0.0.1:9999/")),
        NavigationTarget::ExternalWeb
    );
}

#[test]
fn the_bundled_splash_loads_before_any_origin_is_known() {
    // The splash is built before the backend's port exists, so its own page has
    // to be internal on the strength of its scheme alone.
    for target in [
        "tauri://localhost/index.html",
        "http://tauri.localhost/index.html",
        "about:blank",
    ] {
        assert_eq!(
            classify_navigation(None, &url(target)),
            NavigationTarget::Internal,
            "{target} is the webview's own page"
        );
    }
}

#[test]
fn the_open_web_goes_to_the_user_s_own_browser() {
    let origin = url("http://127.0.0.1:8123");

    // Where their bookmarks, their sessions and an address bar are — rather
    // than replacing the app inside its own frame with a site it does not own.
    assert_eq!(
        classify_navigation(Some(&origin), &url("https://example.com/docs")),
        NavigationTarget::ExternalWeb
    );
}

#[test]
fn every_other_scheme_is_refused_outright() {
    let origin = url("http://127.0.0.1:8123");

    for target in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,<h1>hi",
        "blob:http://127.0.0.1:8123/abc",
    ] {
        assert_eq!(
            classify_navigation(Some(&origin), &url(target)),
            NavigationTarget::Blocked,
            "{target} must be refused, not opened anywhere"
        );
    }
}

#[test]
fn the_splash_scheme_is_internal_before_the_app_origin_is_known() {
    // The splash page itself may need to reach its own snapshot root (an
    // `img`/`link` request) before hand-over — the one legitimate use of the
    // scheme from inside a webview.
    assert_eq!(
        classify_navigation(None, &url("tfsapp-splash://localhost/splash.html")),
        NavigationTarget::Internal
    );
}

#[test]
fn the_splash_scheme_is_blocked_once_the_app_origin_is_published() {
    let origin = url("http://127.0.0.1:8123");

    // Once hand-over has happened, the running app has no legitimate reason
    // to reach the splash snapshot a second time — the scheme falls through
    // to the same refusal as any other non-http(s) target.
    assert_eq!(
        classify_navigation(Some(&origin), &url("tfsapp-splash://localhost/splash.html")),
        NavigationTarget::Blocked
    );
}

#[test]
fn an_unknown_origin_never_makes_a_target_internal() {
    // While the splash has not learned the backend URL, an http target is not
    // the app's — treating it as internal would let the pre-backend window be
    // navigated anywhere.
    assert_eq!(
        classify_navigation(None, &url("http://127.0.0.1:8123/")),
        NavigationTarget::ExternalWeb
    );
}

#[test]
fn actions_capability_is_not_granted_to_the_splash_origin() {
    // Tauri's `local` capability target includes the app's bundled fallback
    // (`tauri:`) and custom splash scheme. `action_capability` passes this
    // value straight to `CapabilityBuilder::local`, so false confines actions
    // to its explicit loopback `remote` target instead.
    assert!(!action_capability_is_local());
}

#[test]
fn action_ipc_grants_are_default_off_and_picker_is_independent() {
    let mut actions = crate::manifest::ActionsConfig::default();
    assert!(declared_action_ipc_grants(&actions).next().is_none());

    actions.picker.ipc = true;
    let grants: Vec<_> = declared_action_ipc_grants(&actions)
        .map(|grant| (grant.capability_identifier, grant.permission))
        .collect();

    assert_eq!(grants, [("actions-picker", "allow-picker")]);
}

#[test]
fn action_ipc_grants_close_guard_is_independent_of_the_others() {
    // `close_guard.ipc` alone grants only its own permission: a manifest
    // declaring it does not hand out secrets, update or picker, and none of
    // those three hands out close-guard.
    let mut actions = crate::manifest::ActionsConfig::default();
    actions.close_guard.ipc = true;
    let grants: Vec<_> = declared_action_ipc_grants(&actions)
        .map(|grant| (grant.capability_identifier, grant.permission))
        .collect();

    assert_eq!(grants, [("actions-close-guard", "allow-close-guard")]);

    actions.secrets.ipc = true;
    actions.update.ipc = true;
    actions.picker.ipc = true;
    let grants: Vec<_> = declared_action_ipc_grants(&actions)
        .map(|grant| (grant.capability_identifier, grant.permission))
        .collect();

    assert_eq!(
        grants,
        [
            ("actions-secrets", "allow-secrets"),
            ("actions-update", "allow-update"),
            ("actions-picker", "allow-picker"),
            ("actions-close-guard", "allow-close-guard"),
        ]
    );
}

// --- the splash scheme's path confinement ---------------------------------
//
// `resolve_within` is the whole safety argument for plan 025's scheme: one
// process, one root, and nothing a request can do reaches outside it.

#[test]
fn a_file_actually_inside_the_root_resolves() {
    let dir = tempfile::tempdir().expect("a temp snapshot root");
    let root = dir.path().canonicalize().expect("a canonical root");
    std::fs::write(root.join("splash.html"), b"<h1>hi</h1>").expect("write the splash file");

    assert_eq!(
        resolve_within(&root, "/splash.html"),
        Some(root.join("splash.html"))
    );
}

#[test]
fn a_dot_dot_segment_cannot_escape_the_root() {
    let dir = tempfile::tempdir().expect("a temp parent dir");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("create the root");
    let root = root.canonicalize().expect("a canonical root");
    std::fs::write(dir.path().join("secret.txt"), b"nope").expect("write the sibling file");

    assert_eq!(resolve_within(&root, "/../secret.txt"), None);
}

#[test]
fn a_symlink_pointing_outside_the_root_is_refused() {
    let dir = tempfile::tempdir().expect("a temp parent dir");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).expect("create the outside dir");
    std::fs::write(outside.join("secret.txt"), b"nope").expect("write the outside file");

    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("create the root");
    std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink out of the root");
    let root = root.canonicalize().expect("a canonical root");

    assert_eq!(resolve_within(&root, "/link/secret.txt"), None);
}

#[test]
fn a_missing_file_resolves_to_nothing() {
    let dir = tempfile::tempdir().expect("a temp snapshot root");
    let root = dir.path().canonicalize().expect("a canonical root");

    assert_eq!(resolve_within(&root, "/missing.html"), None);
}

#[test]
fn a_request_for_exactly_the_root_stays_confined() {
    let dir = tempfile::tempdir().expect("a temp snapshot root");
    let root = dir.path().canonicalize().expect("a canonical root");

    // Not a useful response on its own (the handler's `fs::read` on a
    // directory fails and falls back to a refusal) — this only proves the
    // confinement check itself treats the boundary as inside, not outside.
    assert_eq!(resolve_within(&root, "/"), Some(root.clone()));
    assert_eq!(resolve_within(&root, ""), Some(root));
}

// --- resolving splash_path against the snapshot root ----------------------

#[test]
fn no_splash_path_declared_is_the_fallback() {
    let dir = tempfile::tempdir().expect("a temp snapshot root");

    assert_eq!(
        resolve_splash_source(dir.path(), None),
        SplashSource::Fallback
    );
}

#[test]
fn a_declared_splash_path_that_exists_resolves_to_the_scheme() {
    let dir = tempfile::tempdir().expect("a temp snapshot root");
    std::fs::write(dir.path().join("splash.html"), b"<h1>hi</h1>").expect("write the splash");

    assert_eq!(
        resolve_splash_source(dir.path(), Some("splash.html")),
        SplashSource::App(url("tfsapp-splash://localhost/splash.html"))
    );
}

#[test]
fn a_declared_splash_path_pointing_at_a_missing_file_is_the_fallback() {
    let dir = tempfile::tempdir().expect("a temp snapshot root");

    assert_eq!(
        resolve_splash_source(dir.path(), Some("missing.html")),
        SplashSource::Fallback
    );
}

#[test]
fn a_declared_splash_path_escaping_the_root_is_the_fallback() {
    let dir = tempfile::tempdir().expect("a temp parent dir");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("create the root");
    std::fs::write(dir.path().join("secret.txt"), b"nope").expect("write the sibling file");

    assert_eq!(
        resolve_splash_source(&root, Some("../secret.txt")),
        SplashSource::Fallback
    );
}

// --- window labels -------------------------------------------------------

#[test]
fn the_first_window_is_main() {
    assert_eq!(next_window_label_among(|_| false), "main");
}

#[test]
fn further_windows_number_upwards_and_fill_gaps() {
    let open: BTreeSet<&str> = ["main", "main-3"].into_iter().collect();

    // `main-2` closed while `main-3` stayed: the gap is reused rather than
    // counted past, so a long-lived app does not drift to main-97.
    assert_eq!(
        next_window_label_among(|label| open.contains(label)),
        "main-2"
    );
}

// --- splash dressing -----------------------------------------------------

#[test]
fn the_splash_carries_the_app_s_name_and_colours() {
    let script = splash_style("TFS App Test", Some("#101014"), Some("#f0f0f0"));

    assert!(script.contains("Starting TFS App Test"), "{script}");
    assert!(script.contains("--splash-bg"), "{script}");
    assert!(script.contains("#101014"), "{script}");
    assert!(script.contains("#f0f0f0"), "{script}");
}

#[test]
fn an_app_declaring_no_colours_sets_none() {
    let script = splash_style("Demo", None, None);

    // The page's own defaults stay in force rather than being overwritten with
    // something the app never asked for.
    assert!(!script.contains("--splash-bg"), "{script}");
    assert!(!script.contains("--splash-text"), "{script}");
}

#[test]
fn a_name_carrying_a_quote_cannot_break_out_of_the_script() {
    // The name comes from the app's own manifest, which the hub does not write.
    // It reaches the page as JSON rather than as pasted source, so the worst a
    // hostile one can do is look odd.
    let script = splash_style("Bobby \"'; alert(1); //", None, None);

    assert!(
        script.contains(r#"alert(1); //"#) && script.contains(r#"\""#),
        "the name must arrive quoted, not spliced: {script}"
    );
}

// --- second_instance_action (plan 055 step 5) -------------------------------
//
// What a second `open` of an already-running app must do, decided from the
// two facts that matter. Pure, so the topology rules are a unit test rather
// than a live second instance.

#[test]
fn a_second_instance_opens_a_window_only_while_the_launch_survives() {
    assert_eq!(
        second_instance_action(true, false),
        SecondInstanceAction::OpenWindow,
        "a second open of a running app opens another window on its backend"
    );
    assert_eq!(
        second_instance_action(false, false),
        SecondInstanceAction::StillStarting,
        "before the backend is resolved, the window the user waits for is \
         the answer"
    );
}

#[test]
fn a_committed_shutdown_refuses_to_revive_the_closing_instance() {
    // The shutdown commitment wins over a present launch: a `Launch` state
    // outlives the moment its process decided to go away, and a window
    // admitted after the commitment would be a window on a backend about
    // to stop, opened by the one arrival path that never went through a
    // closing decision.
    assert_eq!(
        second_instance_action(true, true),
        SecondInstanceAction::ShuttingDown
    );
    assert_eq!(
        second_instance_action(false, true),
        SecondInstanceAction::ShuttingDown
    );
}

// --- admit_second_instance_window (plan 055 step 8, audit 017 finding 2) -----
//
// The admission unit a second `open` resolves to, as one piece of work on
// the event loop: the shutdown commitment is re-read inside it, at decision
// time, and the window is created inside it too. The D-Bus service thread
// that schedules it is serialized with nothing, which is exactly what these
// regressions model with explicit barriers — no timing, no sleeps.

fn admit_once(
    state: &crate::close_guard::CloseGuardState,
    launch_present: bool,
) -> (
    super::SecondInstanceOutcome,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let created = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = created.clone();
    let outcome = super::admit_second_instance_window(state, launch_present, move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    (outcome, created)
}

#[test]
fn an_arrival_suspended_across_a_committed_shutdown_creates_no_window() {
    // Audit 017, finding 2's interleaving, pinned with barriers: the arrival
    // reads its launch facts, the GTK loop commits a final shutdown while
    // the arrival hangs between that read and its admission unit, and only
    // then does the unit run. The unit re-reads the commitment at decision
    // time — from inside the event loop's serialization — so the closing
    // instance is not revived, whatever the suspended thread once saw.
    let state = std::sync::Arc::new(crate::close_guard::CloseGuardState::new());
    let suspended = std::sync::Arc::new(std::sync::Barrier::new(2));
    let committed = std::sync::Arc::new(std::sync::Barrier::new(2));
    let created = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let arrival = {
        let state = state.clone();
        let created = created.clone();
        let suspended = suspended.clone();
        let committed = committed.clone();
        std::thread::spawn(move || {
            // The D-Bus thread's read, before the suspension: the backend is
            // up, and at this instant no shutdown has committed.
            let launch_present = true;
            assert!(!state.is_closing());
            suspended.wait();
            committed.wait();
            // The scheduled unit finally runs on the event loop.
            let outcome = super::admit_second_instance_window(&state, launch_present, move || {
                created.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            });
            assert_eq!(outcome, super::SecondInstanceOutcome::ShuttingDown);
        })
    };
    suspended.wait();
    // The loop commits the final shutdown while the arrival is suspended.
    state.commit_shutdown();
    committed.wait();
    arrival.join().expect("the arrival completes");
    assert_eq!(
        created.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no window was created on the closing instance"
    );
}

#[test]
fn an_arrival_admitted_before_the_commitment_opens_its_window() {
    // The inverse order, still pinned: the unit runs to completion before
    // the loop commits anything, so the window is created — inside the same
    // serialization every later close decision reads, which is what makes
    // the topology of that decision count it.
    let state = crate::close_guard::CloseGuardState::new();
    let (outcome, created) = admit_once(&state, true);
    assert_eq!(outcome, super::SecondInstanceOutcome::WindowOpened);
    assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn an_admission_unit_refuses_before_the_backend_and_reports_its_failures() {
    let state = crate::close_guard::CloseGuardState::new();
    // No launch to open a window on: the arrival was given nothing but the
    // wait it already had, and nothing is created.
    let (outcome, created) = admit_once(&state, false);
    assert_eq!(outcome, super::SecondInstanceOutcome::StillStarting);
    assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 0);
    // A creation that fails is an outcome, not a panic: the caller reports
    // it and the arrival is over.
    let failed = super::admit_second_instance_window(&state, true, || {
        Err("the webview could not be built".to_string())
    });
    assert_eq!(
        failed,
        super::SecondInstanceOutcome::WindowFailed("the webview could not be built".to_string())
    );
}
