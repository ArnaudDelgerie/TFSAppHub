use super::{refresh_if_due, RefreshOutcome};
use crate::{paths::Paths, registry::Source, update_cache::CachedRelease};

fn temp_paths() -> (tempfile::TempDir, Paths) {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = Paths::rooted_at(base.path());
    (base, paths)
}

fn release_source() -> Source {
    Source {
        kind: crate::registry::SourceKind::Release,
        location: "owner/repo".to_string(),
        reference: Some("v1.0.0".to_string()),
        reference_kind: Some(crate::registry::ReferenceKind::Tag),
        index: Some("github".to_string()),
    }
}

/// A `tiny_http` server that answers exactly one request with `status` and
/// `body`, then stops — `release_tests.rs`'s own `stub_once`, duplicated
/// rather than shared across two private test modules.
fn stub_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("a local stub server");
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => panic!("unexpected listen address: {other:?}"),
    };
    let handle = std::thread::spawn(move || {
        let request = server.recv().expect("one request");
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .expect("a valid header");
        let response = tiny_http::Response::from_string(body)
            .with_status_code(status)
            .with_header(header);
        request.respond(response).expect("a sent response");
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn seed(paths: &Paths, checked_at: &str, tag: &str) {
    crate::update_cache::update(paths, |cache| {
        cache.insert(
            "owner/repo".to_string(),
            CachedRelease {
                checked_at: checked_at.to_string(),
                tag: tag.to_string(),
                release_url: format!("https://github.com/owner/repo/releases/tag/{tag}"),
                notes: String::new(),
                unknown: serde_json::Map::new(),
            },
        );
    })
    .expect("seeding the cache writes");
}

fn hours_ago(hours: i64) -> String {
    (time::OffsetDateTime::now_utc() - time::Duration::hours(hours))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("a formattable timestamp")
}

#[test]
fn a_missing_entry_is_refreshed() {
    let (_base, paths) = temp_paths();
    let (base_url, handle) = stub_once(
        200,
        r#"{"tag_name": "v1.2.0", "html_url": "https://github.com/owner/repo/releases/tag/v1.2.0", "assets": [], "body": "Notes."}"#,
    );

    let outcome = refresh_if_due(&paths, &base_url, &release_source());
    handle.join().expect("the stub thread finishes");

    assert_eq!(
        outcome,
        RefreshOutcome::Refreshed {
            malformed_tag: None
        }
    );
    let cache = crate::update_cache::load(&paths);
    assert_eq!(cache["owner/repo"].tag, "v1.2.0");
    assert_eq!(cache["owner/repo"].notes, "Notes.");
}

#[test]
fn a_stale_entry_is_replaced_with_the_latest_release() {
    let (_base, paths) = temp_paths();
    seed(&paths, &hours_ago(25), "v1.0.0");
    let (base_url, handle) = stub_once(
        200,
        r#"{"tag_name": "v1.2.0", "html_url": "https://github.com/owner/repo/releases/tag/v1.2.0", "assets": []}"#,
    );

    let outcome = refresh_if_due(&paths, &base_url, &release_source());
    handle.join().expect("the stub thread finishes");

    assert_eq!(
        outcome,
        RefreshOutcome::Refreshed {
            malformed_tag: None
        }
    );
    assert_eq!(
        crate::update_cache::load(&paths)["owner/repo"].tag,
        "v1.2.0"
    );
}

#[test]
fn refreshing_a_stale_entry_preserves_its_unknown_fields() {
    let (_base, paths) = temp_paths();
    crate::update_cache::update(&paths, |cache| {
        cache.insert(
            "owner/repo".to_string(),
            CachedRelease {
                checked_at: hours_ago(25),
                tag: "v1.0.0".to_string(),
                release_url: "https://github.com/owner/repo/releases/tag/v1.0.0".to_string(),
                notes: String::new(),
                unknown: serde_json::Map::from_iter([(
                    "newer_hubs_field".to_string(),
                    serde_json::json!({"kept": true}),
                )]),
            },
        );
    })
    .expect("seeding the cache writes");
    let (base_url, handle) = stub_once(
        200,
        r#"{"tag_name": "v1.2.0", "html_url": "https://github.com/owner/repo/releases/tag/v1.2.0", "assets": []}"#,
    );

    let outcome = refresh_if_due(&paths, &base_url, &release_source());
    handle.join().expect("the stub thread finishes");

    assert_eq!(
        outcome,
        RefreshOutcome::Refreshed {
            malformed_tag: None
        }
    );
    assert_eq!(
        crate::update_cache::load(&paths)["owner/repo"].unknown["newer_hubs_field"],
        serde_json::json!({"kept": true})
    );
}

#[test]
fn a_fresh_entry_is_left_alone_with_no_request_made() {
    let (_base, paths) = temp_paths();
    seed(&paths, &hours_ago(1), "v1.0.0");

    // No stub server is started at all — a request of any kind would fail to
    // connect, so a `NotDue` outcome is the only way this can pass.
    let outcome = refresh_if_due(&paths, "http://127.0.0.1:1", &release_source());

    assert_eq!(outcome, RefreshOutcome::NotDue);
    assert_eq!(
        crate::update_cache::load(&paths)["owner/repo"].tag,
        "v1.0.0"
    );
}

#[test]
fn a_failing_endpoint_leaves_the_previous_entry_intact() {
    let (_base, paths) = temp_paths();
    seed(&paths, &hours_ago(25), "v1.0.0");
    let (base_url, handle) = stub_once(404, "");

    let outcome = refresh_if_due(&paths, &base_url, &release_source());
    handle.join().expect("the stub thread finishes");

    assert!(matches!(outcome, RefreshOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(
        crate::update_cache::load(&paths)["owner/repo"].tag,
        "v1.0.0"
    );
}

#[test]
fn a_tag_that_does_not_parse_as_a_version_is_still_cached_raw() {
    let (_base, paths) = temp_paths();
    let (base_url, handle) = stub_once(
        200,
        r#"{"tag_name": "not-a-version", "html_url": "https://github.com/owner/repo/releases/tag/not-a-version", "assets": []}"#,
    );

    let outcome = refresh_if_due(&paths, &base_url, &release_source());
    handle.join().expect("the stub thread finishes");

    assert_eq!(
        outcome,
        RefreshOutcome::Refreshed {
            malformed_tag: Some("not-a-version".to_string())
        }
    );
    // Cached exactly as published — `update_check::answer` is what turns this
    // into `no_answer_yet`, not this module.
    assert_eq!(
        crate::update_cache::load(&paths)["owner/repo"].tag,
        "not-a-version"
    );
}

#[test]
fn an_unparseable_checked_at_is_treated_as_stale() {
    let (_base, paths) = temp_paths();
    seed(&paths, "not a timestamp", "v1.0.0");
    let (base_url, handle) = stub_once(
        200,
        r#"{"tag_name": "v1.2.0", "html_url": "https://github.com/owner/repo/releases/tag/v1.2.0", "assets": []}"#,
    );

    let outcome = refresh_if_due(&paths, &base_url, &release_source());
    handle.join().expect("the stub thread finishes");

    assert_eq!(
        outcome,
        RefreshOutcome::Refreshed {
            malformed_tag: None
        }
    );
}
