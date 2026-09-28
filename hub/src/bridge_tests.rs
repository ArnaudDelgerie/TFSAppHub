use std::io::{Read, Write};
use std::net::TcpStream;

use super::{start, Bridge, BridgeGroups};
use crate::secrets::new_fake_keyring_store;
use crate::update_check::Context;

/// A bridge with both groups on, one declared key, and a store that never
/// touches D-Bus. Its update context is `Dev` — plain enough for every test
/// but the ones that care about `/update/check`'s own shape, which build
/// their own context via [`bridge_with_update_context`].
fn bridge(groups: BridgeGroups, keys: &[&str]) -> Bridge {
    bridge_with_update_context(groups, keys, Context::Dev)
}

fn bridge_with_update_context(
    groups: BridgeGroups,
    keys: &[&str],
    update_context: Context,
) -> Bridge {
    bridge_with_close_guards(groups, keys, update_context, &fresh_close_guards())
}

/// A bridge sharing `close_guards` — the shape a test needs whenever it drives
/// the same state the routes saw, such as committing shutdown before a call.
fn bridge_with_close_guards(
    groups: BridgeGroups,
    keys: &[&str],
    update_context: Context,
    close_guards: &crate::close_guard::SharedCloseGuards,
) -> Bridge {
    let store = new_fake_keyring_store();
    crate::secrets::secrets_set(&store, "openai", "sk-stored".to_string())
        .expect("the store accepts the write");
    start(
        store,
        keys.iter().map(|key| key.to_string()).collect(),
        groups,
        update_context,
        close_guards.clone(),
    )
    .expect("a started bridge")
}

fn fresh_close_guards() -> crate::close_guard::SharedCloseGuards {
    std::sync::Arc::new(crate::close_guard::CloseGuardState::new())
}

/// A bridge whose store fails every operation — the shape every
/// `storage_failed` assertion drives, without needing a broken D-Bus.
fn failing_bridge(keys: &[&str]) -> Bridge {
    stalled_or_failing_bridge(keys, false)
}

/// A bridge whose store never answers — the frozen-Secret-Service shape,
/// under the shortened test deadline, so the wedged request costs ~100 ms.
fn stalled_bridge(keys: &[&str]) -> Bridge {
    stalled_or_failing_bridge(keys, true)
}

fn stalled_or_failing_bridge(keys: &[&str], stalled: bool) -> Bridge {
    let keyring = crate::secrets::new_fake_keyring();
    let store = if stalled {
        keyring.stalled_store("test.tfsapp-hub")
    } else {
        keyring.failing_store("test.tfsapp-hub")
    };
    start(
        store,
        keys.iter().map(|key| key.to_string()).collect(),
        BOTH,
        Context::Dev,
        fresh_close_guards(),
    )
    .expect("a started bridge")
}

/// A release source pointing at `cache_path`, the shape `/update/check`
/// tests build their context from.
fn release_update_context(cache_path: std::path::PathBuf) -> Context {
    Context::Installed {
        source: crate::registry::Source {
            kind: crate::registry::SourceKind::Release,
            location: "owner/repo".to_string(),
            reference: Some("v1.0.0".to_string()),
            reference_kind: Some(crate::registry::ReferenceKind::Tag),
            index: Some("github".to_string()),
        },
        app_version: "1.0.0".to_string(),
        cache_path,
    }
}

const BOTH: BridgeGroups = BridgeGroups {
    secrets: true,
    update: true,
    close_guard: true,
};

