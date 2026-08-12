use std::collections::BTreeSet;

use tauri::Url;

use super::{
    classify_navigation, next_window_label_among, resolve_within, splash_style, NavigationTarget,
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
fn an_unknown_origin_never_makes_a_target_internal() {
    // While the splash has not learned the backend URL, an http target is not
    // the app's — treating it as internal would let the pre-backend window be
    // navigated anywhere.
    assert_eq!(
        classify_navigation(None, &url("http://127.0.0.1:8123/")),
        NavigationTarget::ExternalWeb
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
