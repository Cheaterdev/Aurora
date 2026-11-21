//! Idempotent writes: a request that carries a request id is applied at most
//! once, however often it is sent. See "Idempotency" in `docs/api.md`.
//!
//! The id is recorded by the same transaction that makes the change
//! (`aurora_store::request_log::run`), so a crash leaves both or neither. That
//! is why the handlers do not just run behind a layer that records the id
//! afterwards: the layer here only reads the id and fingerprints the request,
//! and the handlers' writes ([`json`], [`empty`], [`secret`], ...) check and
//! record the id inside their transaction, building the response body there.
//!
//! The layer sits behind authentication on the protected routes. `POST
//! /auth/sign-out` and `POST .../documents/preview` ignore the header ([`excluded`]);
//! the public routes (`/setup`, `/auth/sign-in`) are outside the layer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aurora_store::auth::{KEY_PREFIX, Principal};
use aurora_store::failpoints::{self, Point};
use aurora_store::request_log::{self, Replayed, Reply, RequestKey, Run};
use aurora_store::rusqlite::Transaction;
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::body::{Body, to_bytes};
use axum::extract::{OriginalUri, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::routes::{ApiError, ApiResult};
use crate::shutdown::ShutdownHooks;

/// The request header that carries the id.
pub const HEADER: &str = "idempotency-key";
/// Set to `true` on a response that was replayed from the request log.
pub const REPLAY_HEADER: &str = "idempotent-replay";
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// How often expired ids are deleted.
pub const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

const TOO_LARGE_NOTE: &str = "original response too large to replay";
const KEY_NOTE: &str = "this request was already applied and its key was shown once; \
                        the key is not stored and cannot be shown again";

#[derive(Clone)]
struct Context {
    key: RequestKey,
    used: Arc<AtomicBool>,
}

tokio::task_local! {
    static CONTEXT: Context;
}

/// Writes that never take an id: `POST /auth/sign-out` and the Markdown preview
/// have no effect to repeat. `path` is the request path, with or without `/api/v1`.
pub fn excluded(method: &Method, path: &str) -> bool {
    if method != Method::POST {
        return false;
    }
    let path = path.strip_prefix("/api/v1").unwrap_or(path);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    matches!(
        segments.as_slice(),
        ["auth", "sign-out"] | ["projects", _, "documents", "preview"]
    )
}

fn is_safe(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD || method == Method::OPTIONS
}

fn invalid(reason: String) -> ApiError {
    StoreError::from(Rejection::Invalid(reason)).into()
}

/// The layer: reads `Idempotency-Key` on a changing request and makes it
/// available to the write the handler runs.
pub(crate) async fn layer(req: Request, next: Next) -> Response {
    let present = req.headers().get(HEADER).cloned();
    let principal = req.extensions().get::<Principal>().copied();
    let (Some(value), Some(principal)) = (present, principal) else {
        return next.run(req).await;
    };
    if is_safe(req.method()) || excluded(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    let Some(id) = value.to_str().ok() else {
        return invalid("a request id must be 8 to 128 characters of A-Z a-z 0-9 . _ : -".into())
            .into_response();
    };
    let method = req.method().clone();
    let target = req
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(
            || req.uri().path_and_query().map(ToString::to_string),
            |original| original.0.path_and_query().map(ToString::to_string),
        )
        .unwrap_or_default();
    let id = id.to_string();

    let (parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({ "error": "the request body is unreadable or larger than 2 MiB" })),
        )
            .into_response();
    };
    // JSON is compared by value, so key order and spacing do not matter.
    let canonical = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| serde_json::to_vec(&value).ok())
        .unwrap_or_else(|| bytes.to_vec());
    let key = match RequestKey::new(
        principal.user_id,
        &id,
        &[method.as_str().as_bytes(), target.as_bytes(), &canonical],
    ) {
        Ok(key) => key,
        Err(rejection) => return ApiError::from(StoreError::from(rejection)).into_response(),
    };

    let used = Arc::new(AtomicBool::new(false));
    let context = Context {
        key,
        used: used.clone(),
    };
    let req = Request::from_parts(parts, Body::from(bytes));
    let response = CONTEXT.scope(context, next.run(req)).await;
    if response.status().is_success() && !used.load(Ordering::SeqCst) {
        tracing::error!(
            %method, %target,
            "a request id was sent to a write that does not honour it; the change was not protected"
        );
    }
    response
}

fn answer(status: u16, body: String, replayed: bool) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    if replayed {
        response
            .headers_mut()
            .insert(REPLAY_HEADER, HeaderValue::from_static("true"));
    }
    response
}

fn replay(replayed: Replayed) -> Response {
    let body = replayed
        .body
        .unwrap_or_else(|| json!({ "replayed": true, "note": TOO_LARGE_NOTE }).to_string());
    answer(replayed.status, body, true)
}

