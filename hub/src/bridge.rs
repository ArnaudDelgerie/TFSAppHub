//! The HTTP bridge: the only way the app's PHP reaches native capabilities.
//!
//! Symfony runs in a FrankenPHP process that cannot see Tauri's IPC channel, so
//! the capabilities the webview reaches with `invoke()` reach PHP over a
//! loopback HTTP socket instead. Plain blocking HTTP, thread-per-request, no
//! async runtime — CONTRACT.md's bridge wire contract. Ported from the station.
//!
//! **What keeps it private is the token, not the port.** It binds `127.0.0.1:0`,
//! so any process on this machine can reach it; every request is authorised
//! against a 256-bit bearer token generated per launch and handed only to this
//! app's own sidecar through `TFS_BRIDGE_TOKEN`. That is the same posture the
//! station has, and it is worth being precise about: it keeps *other users* and
//! *the network* out, and it keeps another app's PHP out only inasmuch as that
//! app never sees this token.
//!
//! **One bridge per app process, and that is the isolation.** The hub runs one
//! process per open app, so a bridge only ever holds one app's store and one
//! app's declared keys. There is no route that names an app, and no request that
//! could reach a store other than the one this bridge was built with — the same
//! rule `secrets.rs` states for the IPC side, arrived at here by construction
//! rather than by a check.

use std::{io::Read, thread};

use serde::Deserialize;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, StatusCode};

use crate::secrets::{
    secret_key_allowed, secret_list_entries, secrets_delete, secrets_get, secrets_has, secrets_set,
    SecretStore, MAX_SECRET_VALUE_BYTES,
};

/// A transport-level cap on the whole body, independent of the per-value one: a
/// body this large cannot hold a valid request anyway, so it is refused before
/// any JSON parsing runs.
const MAX_BODY_BYTES: u64 = 16 * 1024;

pub struct Bridge {
    pub port: u16,
    pub token: String,
}

/// Which groups this bridge answers for.
///
/// The bridge itself starts as soon as *any* group declares `bridge: true`, so
/// each group's routes still have to be gated on that group alone — otherwise
/// declaring one would quietly hand out the other.
#[derive(Clone, Copy)]
pub struct BridgeGroups {
    pub secrets: bool,
    pub update: bool,
    pub close_guard: bool,
}

/// Start the bridge on a free loopback port and return at once; the accept loop
/// runs on its own thread and each request on a fresh one, so a slow handler — a
/// keyring prompt, say — can never block a concurrent call.
///
/// `keys` is the app's declared `actions.secrets.keys`, snapshotted here: like
/// the store, it does not change for the life of a launch. `update_context` is
/// the same `Context` `main::serve` manages for the IPC side — see
/// `update_check.rs` — cloned once per request so `/update/check` re-reads the
/// cache fresh, never the socket. `close_guards` is the launch's shared guard
/// state, cloned once per request: the backend namespace it answers for is
/// this app instance's and nothing else, so a route can never reach another
/// transport's namespace, let alone another app's state.
pub fn start(
    store: SecretStore,
    keys: Vec<String>,
    groups: BridgeGroups,
    update_context: crate::update_check::Context,
    close_guards: crate::close_guard::SharedCloseGuards,
) -> Result<Bridge, Box<dyn std::error::Error>> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| format!("Cannot start the actions bridge: {error}"))?;
    let port = match server.server_addr() {
        tiny_http::ListenAddr::IP(address) => address.port(),
        other => return Err(format!("The bridge bound an unexpected address: {other:?}").into()),
    };
    let token = tfsapp_core::app_secret::random_secret_hex()?;
    let accept_token = token.clone();

    thread::spawn(move || {
        for request in server.incoming_requests() {
            let store = store.clone();
            let token = accept_token.clone();
            let keys = keys.clone();
            let update_context = update_context.clone();
            let close_guards = close_guards.clone();
            thread::spawn(move || {
                handle_request(
                    request,
                    &store,
                    &token,
                    &keys,
                    groups,
                    &update_context,
                    &close_guards,
                )
            });
        }
    });

    Ok(Bridge { port, token })
}

#[derive(Deserialize)]
struct KeyRequest {
    key: String,
}