/// One request, spelled out over a raw socket rather than through an HTTP
/// client: the wire contract is what an app's PHP will speak, so the test
/// speaks it too.
fn request(bridge: &Bridge, method: &str, path: &str, token: Option<&str>, body: &str) -> Response {
    let mut stream =
        TcpStream::connect(("127.0.0.1", bridge.port)).expect("the bridge accepts connections");
    let authorization = match token {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
    let raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{authorization}\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(raw.as_bytes()).expect("a written request");

    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("a read response");
    let status = answer
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status code");
    let body = answer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Response { status, body }
}

struct Response {
    status: u16,
    body: String,
}

impl Response {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

#[test]
fn a_request_without_the_token_is_refused_before_anything_else() {
    let bridge = bridge(BOTH, &["openai"]);

    let answer = request(&bridge, "GET", "/healthz", None, "");

    // Before the route is even looked at: the bridge binds loopback, so any
    // process on this machine can reach it and the token is what actually
    // keeps them out.
    assert_eq!(answer.status, 401);
}

#[test]
fn a_request_with_the_wrong_token_is_refused_too() {
    let bridge = bridge(BOTH, &["openai"]);

    let answer = request(&bridge, "GET", "/healthz", Some("not-the-token"), "");

    assert_eq!(answer.status, 401);
}

#[test]
fn the_declared_key_round_trips_over_the_wire() {
    let bridge = bridge(BOTH, &["openai"]);
    let token = bridge.token.clone();

    let has = request(
        &bridge,
        "POST",
        "/secrets/has",
        Some(&token),
        r#"{"key":"openai"}"#,
    );
    assert_eq!(has.status, 200);
    assert_eq!(has.json()["has"], true);

    let got = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        r#"{"key":"openai"}"#,
    );
    assert_eq!(got.json()["value"], "sk-stored");

    let set = request(
        &bridge,
        "POST",
        "/secrets/set",
        Some(&token),
        r#"{"key":"openai","value":"sk-new"}"#,
    );
    assert_eq!(set.status, 200);

    let deleted = request(
        &bridge,
        "POST",
        "/secrets/delete",
        Some(&token),
        r#"{"key":"openai"}"#,
    );
    assert_eq!(deleted.json()["ok"], true);
}

#[test]
fn an_undeclared_key_is_refused_on_the_bridge_as_it_is_over_ipc() {
    let bridge = bridge(BOTH, &["openai"]);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        r#"{"key":"anthropic"}"#,
    );

    // `keys` is a manifest that applies to both transports; the bridge is not
    // the exempt one.
    assert_eq!(answer.status, 403);
    assert_eq!(answer.json()["error"], "key_not_declared");
}

#[test]
fn a_reserved_key_is_refused_even_though_the_app_declared_it() {
    let bridge = bridge(BOTH, &[crate::secrets::APP_SECRET_ACCOUNT]);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        &format!(r#"{{"key":"{}"}}"#, crate::secrets::APP_SECRET_ACCOUNT),
    );

    assert_eq!(answer.status, 403);
}

#[test]
fn every_secret_route_answers_storage_failed_when_the_store_fails() {
    let bridge = failing_bridge(&["openai"]);
    let token = bridge.token.clone();

    let keys = request(&bridge, "GET", "/secrets/keys", Some(&token), "");
    assert_eq!(keys.status, 500);
    assert_eq!(keys.json()["error"], "storage_failed");

    let has = request(
        &bridge,
        "POST",
        "/secrets/has",
        Some(&token),
        r#"{"key":"openai"}"#,
    );
    assert_eq!(has.status, 500);
    assert_eq!(has.json()["error"], "storage_failed");

    let get = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        r#"{"key":"openai"}"#,
    );
    assert_eq!(get.status, 500);
    assert_eq!(get.json()["error"], "storage_failed");

    // Neither is answered 200 any more: a write the backend refused is not a
    // success the next launch disproves, and a delete that failed leaves the
    // key to be assumed still there.
    let set = request(
        &bridge,
        "POST",
        "/secrets/set",
        Some(&token),
        r#"{"key":"openai","value":"sk-new"}"#,
    );
    assert_eq!(set.status, 500);
    assert_eq!(set.json()["error"], "storage_failed");

    let deleted = request(
        &bridge,
        "POST",
        "/secrets/delete",
        Some(&token),
        r#"{"key":"openai"}"#,
    );
    assert_eq!(deleted.status, 500);
    assert_eq!(deleted.json()["error"], "storage_failed");
}

