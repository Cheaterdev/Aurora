//! Authentication for the REST API: the middleware that guards every route
//! except `/health`, `POST /setup` and `POST /auth/sign-in`, and the handlers
//! for first-run setup, sign-in and sign-out, API keys and agent users.
//!
//! A request authenticates with `Authorization: Bearer <api key>` or with the
//! `aurora_session` cookie. The authenticated [`Principal`] and the way it
//! authenticated ([`Via`]) are put in the request extensions for handlers.
//!
//! # Rules
//!
//! * No, malformed, unknown, revoked or expired credentials: 401 with
//!   `WWW-Authenticate: Bearer`. An `Authorization` header is never mixed with
//!   a cookie: if one is present, only it is considered.
//! * A `read` scope key may only use GET and HEAD; anything else is 403.
//! * CSRF: a browser attaches the cookie to cross-site requests on its own, so
//!   a state-changing request (anything but GET/HEAD) authenticated by the
//!   cookie must carry `X-Requested-With: aurora`. A cross-site page cannot add
//!   a custom header without a CORS preflight, which this server never
//!   approves (it sends no CORS headers). The cookie is also `SameSite=Lax`.
//!   Bearer requests are exempt: nothing attaches a key automatically.
//! * Safe methods never change state in this API, so GET needs no header.

use aurora_store::auth::{self, ApiKey, Principal, Scope, User};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::Json;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::routes::{ApiError, ApiJson, ApiQuery, ApiResult, created, no_content};

pub const SESSION_COOKIE: &str = "aurora_session";
pub const CSRF_HEADER: &str = "x-requested-with";
pub const CSRF_VALUE: &str = "aurora";

/// How the request proved who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Key,
    Session,
}

impl Via {
    fn as_str(self) -> &'static str {
        match self {
            Via::Key => "key",
            Via::Session => "session",
        }
    }
}

enum Presented {
    Key(String),
    Session(String),
}

fn bearer_token(value: &HeaderValue) -> Option<&str> {
    let text = value.to_str().ok()?;
    let (scheme, token) = text.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn is_safe(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD
}

fn presented(headers: &HeaderMap) -> Result<Presented, ApiError> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        return match bearer_token(value) {
            Some(token) => Ok(Presented::Key(token.to_string())),
            None => Err(unauthorized("invalid credentials")),
        };
    }
    match cookie_value(headers, SESSION_COOKIE) {
        Some(token) if !token.is_empty() => Ok(Presented::Session(token.to_string())),
        _ => Err(unauthorized("authentication required")),
    }
}

fn unauthorized(message: &str) -> ApiError {
    StoreError::from(Rejection::Unauthorized(message.into())).into()
}

fn forbidden(message: &str) -> ApiError {
    StoreError::from(Rejection::Forbidden(message.into())).into()
}

pub(crate) async fn authenticate(
    store: &Store,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(Principal, Via), ApiError> {
    let presented = presented(headers)?;
    let now = now_millis();
    let (found, via) = match presented {
        Presented::Key(key) => (
            store
                .read(move |c| auth::authenticate_key(c, now, &key))
                .await?,
            Via::Key,
        ),
        Presented::Session(token) => (
            store
                .read(move |c| auth::authenticate_session(c, now, &token))
                .await?,
            Via::Session,
        ),
    };
    let found = found.ok_or_else(|| unauthorized("invalid credentials"))?;
    let principal = found.principal;

    if !is_safe(method) {
        if !principal.scope.allows_writes() {
            return Err(forbidden(
                "this key has read scope and cannot change anything",
            ));
        }
        let header_ok = headers
            .get(CSRF_HEADER)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(CSRF_VALUE.as_bytes()));
        if via == Via::Session && !header_ok {
            return Err(forbidden(
                "requests that change data with a session cookie need the header X-Requested-With: aurora",
            ));
        }
    }

    if let Some(touch) = found.touch {
        // Bookkeeping only; a failure must not fail the request.
        if let Err(err) = store
            .write(move |tx| auth::apply_touch(tx, now, &touch))
            .await
        {
            tracing::warn!(error = %err, "could not record that a credential was used");
        }
    }
    Ok((principal, via))
}

/// The layer on every protected route.
pub async fn require_auth(State(store): State<Store>, mut req: Request, next: Next) -> Response {
    // A request body is not Sync, so nothing borrowed from `req` may be held across an await.
    let method = req.method().clone();
    let headers = req.headers().clone();
    match authenticate(&store, &method, &headers).await {
        Ok((principal, via)) => {
            req.extensions_mut().insert(principal);
            req.extensions_mut().insert(via);
            next.run(req).await
        }
        Err(err) => err.into_response(),
    }
}

// ---- cookie ----

/// `Secure` is only added when a proxy says the request came in over HTTPS;
/// this server speaks plain HTTP itself (TLS and Secure-by-default are TRK-29).
fn session_cookie(
    headers: &HeaderMap,
    value: &str,
    max_age_secs: i64,
) -> Result<HeaderValue, ApiError> {
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age_secs}{secure}"
    ))
    .map_err(|_| StoreError::Panicked.into())
}