#[derive(Deserialize)]
struct SetSecretRequest {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct GuardRequest {
    id: String,
}

fn handle_request(
    mut request: tiny_http::Request,
    store: &SecretStore,
    token: &str,
    keys: &[String],
    groups: BridgeGroups,
    update_context: &crate::update_check::Context,
    close_guards: &crate::close_guard::SharedCloseGuards,
) {
    if !is_authorized(&request, token) {
        respond(request, 401, &json!({"error": "unauthorized"}));
        return;
    }

    let method = request.method().clone();
    let url = request.url().to_string();

    // A group whose `bridge` is off gets a plain 404, identical to an
    // unrecognised path: a declined group leaks no more about what this bridge
    // can do than "not found". `/healthz` is ungated — it is the bridge's own
    // liveness route, not any group's.
    if url.starts_with("/secrets/") && !groups.secrets {
        respond(request, 404, &json!({"error": "not_found"}));
        return;
    }
    if url.starts_with("/update/") && !groups.update {
        respond(request, 404, &json!({"error": "not_found"}));
        return;
    }
    if url.starts_with("/close-guard/") && !groups.close_guard {
        respond(request, 404, &json!({"error": "not_found"}));
        return;
    }

    match (method, url.as_str()) {
        (Method::Get, "/healthz") => respond(request, 200, &json!({"status": "ok"})),

        // Always 200: the same answer the IPC command reads never errors, so a
        // caller polling it has one shape to read and no exception to handle.
        (Method::Get, "/update/check") => {
            let body = serde_json::to_value(crate::update_check::answer_now(update_context))
                .expect("an UpdateCheckResult always serialises");
            respond(request, 200, &body);
        }

        // Step 068-1 transitional: the store reports failures, the bridge does
        // not yet — each route maps an `Err` to the answer the old swallowing
        // code gave, so this commit changes no transport. Step 2 replaces every
        // mapping with `500 storage_failed`.
        (Method::Get, "/secrets/keys") => {
            let entries = secret_list_entries(store, keys).unwrap_or_else(|_| {
                keys.iter()
                    .filter(|key| secret_key_allowed(keys, key))
                    .map(|key| crate::secrets::SecretListEntry {
                        key: key.clone(),
                        set: false,
                    })
                    .collect()
            });
            respond(request, 200, &json!({"keys": entries}));
        }

        (Method::Post, "/secrets/has") => match read_body::<KeyRequest>(&mut request) {
            Ok(body) => {
                if !secret_key_allowed(keys, &body.key) {
                    respond(request, 403, &json!({"error": "key_not_declared"}));
                    return;
                }
                let has = secrets_has(store, &body.key).unwrap_or(false);
                respond(request, 200, &json!({"has": has}));
            }
            Err(error) => respond(request, error.status(), &error.body()),
        },

        (Method::Post, "/secrets/get") => match read_body::<KeyRequest>(&mut request) {
            Ok(body) => {
                if !secret_key_allowed(keys, &body.key) {
                    respond(request, 403, &json!({"error": "key_not_declared"}));
                    return;
                }
                match secrets_get(store, &body.key).ok().flatten() {
                    Some(value) => respond(request, 200, &json!({"value": value})),
                    None => respond(request, 404, &json!({"error": "not_found"})),
                }
            }
            Err(error) => respond(request, error.status(), &error.body()),
        },

        (Method::Post, "/secrets/set") => match read_body::<SetSecretRequest>(&mut request) {
            Ok(body) => {
                if !secret_key_allowed(keys, &body.key) {
                    respond(request, 403, &json!({"error": "key_not_declared"}));
                    return;
                }
                if body.value.len() > MAX_SECRET_VALUE_BYTES {
                    respond(request, 413, &json!({"error": "value_too_large"}));
                    return;
                }
                let _ = secrets_set(store, &body.key, body.value);
                respond(request, 200, &json!({"ok": true}));
            }
            Err(error) => respond(request, error.status(), &error.body()),
        },

        (Method::Post, "/secrets/delete") => match read_body::<KeyRequest>(&mut request) {
            Ok(body) => {
                if !secret_key_allowed(keys, &body.key) {
                    respond(request, 403, &json!({"error": "key_not_declared"}));
                    return;
                }
                let existed = secrets_delete(store, &body.key).unwrap_or(false);
                respond(request, 200, &json!({"ok": existed}));
            }
            Err(error) => respond(request, error.status(), &error.body()),
        },

        // The two close-guard routes only change shared state: they never open
        // a dialog, wait on a person, or hold a lock past the call. Errors come
        // back as the contract's status/error pairs; nothing logs the id, which
        // is app-chosen and may name documents or jobs.
        (Method::Post, "/close-guard/register") => match read_body::<GuardRequest>(&mut request) {
            Ok(body) => match close_guards.backend_register(&body.id) {
                Ok(()) => respond(request, 200, &json!({"ok": true})),
                Err(error) => {
                    let (status, code) = guard_error_response(&error);
                    respond(request, status, &json!({ "error": code }));
                }
            },
            Err(error) => respond(request, error.status(), &error.body()),
        },

        // Removal of an absent id is a plain success — the guard's owner may
        // not know whether its own removal already happened.
        (Method::Post, "/close-guard/remove") => match read_body::<GuardRequest>(&mut request) {
            Ok(body) => match close_guards.backend_remove(&body.id) {
                Ok(()) => respond(request, 200, &json!({"ok": true})),
                Err(error) => {
                    let (status, code) = guard_error_response(&error);
                    respond(request, status, &json!({ "error": code }));
                }
            },
            Err(error) => respond(request, error.status(), &error.body()),
        },

        _ => respond(request, 404, &json!({"error": "not_found"})),
    }
}

/// The contract's status/error pairs for a failed guard operation. The state
/// model only hands back its own `GuardError`s, so this is the single place
/// the error tail — 400 `invalid_id`, 429 `too_many_guards`, 503 `closing` —
/// becomes HTTP. `StaleDocument` cannot reach the backend namespace (there is
/// no document to be stale about); the defensive 400 keeps that true rather
/// than asserting it.
fn guard_error_response(error: &crate::close_guard::GuardError) -> (u16, &'static str) {
    use crate::close_guard::GuardError;
    match error {
        GuardError::InvalidId | GuardError::StaleDocument => (400, error.code()),
        GuardError::TooManyGuards => (429, error.code()),
        GuardError::Closing => (503, error.code()),
    }
}

/// Compare against the expected header in constant time.
///
/// Defence in depth rather than a fix for a live exploit: this is a loopback
/// comparison against a 256-bit token, where a timing signal is already
/// impractical to observe. The token's length is fixed and public, so the
/// length short-circuit leaks nothing an observer does not already know.
fn is_authorized(request: &tiny_http::Request, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    request.headers().iter().any(|header| {
        header.field.equiv("Authorization") && constant_time_eq(header.value.as_str(), &expected)
    })
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

enum ReadBodyError {
    Invalid,
    TooLarge,
}

impl ReadBodyError {
    fn status(&self) -> u16 {
        match self {
            Self::Invalid => 400,
            Self::TooLarge => 413,
        }
    }

    fn body(&self) -> Value {
        match self {
            Self::Invalid => json!({"error": "invalid_body"}),
            Self::TooLarge => json!({"error": "payload_too_large"}),
        }
    }
}

/// Reads one byte past the cap, so a body that is merely *at* the limit can be
/// told from one that exceeds it — regardless of what the underlying stream had
/// left.
fn read_body<T: for<'de> Deserialize<'de>>(
    request: &mut tiny_http::Request,
) -> Result<T, ReadBodyError> {
    let mut content = String::new();
    Read::take(request.as_reader(), MAX_BODY_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(|_| ReadBodyError::Invalid)?;
    if content.len() as u64 > MAX_BODY_BYTES {
        return Err(ReadBodyError::TooLarge);
    }
    serde_json::from_str(&content).map_err(|_| ReadBodyError::Invalid)
}

fn respond(request: tiny_http::Request, status: u16, body: &Value) {
    let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("a static header is valid");
    let response = Response::from_string(payload)
        .with_status_code(StatusCode(status))
        .with_header(header);
    // Never log the token or any value; a send error only means the caller went
    // away.
    let _ = request.respond(response);
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