#[test]
fn a_stalled_store_answers_storage_failed_over_the_bridge() {
    // The frozen-Secret-Service shape: the request is answered after the
    // deadline, as `storage_failed` — never held open forever.
    let bridge = stalled_bridge(&["openai"]);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        r#"{"key":"openai"}"#,
    );

    assert_eq!(answer.status, 500);
    assert_eq!(answer.json()["error"], "storage_failed");
}

#[test]
fn a_store_failure_is_answered_after_every_other_refusal() {
    let bridge = failing_bridge(&["openai"]);
    let token = bridge.token.clone();

    let undeclared = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        r#"{"key":"anthropic"}"#,
    );
    assert_eq!(undeclared.status, 403);
    assert_eq!(undeclared.json()["error"], "key_not_declared");

    let too_large = request(
        &bridge,
        "POST",
        "/secrets/set",
        Some(&token),
        &format!(
            r#"{{"key":"openai","value":"{}"}}"#,
            "x".repeat(crate::secrets::MAX_SECRET_VALUE_BYTES + 1)
        ),
    );
    assert_eq!(too_large.status, 413);
    assert_eq!(too_large.json()["error"], "value_too_large");
}

#[test]
fn a_group_that_is_off_answers_not_found_rather_than_forbidden() {
    let bridge = bridge(
        BridgeGroups {
            secrets: false,
            update: true,
            close_guard: true,
        },
        &["openai"],
    );
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/secrets/get",
        Some(&token),
        r#"{"key":"openai"}"#,
    );

    // Indistinguishable from an unrecognised path on purpose: a declined group
    // leaks no more about what this bridge can do than "not found".
    assert_eq!(answer.status, 404);
    assert_eq!(answer.json()["error"], "not_found");
}

#[test]
fn the_update_route_answers_unavailable_for_a_dev_session() {
    let bridge = bridge(BOTH, &[]);
    let token = bridge.token.clone();

    let answer = request(&bridge, "GET", "/update/check", Some(&token), "");

    // 200 with an `unavailable` result, never an error status: a check the host
    // cannot make is a result, and an app polling it has one shape to read.
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["status"], "unavailable");
    assert_eq!(
        answer.json()["reason"],
        crate::update_check::REASON_LOCAL_SOURCE
    );
}

#[test]
fn the_update_route_answers_no_answer_yet_with_nothing_cached() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = crate::paths::Paths::rooted_at(base.path());
    let bridge =
        bridge_with_update_context(BOTH, &[], release_update_context(paths.update_cache_path()));
    let token = bridge.token.clone();

    let answer = request(&bridge, "GET", "/update/check", Some(&token), "");

    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["status"], "unavailable");
    assert_eq!(
        answer.json()["reason"],
        crate::update_check::REASON_NO_ANSWER_YET
    );
}

#[test]
fn the_update_route_answers_ok_once_the_cache_holds_a_release() {
    let base = tempfile::tempdir().expect("a temp data dir");
    let paths = crate::paths::Paths::rooted_at(base.path());
    crate::update_cache::update(&paths, |cache| {
        cache.insert(
            "owner/repo".to_string(),
            crate::update_cache::CachedRelease {
                checked_at: "2026-08-11T00:00:00Z".to_string(),
                tag: "v1.2.0".to_string(),
                release_url: "https://github.com/owner/repo/releases/tag/v1.2.0".to_string(),
                notes: "release notes".to_string(),
                unknown: serde_json::Map::new(),
            },
        );
    })
    .expect("it writes");
    let bridge =
        bridge_with_update_context(BOTH, &[], release_update_context(paths.update_cache_path()));
    let token = bridge.token.clone();

    let answer = request(&bridge, "GET", "/update/check", Some(&token), "");

    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["status"], "ok");
    assert_eq!(answer.json()["current"], "1.0.0");
    assert_eq!(answer.json()["latest"], "1.2.0");
    assert_eq!(answer.json()["update_available"], true);
    assert_eq!(
        answer.json()["release_url"],
        "https://github.com/owner/repo/releases/tag/v1.2.0"
    );
}