/// Runs `change` in the writer's transaction and answers with its [`Reply`];
/// when the request carries an id, the id is checked first and recorded before
/// the commit.
async fn run(
    store: &Store,
    change: impl FnOnce(&Transaction<'_>) -> Result<Reply, StoreError> + Send + 'static,
) -> ApiResult<Response> {
    let context = CONTEXT.try_with(Clone::clone).ok();
    if let Some(context) = &context {
        context.used.store(true, Ordering::SeqCst);
    }
    let key = context.map(|context| context.key);
    let now = now_millis();
    let retention = store.request_retention();
    let outcome = store
        .write(move |tx| request_log::run(tx, key.as_ref(), now, retention, || change(tx)))
        .await?;
    failpoints::hit(Point::AfterCommitBeforeResponse);
    Ok(match outcome {
        Run::Fresh(reply) => answer(reply.status, reply.body, false),
        Run::Replayed(replayed) => replay(replayed),
    })
}

/// A write whose result is the JSON body of a response with `status`.
pub(crate) async fn json<T, F>(store: &Store, status: StatusCode, change: F) -> ApiResult<Response>
where
    T: Serialize,
    F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
{
    run(store, move |tx| {
        let body = serde_json::to_string(&change(tx)?)?;
        Ok(Reply::new(status.as_u16(), body))
    })
    .await
}

/// Like [`json`], for a write that decides the status itself.
pub(crate) async fn json_status<T, F>(store: &Store, change: F) -> ApiResult<Response>
where
    T: Serialize,
    F: FnOnce(&Transaction<'_>) -> Result<(StatusCode, T), StoreError> + Send + 'static,
{
    run(store, move |tx| {
        let (status, value) = change(tx)?;
        Ok(Reply::new(status.as_u16(), serde_json::to_string(&value)?))
    })
    .await
}

/// A write answered with `204 No Content`.
pub(crate) async fn empty<F>(store: &Store, change: F) -> ApiResult<Response>
where
    F: FnOnce(&Transaction<'_>) -> Result<(), StoreError> + Send + 'static,
{
    run(store, move |tx| {
        change(tx)?;
        Ok(Reply::new(StatusCode::NO_CONTENT.as_u16(), String::new()))
    })
    .await
}

/// Replaces every string that looks like (or starts like) an API key, the
/// secret and the listed prefix alike, so that nothing key-shaped is stored.
fn scrub(value: &mut Value) {
    match value {
        Value::String(text) if text.starts_with(KEY_PREFIX) => *value = Value::Null,
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::Object(map) => map.values_mut().for_each(scrub),
        _ => {}
    }
}

/// A write whose response carries a one-time secret in its top-level `key`.
/// The response is sent as it is; the log keeps the same body with `key`
/// replaced by `null`, so the secret is never stored and a replay can only
/// say that it was shown once.
pub(crate) async fn secret<T, F>(
    store: &Store,
    status: StatusCode,
    change: F,
) -> ApiResult<Response>
where
    T: Serialize,
    F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
{
    run(store, move |tx| {
        let full = serde_json::to_value(change(tx)?)?;
        let mut redacted = full.clone();
        scrub(&mut redacted);
        if let Some(object) = redacted.as_object_mut() {
            object.insert("key".into(), Value::Null);
            object.insert("key_replayed".into(), Value::Bool(true));
            object.insert("message".into(), Value::String(KEY_NOTE.into()));
        }
        Ok(Reply {
            status: status.as_u16(),
            body: full.to_string(),
            stored_body: Some(redacted.to_string()),
        })
    })
    .await
}

/// Deletes expired request ids now and then until the store closes: once at
/// start-up, then every `every`. Returns the `Notify` that stops it; it also
/// stops by itself when the store is closed.
pub fn spawn_purge(store: Store, every: Duration) -> Arc<Notify> {
    let stop = Arc::new(Notify::new());
    let signal = stop.clone();
    crate::supervise::spawn_supervised("request-log-purge", async move {
        loop {
            match purge_all(&store).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "purged expired request ids"),
                Err(StoreError::Closed) => return Ok(()),
                Err(err) => tracing::warn!(error = %err, "could not purge expired request ids"),
            }
            tokio::select! {
                () = tokio::time::sleep(every) => {}
                () = signal.notified() => return Ok(()),
            }
        }
    });
    stop
}

/// Whole batches, each its own short write, so user writes interleave.
async fn purge_all(store: &Store) -> Result<usize, StoreError> {
    let mut total = 0;
    loop {
        let removed = store.purge_request_log(now_millis()).await?;
        total += removed;
        if removed < request_log::PURGE_BATCH {
            return Ok(total);
        }
        tokio::task::yield_now().await;
    }
}

/// Registers the hook that stops the purge task before the store closes.
pub fn register_purge_stop(hooks: &mut ShutdownHooks, stop: Arc<Notify>) {
    hooks.register("request-log-purge", move || async move {
        stop.notify_one();
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_are_exactly_sign_out_and_preview() {
        let post = Method::POST;
        assert!(excluded(&post, "/api/v1/auth/sign-out"));
        assert!(excluded(&post, "/auth/sign-out"));
        assert!(excluded(&post, "/api/v1/projects/TRK/documents/preview"));
        assert!(!excluded(&Method::GET, "/api/v1/auth/sign-out"));
        assert!(!excluded(&post, "/api/v1/projects/TRK/documents"));
        assert!(!excluded(&post, "/api/v1/projects/TRK/documents/3/archive"));
        assert!(!excluded(&post, "/api/v1/keys"));
    }
}
