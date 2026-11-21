//! The MCP endpoint: `POST /mcp`, the Model Context Protocol over the
//! Streamable HTTP transport (JSON-RPC 2.0), stateless and without SSE.
//!
//! * A request gets one `application/json` response; a notification or a
//!   response from the client gets `202` with no body; a batch is answered with
//!   a batch. The server never starts a message and issues no session id, so
//!   `GET` and `DELETE` are `405`.
//! * Authentication is the REST API's key check (`crate::auth::authenticate`),
//!   `Authorization: Bearer` only: a session cookie is not accepted here.
//!   A `read` key may list and read; the tools that change data refuse it.
//! * A browser `Origin` that is neither the request's `Host` nor a localhost
//!   name is refused (the spec's DNS-rebinding rule).
//!
//! See `docs/mcp.md`.

mod tools;

use aurora_store::auth::Principal;
use aurora_store::{Rejection, Store, StoreError};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use crate::auth::authenticate;
use crate::routes::ApiError;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_BATCH: usize = 50;
/// Oldest first.
const SUPPORTED_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_VERSION: &str = "2025-06-18";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const INSTRUCTIONS: &str = "Aurora is an issue tracker. Start with list_projects and get_project to learn a project's columns (use their state types), issue types and labels. Issues are addressed as KEY-n, e.g. TRK-12. Move an issue with move_issue (state_type started or completed is enough), keep comments short, and mention a user with @name.";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub fn router() -> Router<Store> {
    Router::new().route("/mcp", post(post_mcp))
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn http_error(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

/// The newest supported revision that is not newer than the client's; the
/// server's latest when the client's is older than everything it supports.
fn negotiate(requested: &str) -> &'static str {
    SUPPORTED_VERSIONS
        .iter()
        .rev()
        .find(|version| **version <= requested)
        .copied()
        .unwrap_or(LATEST_VERSION)
}

fn hostname(authority: &str) -> &str {
    if authority.starts_with('[') {
        authority
            .find(']')
            .map_or(authority, |end| &authority[..=end])
    } else {
        authority.split(':').next().unwrap_or(authority)
    }
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(origin) = origin.to_str().ok() else {
        return false;
    };
    // "null" and anything that is not scheme://authority never match.
    let Some((_, rest)) = origin.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    let same_host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|host| host.eq_ignore_ascii_case(authority));
    same_host || matches!(hostname(authority), "localhost" | "127.0.0.1" | "[::1]")
}

async fn principal_of(store: &Store, headers: &HeaderMap) -> Result<Principal, ApiError> {
    if !headers.contains_key(header::AUTHORIZATION) {
        let err = StoreError::from(Rejection::Unauthorized(
            "send Authorization: Bearer <api key>; /mcp does not accept session cookies".into(),
        ));
        return Err(ApiError::from(err));
    }
    // Every MCP call is a POST, including reads, so the method rule that keeps
    // read keys to GET must not apply here; the tools check the scope instead.
    authenticate(store, &Method::GET, headers)
        .await
        .map(|(principal, _)| principal)
}

struct Session<'a> {
    store: &'a Store,
    principal: Principal,
}

enum Handled {
    Response(Value),
    /// A notification or a response from the client: nothing to answer.
    Nothing,
    /// Not a valid JSON-RPC message; the value is the error response.
    Invalid(Value),
}

async fn post_mcp(State(store): State<Store>, headers: HeaderMap, body: Body) -> Response {
    if !origin_allowed(&headers) {
        return http_error(
            StatusCode::FORBIDDEN,
            "the Origin of this request is not this server",
        );
    }
    let principal = match principal_of(&store, &headers).await {
        Ok(principal) => principal,
        Err(err) => return err.into_response(),
    };
    if let Some(version) = headers.get(PROTOCOL_HEADER) {
        let supported = version
            .to_str()
            .ok()
            .is_some_and(|v| SUPPORTED_VERSIONS.contains(&v));
        if !supported {
            return http_error(
                StatusCode::BAD_REQUEST,
                "unsupported MCP-Protocol-Version; use 2024-11-05, 2025-03-26 or 2025-06-18",
            );
        }
    }
    let bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &rpc_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "the request body is unreadable or larger than 1 MiB",
                ),
            );
        }
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(message) => message,
        Err(err) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &rpc_error(Value::Null, PARSE_ERROR, format!("Parse error: {err}")),
            );
        }
    };

    let session = Session {
        store: &store,
        principal,
    };
    match message {
        Value::Array(items) => {
            if items.is_empty() || items.len() > MAX_BATCH {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &rpc_error(
                        Value::Null,
                        INVALID_REQUEST,
                        format!("a batch holds 1 to {MAX_BATCH} messages"),
                    ),
                );
            }
            let mut answers = Vec::new();
            for item in items {
                match process(&session, item).await {
                    Handled::Response(value) | Handled::Invalid(value) => answers.push(value),
                    Handled::Nothing => {}
                }
            }
            if answers.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                json_response(StatusCode::OK, &Value::Array(answers))
            }
        }
        single => match process(&session, single).await {
            Handled::Response(value) => json_response(StatusCode::OK, &value),
            Handled::Nothing => StatusCode::ACCEPTED.into_response(),
            Handled::Invalid(value) => json_response(StatusCode::BAD_REQUEST, &value),
        },
    }
}