#[test]
fn an_oversized_body_is_refused_before_it_is_parsed() {
    let bridge = bridge(BOTH, &["openai"]);
    let token = bridge.token.clone();
    let huge = "x".repeat(20 * 1024);

    let answer = request(
        &bridge,
        "POST",
        "/secrets/set",
        Some(&token),
        &format!(r#"{{"key":"openai","value":"{huge}"}}"#),
    );

    assert_eq!(answer.status, 413);
}

#[test]
fn an_unknown_path_is_a_plain_not_found() {
    let bridge = bridge(BOTH, &["openai"]);
    let token = bridge.token.clone();

    assert_eq!(
        request(&bridge, "GET", "/whatever", Some(&token), "").status,
        404
    );
}

// --- close-guard routes -----------------------------------------------------

#[test]
fn a_backend_guard_registers_and_removes_over_the_route_pair() {
    let guards = fresh_close_guards();
    let bridge = bridge_with_close_guards(BOTH, &[], crate::update_check::Context::Dev, &guards);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["ok"], true);

    assert!(guards.backend_guards().contains("export:job-1"));

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/remove",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["ok"], true);

    assert!(guards.backend_guards().is_empty());
}

#[test]
fn a_backend_guard_needs_the_bearer_token() {
    let bridge = bridge(BOTH, &[]);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        None,
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 401);
    assert_eq!(answer.json()["error"], "unauthorized");

    // The refused request installed nothing.
    let authorized = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(authorized.status, 200);
}

#[test]
fn a_declined_close_guard_group_answers_not_found_like_any_other() {
    let bridge = bridge(
        BridgeGroups {
            secrets: true,
            update: true,
            close_guard: false,
        },
        &[],
    );
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 404);
    assert_eq!(answer.json()["error"], "not_found");
}

#[test]
fn a_malformed_guard_body_is_refused_as_invalid() {
    let bridge = bridge(BOTH, &[]);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"not":"the shape"}"#,
    );
    assert_eq!(answer.status, 400);
    assert_eq!(answer.json()["error"], "invalid_body");
}

#[test]
fn an_oversized_guard_body_is_refused_before_parsing() {
    let bridge = bridge(BOTH, &[]);
    let token = bridge.token.clone();
    let huge_id = "x".repeat(20 * 1024);

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        &format!(r#"{{"id":"{huge_id}"}}"#),
    );
    assert_eq!(answer.status, 413);
}

#[test]
fn an_invalid_guard_id_is_refused_without_touching_the_namespace() {
    let bridge = bridge(BOTH, &[]);
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":""}"#,
    );
    assert_eq!(answer.status, 400);
    assert_eq!(answer.json()["error"], "invalid_id");

    // And the too-many tail: the cap is 16, a 17th registration is refused
    // without evicting any of the standing guards.
    for index in 0..16 {
        let answer = request(
            &bridge,
            "POST",
            "/close-guard/register",
            Some(&token),
            &format!(r#"{{"id":"export:job-{index}"}}"#),
        );
        assert_eq!(answer.status, 200);
    }
    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"export:job-16"}"#,
    );
    assert_eq!(answer.status, 429);
    assert_eq!(answer.json()["error"], "too_many_guards");
}

#[test]
fn a_registration_after_shutdown_commitment_is_refused_as_closing() {
    let guards = fresh_close_guards();
    let bridge = bridge_with_close_guards(BOTH, &[], crate::update_check::Context::Dev, &guards);
    let token = bridge.token.clone();
    guards.commit_shutdown();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 503);
    assert_eq!(answer.json()["error"], "closing");

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/remove",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 503);
    assert_eq!(answer.json()["error"], "closing");
}

