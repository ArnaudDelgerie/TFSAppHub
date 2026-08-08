use std::io::{Read, Write};
use std::net::TcpStream;

use super::{start, Bridge, BridgeGroups};
use crate::secrets::new_fake_keyring_store;

/// A bridge with both groups on, one declared key, and a store that never
/// touches D-Bus.
fn bridge(groups: BridgeGroups, keys: &[&str]) -> Bridge {
    let store = new_fake_keyring_store();
    crate::secrets::secrets_set(&store, "openai", "sk-stored".to_string());
    start(
        store,
        keys.iter().map(|key| key.to_string()).collect(),
        groups,
    )
    .expect("a started bridge")
}

const BOTH: BridgeGroups = BridgeGroups {
    secrets: true,
    update: true,
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
fn a_group_that_is_off_answers_not_found_rather_than_forbidden() {
    let bridge = bridge(
        BridgeGroups {
            secrets: false,
            update: true,
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
fn the_update_route_answers_the_shape_an_app_already_handles() {
    let bridge = bridge(BOTH, &[]);
    let token = bridge.token.clone();

    let answer = request(&bridge, "GET", "/update/check", Some(&token), "");

    // 200 with an `unavailable` result, never an error status: a check the host
    // cannot make is a result, and an app polling it has one shape to read.
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json()["status"], "unavailable");
    assert_eq!(
        answer.json()["reason"],
        crate::update::HOST_RESOLVES_UPDATES
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