async fn process(session: &Session<'_>, message: Value) -> Handled {
    let Value::Object(mut object) = message else {
        return Handled::Invalid(rpc_error(
            Value::Null,
            INVALID_REQUEST,
            "Invalid Request: a message must be a JSON object",
        ));
    };
    let id = object.remove("id");
    let echo = id.clone().unwrap_or(Value::Null);
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Handled::Invalid(rpc_error(
            echo,
            INVALID_REQUEST,
            "Invalid Request: jsonrpc must be \"2.0\"",
        ));
    }
    match object.remove("method") {
        None if object.contains_key("result") || object.contains_key("error") => Handled::Nothing,
        Some(Value::String(method)) => {
            let params = object.remove("params").unwrap_or(Value::Null);
            match id {
                // A notification: nothing is answered, whatever it is.
                None => Handled::Nothing,
                Some(id) if id.is_string() || id.is_number() => {
                    Handled::Response(dispatch(session, &method, params, id).await)
                }
                Some(_) => Handled::Invalid(rpc_error(
                    Value::Null,
                    INVALID_REQUEST,
                    "Invalid Request: id must be a string or a number",
                )),
            }
        }
        _ => Handled::Invalid(rpc_error(
            echo,
            INVALID_REQUEST,
            "Invalid Request: a request needs a string method",
        )),
    }
}

async fn dispatch(session: &Session<'_>, method: &str, params: Value, id: Value) -> Value {
    match method {
        "initialize" => initialize(params, id),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools::definitions() })),
        "tools/call" => call_tool(session, params, id).await,
        other => rpc_error(id, METHOD_NOT_FOUND, format!("Method not found: {other}")),
    }
}

fn initialize(params: Value, id: Value) -> Value {
    let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "initialize needs params.protocolVersion (a string such as 2025-03-26)",
        );
    };
    rpc_result(
        id,
        json!({
            "protocolVersion": negotiate(requested),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "aurora", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }),
    )
}

async fn call_tool(session: &Session<'_>, params: Value, id: Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(
            id,
            INVALID_PARAMS,
            "tools/call needs params.name (a string)",
        );
    };
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(arguments) if arguments.is_object() => arguments.clone(),
        Some(_) => {
            return rpc_error(id, INVALID_PARAMS, "params.arguments must be an object");
        }
    };
    match tools::call(session.store, &session.principal, name, arguments).await {
        Some(result) => rpc_result(id, result),
        None => rpc_error(id, INVALID_PARAMS, format!("Unknown tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_negotiated_down_to_what_the_server_supports() {
        assert_eq!(negotiate("2024-11-05"), "2024-11-05");
        assert_eq!(negotiate("2025-03-26"), "2025-03-26");
        assert_eq!(negotiate("2025-06-18"), "2025-06-18");
        assert_eq!(negotiate("2025-04-01"), "2025-03-26");
        assert_eq!(negotiate("2099-01-01"), "2025-06-18");
        assert_eq!(negotiate("2020-01-01"), LATEST_VERSION);
        assert_eq!(negotiate("garbage"), LATEST_VERSION);
    }

    #[test]
    fn origins_must_be_this_server_or_localhost() {
        let check = |origin: Option<&str>, host: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, origin.parse().unwrap());
            }
            origin_allowed(&headers)
        };
        assert!(check(None, "example.com"));
        assert!(check(Some("https://example.com"), "example.com"));
        assert!(check(Some("http://localhost:3000"), "127.0.0.1:8787"));
        assert!(check(Some("http://[::1]:3000"), "127.0.0.1:8787"));
        assert!(!check(Some("https://evil.example"), "127.0.0.1:8787"));
        assert!(!check(Some("null"), "127.0.0.1:8787"));
        assert!(!check(
            Some("http://localhost.evil.example"),
            "127.0.0.1:8787"
        ));
    }
}