#[test]
fn concurrent_backend_registrations_through_the_real_routes_end_empty() {
    let guards = fresh_close_guards();
    let bridge = bridge_with_close_guards(BOTH, &[], crate::update_check::Context::Dev, &guards);
    let port = bridge.port;
    let token = bridge.token.clone();

    // 8 threads each doing register/remove over their own raw socket: the
    // route pair is what a real worker's `finally` speaks, so that is what
    // the test speaks too.
    let mut threads = Vec::new();
    for worker in 0..8 {
        let token = token.clone();
        threads.push(std::thread::spawn(move || {
            for attempt in 0..5 {
                let id = format!("export:worker-{worker}:{attempt}");
                let mut stream = TcpStream::connect(("127.0.0.1", port))
                    .expect("the bridge accepts connections");
                let raw = format!(
                    "POST /close-guard/register HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                     Authorization: Bearer {token}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{{\"id\":\"{id}\"}}",
                    format!(r#"{{"id":"{id}"}}"#).len()
                );
                stream.write_all(raw.as_bytes()).expect("a written request");
                let mut answer = String::new();
                stream.read_to_string(&mut answer).expect("a read response");
                assert!(answer.contains(" 200 "), "registration failed: {answer}");

                let mut stream = TcpStream::connect(("127.0.0.1", port))
                    .expect("the bridge accepts connections");
                let raw = format!(
                    "POST /close-guard/remove HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                     Authorization: Bearer {token}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{{\"id\":\"{id}\"}}",
                    format!(r#"{{"id":"{id}"}}"#).len()
                );
                stream.write_all(raw.as_bytes()).expect("a written request");
                let mut answer = String::new();
                stream.read_to_string(&mut answer).expect("a read response");
                assert!(answer.contains(" 200 "), "removal failed: {answer}");
            }
        }));
    }
    for thread in threads {
        thread.join().expect("a worker that finished cleanly");
    }

    assert!(guards.backend_guards().is_empty());
}

#[test]
fn a_close_guard_only_bridge_starts_without_the_other_groups() {
    // `close_guard.bridge` on its own must be enough to start the bridge and
    // inject its environment (§7): the group's own routes answer, and the two
    // groups that stayed off answer 404 like the declined groups they are.
    let bridge = bridge(
        BridgeGroups {
            secrets: false,
            update: false,
            close_guard: true,
        },
        &[],
    );
    let token = bridge.token.clone();

    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"export:job-1"}"#,
    );
    assert_eq!(answer.status, 200);

    assert_eq!(
        request(
            &bridge,
            "POST",
            "/secrets/get",
            Some(&token),
            r#"{"key":"openai"}"#
        )
        .status,
        404
    );
    assert_eq!(
        request(&bridge, "GET", "/update/check", Some(&token), "").status,
        404
    );

    // The liveness route stays ungated.
    assert_eq!(
        request(&bridge, "GET", "/healthz", Some(&token), "").status,
        200
    );
}

#[test]
fn backend_routes_never_reach_the_frontend_namespace() {
    let guards = fresh_close_guards();
    let context = guards.context("main");
    guards
        .frontend_register("main", &context, "editor:doc-1")
        .expect("a registered frontend guard");
    let bridge = bridge_with_close_guards(BOTH, &[], crate::update_check::Context::Dev, &guards);
    let token = bridge.token.clone();

    // The id the frontend used is free on the backend route: the namespaces
    // are separate, so a backend worker can reuse the very same string
    // without colliding with — or removing — the document's guard.
    let answer = request(
        &bridge,
        "POST",
        "/close-guard/register",
        Some(&token),
        r#"{"id":"editor:doc-1"}"#,
    );
    assert_eq!(answer.status, 200);

    assert!(guards.frontend_guards("main").contains("editor:doc-1"));
    assert!(guards.backend_guards().contains("editor:doc-1"));
}