fn with_cookie(status: StatusCode, cookie: HeaderValue, body: Response) -> Response {
    let mut response = body;
    *response.status_mut() = status;
    response.headers_mut().append(header::SET_COOKIE, cookie);
    response
}

/// Responses that carry a secret must not be cached.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

// ---- setup and sign-in ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    name: String,
    password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("name", &self.name)
            .field("password", &"[redacted]")
            .finish()
    }
}

/// Argon2 takes tens of milliseconds of CPU: never on the async workers or the writer.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| StoreError::Panicked)
}

/// Shared with the HTML setup form.
pub(crate) async fn create_owner_account(
    store: &Store,
    name: String,
    password: String,
) -> ApiResult<User> {
    if store.read(auth::owner_exists).await? {
        return Err(StoreError::from(Rejection::Conflict("setup is already done".into())).into());
    }
    let hash = blocking(move || auth::hash_password(&password)).await??;
    let user = store
        .write(move |tx| auth::create_owner(tx, now_millis(), &name, &hash))
        .await?;
    Ok(user)
}

pub(crate) async fn setup(
    State(store): State<Store>,
    ApiJson(body): ApiJson<Credentials>,
) -> ApiResult<Response> {
    let Credentials { name, password } = body;
    Ok(created(create_owner_account(&store, name, password).await?))
}

/// Checks the password and opens a session; returns the user and the
/// `Set-Cookie` value. Shared with the HTML sign-in form.
pub(crate) async fn open_session_for(
    store: &Store,
    headers: &HeaderMap,
    name: String,
    password: String,
) -> ApiResult<(User, HeaderValue)> {
    let login = store.read(move |c| auth::login_lookup(c, &name)).await?;
    let user = blocking(move || auth::check_password(login.as_ref(), &password))
        .await?
        .ok_or_else(auth::sign_in_failed)?;
    let now = now_millis();
    let user_id = user.id;
    let session = store
        .write(move |tx| auth::open_session(tx, now, user_id))
        .await?;
    let max_age = (session.expires - now) / 1000;
    let cookie = session_cookie(headers, &session.token, max_age)?;
    Ok((user, cookie))
}

pub(crate) async fn sign_in(
    State(store): State<Store>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Credentials>,
) -> ApiResult<Response> {
    let Credentials { name, password } = body;
    let (user, cookie) = open_session_for(&store, &headers, name, password).await?;
    Ok(no_store(with_cookie(
        StatusCode::OK,
        cookie,
        Json(user).into_response(),
    )))
}

/// Ends the cookie session, if the request has one, and clears the cookie.
pub async fn sign_out(State(store): State<Store>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        let token = token.to_string();
        store.write(move |tx| auth::sign_out(tx, &token)).await?;
    }
    Ok(with_cookie(
        StatusCode::NO_CONTENT,
        session_cookie(&headers, "", 0)?,
        no_content(),
    ))
}

pub async fn me(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(via): Extension<Via>,
) -> ApiResult<Response> {
    let user: User = store
        .read(move |c| auth::get_user(c, principal.user_id))
        .await?;
    Ok(
        Json(json!({ "user": user, "scope": principal.scope, "via": via.as_str() }))
            .into_response(),
    )
}

// ---- users ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAgent {
    name: String,
}

pub(crate) async fn create_user(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    ApiJson(body): ApiJson<NewAgent>,
) -> ApiResult<Response> {
    let user = store
        .write(move |tx| auth::create_agent_user(tx, now_millis(), &principal, &body.name))
        .await?;
    Ok(created(user))
}

pub async fn list_users(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Response> {
    if !principal.is_owner() {
        return Err(forbidden("only the owner can list users"));
    }
    let users = store.read(auth::list_users).await?;
    Ok(Json(users).into_response())
}

// ---- keys ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysQuery {
    user_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKeyBody {
    name: String,
    /// Defaults to `write`.
    scope: Option<Scope>,
    /// Only the owner may name another user.
    user_id: Option<i64>,
}

pub(crate) async fn list_keys(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    ApiQuery(query): ApiQuery<KeysQuery>,
) -> ApiResult<Response> {
    let user_id = query.user_id.unwrap_or(principal.user_id);
    let keys: Vec<ApiKey> = store
        .read(move |c| auth::list_keys(c, &principal, user_id))
        .await?;
    Ok(Json(keys).into_response())
}

pub(crate) async fn create_key(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    ApiJson(body): ApiJson<NewKeyBody>,
) -> ApiResult<Response> {
    let user_id = body.user_id.unwrap_or(principal.user_id);
    let scope = body.scope.unwrap_or(Scope::Write);
    let key = store
        .write(move |tx| auth::create_key(tx, now_millis(), &principal, user_id, &body.name, scope))
        .await?;
    Ok(no_store(created(key)))
}

pub async fn rotate_key(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let key = store
        .write(move |tx| auth::rotate_key(tx, now_millis(), &principal, id))
        .await?;
    Ok(no_store(Json(key).into_response()))
}

pub async fn revoke_key(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    store
        .write(move |tx| auth::revoke_key(tx, now_millis(), &principal, id))
        .await?;
    Ok(no_content())
}
