//! The server-rendered pages (everything outside `/api/v1`): sign-in and
//! first-run setup, the project list, the board, the issue page, search
//! results (`/search`), the embedded assets and the live event stream.
//!
//! Pages are plain HTML from [`view`]; the little script in
//! `assets/app.js` talks to the JSON API and re-fetches the board, activity,
//! comments and initiative fragments (`/p/{key}/board`, `/p/{key}/{n}/activity`,
//! `/p/{key}/{n}/comments`, `/p/{key}/initiatives/list`,
//! `/p/{key}/initiatives/{id}/progress`, `/p/{key}/documents/list`,
//! `/p/{key}/documents/{id}/content`) when
//! `/events/stream` reports a change. Pages need the session cookie; a request
//! without one is redirected to `/sign-in` (or to `/setup` while there is no
//! owner), except fragment requests from the script, which get a 401.

mod assets;
mod data;
mod stream;
mod time;
mod view;

use aurora_store::auth::{self as store_auth, Principal};
use aurora_store::{Rejection, Store, StoreError};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Extension, Form, Router};
use maud::Markup;
use serde::Deserialize;

use crate::auth::{self, CSRF_HEADER};

pub use stream::{Live, LiveControl};

/// Everything is same-origin; no inline script or style.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; \
     frame-ancestors 'none'";

pub fn router(store: Store, live: Live) -> Router<Store> {
    let open = Router::new()
        .route("/sign-in", get(sign_in_page).post(sign_in_submit))
        .route("/setup", get(setup_page).post(setup_submit))
        .route("/assets/{name}", get(assets::serve_asset));
    let pages = Router::new()
        .route("/", get(home))
        .route("/search", get(search_page))
        .route("/agents", get(agents_page))
        .route("/agents/{id}", get(agent_page))
        .route("/p/{key}", get(board_page))
        .route("/p/{key}/board", get(board_fragment))
        .route("/p/{key}/initiatives", get(initiatives_page))
        .route("/p/{key}/initiatives/list", get(initiatives_fragment))
        .route("/p/{key}/initiatives/{id}", get(initiative_page))
        .route(
            "/p/{key}/initiatives/{id}/progress",
            get(initiative_progress_fragment),
        )
        .route("/p/{key}/documents", get(documents_page))
        .route("/p/{key}/documents/list", get(documents_fragment))
        .route("/p/{key}/documents/{id}", get(document_page))
        .route("/p/{key}/documents/{id}/content", get(document_fragment))
        .route("/p/{key}/documents/{id}/history", get(history_page))
        .route(
            "/p/{key}/documents/{id}/history/{number}",
            get(revision_page),
        )
        .route("/p/{key}/runs/{id}", get(run_page))
        .route("/p/{key}/runs/{id}/live", get(run_fragment))
        .route("/p/{key}/{number}", get(issue_page))
        .route("/p/{key}/{number}/runs", get(runs_fragment))
        .route("/p/{key}/{number}/activity", get(activity_fragment))
        .route("/p/{key}/{number}/comments", get(comments_fragment))
        .layer(middleware::from_fn_with_state(
            store.clone(),
            require_page_auth,
        ));
    let events = Router::new()
        .route("/events/stream", get(stream::events_stream))
        .layer(middleware::from_fn_with_state(store, auth::require_auth));
    open.merge(pages)
        .merge(events)
        .layer(Extension(live))
        .layer(middleware::map_response(security_headers))
}

async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

// ---- helpers ----

fn html_response(status: StatusCode, markup: Markup) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Html(markup.into_string()),
    )
        .into_response()
}

fn ok(markup: Markup) -> Response {
    html_response(StatusCode::OK, markup)
}

enum PageError {
    NotFound,
    Failed,
}

impl From<StoreError> for PageError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Rejected(Rejection::NotFound(_)) => PageError::NotFound,
            other => {
                tracing::error!(error = %other, "page could not be built");
                PageError::Failed
            }
        }
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        match self {
            PageError::NotFound => html_response(
                StatusCode::NOT_FOUND,
                view::message("Not found", "There is nothing at this address.", None),
            ),
            PageError::Failed => html_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                view::message("Something went wrong", "The page could not be built.", None),
            ),
        }
    }
}

type PageResult = Result<Response, PageError>;

/// Only a path on this site: it starts with one `/`, has no backslash and no
/// characters a header or a browser could treat specially.
fn local_path(next: Option<&str>) -> Option<&str> {
    next.filter(|n| {
        n.starts_with('/')
            && !n.starts_with("//")
            && n.len() <= 2000
            && n.bytes().all(|b| b.is_ascii_graphic() && b != b'\\')
    })
}

fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A form post from another site is refused; browsers send `Origin` on posts.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest);
    matches!((origin, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

fn forbidden_origin() -> Response {
    html_response(
        StatusCode::FORBIDDEN,
        view::message("Refused", "This request came from another site.", None),
    )
}

// ---- authentication for pages ----

async fn require_page_auth(State(store): State<Store>, mut req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let uri = req.uri().clone();
    match auth::authenticate(&store, &method, &headers).await {
        // The pages are the people's UI; a registered agent's key is limited
        // to its projects and cannot be checked page by page.
        Ok((_, _, access)) if !access.is_open() => html_response(
            StatusCode::FORBIDDEN,
            view::message(
                "Not allowed",
                "A registered agent's key cannot be used for the web pages.",
                None,
            ),
        ),
        Ok((principal, via, _)) => {
            req.extensions_mut().insert(principal);
            req.extensions_mut().insert(via);
            next.run(req).await
        }
        Err(err)
            if matches!(
                err.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) =>
        {
            unauthenticated(&store, &headers, &uri).await
        }
        Err(err) => err.into_response(),
    }
}

async fn unauthenticated(store: &Store, headers: &HeaderMap, uri: &Uri) -> Response {
    if headers.contains_key(CSRF_HEADER) {
        // A fetch from the script: a redirect to an HTML form is no use to it.
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    let owner_exists = store.read(store_auth::owner_exists).await.unwrap_or(true);
    if !owner_exists {
        return Redirect::to("/setup").into_response();
    }
    let target = uri.path_and_query().map_or("/", |pq| pq.as_str());
    Redirect::to(&format!("/sign-in?next={}", encode_component(target))).into_response()
}

// ---- sign-in and setup ----

#[derive(Deserialize)]
struct NextQuery {
    next: Option<String>,
}

#[derive(Deserialize)]
struct SignInForm {
    name: String,
    password: String,
    next: Option<String>,
}

#[derive(Deserialize)]
struct SetupForm {
    name: String,
    password: String,
}

fn signed_in(location: &str, cookie: HeaderValue) -> Response {
    let mut response = Redirect::to(location).into_response();
    let headers = response.headers_mut();
    headers.append(header::SET_COOKIE, cookie);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn sign_in_page(
    State(store): State<Store>,
    headers: HeaderMap,
    Query(query): Query<NextQuery>,
) -> PageResult {
    if !store.read(store_auth::owner_exists).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    let next = local_path(query.next.as_deref());
    if auth::authenticate(&store, &Method::GET, &headers)
        .await
        .is_ok()
    {
        return Ok(Redirect::to(next.unwrap_or("/")).into_response());
    }
    Ok(ok(view::sign_in(next, None)))
}

async fn sign_in_submit(
    State(store): State<Store>,
    headers: HeaderMap,
    Form(form): Form<SignInForm>,
) -> PageResult {
    if !same_origin(&headers) {
        return Ok(forbidden_origin());
    }
    let next = local_path(form.next.as_deref());
    match auth::open_session_for(&store, &headers, form.name, form.password).await {
        Ok((_, cookie)) => Ok(signed_in(next.unwrap_or("/"), cookie)),
        Err(err) => Ok(html_response(
            err.status(),
            view::sign_in(next, Some(err.message())),
        )),
    }
}

async fn setup_page(State(store): State<Store>) -> PageResult {
    if store.read(store_auth::owner_exists).await? {
        return Ok(Redirect::to("/sign-in").into_response());
    }
    Ok(ok(view::setup(None)))
}

async fn setup_submit(
    State(store): State<Store>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> PageResult {
    if !same_origin(&headers) {
        return Ok(forbidden_origin());
    }
    let SetupForm { name, password } = form;
    if let Err(err) = auth::create_owner_account(&store, name.clone(), password.clone()).await {
        return Ok(html_response(
            err.status(),
            view::setup(Some(err.message())),
        ));
    }
    match auth::open_session_for(&store, &headers, name, password).await {
        Ok((_, cookie)) => Ok(signed_in("/", cookie)),
        Err(_) => Ok(Redirect::to("/sign-in").into_response()),
    }
}

// ---- project pages ----

#[derive(Deserialize)]
struct HomeQuery {
    list: Option<String>,
}

async fn home(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HomeQuery>,
) -> PageResult {
    let page = store
        .read(move |c| data::load_home(c, principal.user_id))
        .await?;
    if let ([only], None) = (page.projects.as_slice(), &query.list) {
        return Ok(Redirect::to(&format!("/p/{}", only.key)).into_response());
    }
    Ok(ok(view::home(&page)))
}

/// Pages that show a key once must not be cached or stored.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn agents_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
) -> PageResult {
    let dir = crate::transcript::data_dir(&store);
    let data = store
        .read(move |c| {
            let mut data = data::load_agents(c, &principal)?;
            if principal.is_owner() {
                data.housekeeping = Some(crate::housekeeping::scan(c, &dir)?);
            }
            Ok(data)
        })
        .await?;
    Ok(no_store(ok(view::agents_page(&data))))
}

async fn agent_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(runner): Extension<Option<crate::Runner>>,
    Path(id): Path<i64>,
) -> PageResult {
    let mut data = store
        .read(move |c| data::load_agent(c, &principal, id))
        .await?;
    data.max_runs = runner.map(|runner| runner.max_runs());
    Ok(no_store(ok(view::agent_page(&data))))
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    project: Option<String>,
    kind: Option<String>,
    include_archived: Option<String>,
    page: Option<String>,
}

async fn search_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<SearchParams>,
) -> PageResult {
    let request = data::SearchRequest {
        q: params.q.unwrap_or_default(),
        project: params.project.filter(|p| !p.is_empty()),
        kind: params.kind,
        include_archived: params
            .include_archived
            .is_some_and(|v| !v.is_empty() && v != "0" && v != "false"),
        page: params
            .page
            .and_then(|p| p.parse::<i64>().ok())
            .map_or(1, |p| p.clamp(1, 10_000)),
    };
    let data = store
        .read(move |c| data::load_search(c, principal.user_id, request))
        .await?;
    let status = match &data.outcome {
        data::SearchOutcome::Refused { status, .. } => *status,
        _ => StatusCode::OK,
    };
    Ok(html_response(status, view::search_page(&data)))
}

async fn load_board(
    store: &Store,
    principal: Principal,
    key: String,
) -> Result<data::BoardData, PageError> {
    Ok(store
        .read(move |c| data::load_board(c, principal.user_id, &key))
        .await?)
}

async fn board_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<String>,
) -> PageResult {
    let board = load_board(&store, principal, key).await?;
    Ok(ok(view::board_page(&board)))
}

async fn board_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<String>,
) -> PageResult {
    let board = load_board(&store, principal, key).await?;
    Ok(ok(view::board_fragment(&board)))
}

async fn load_initiatives(
    store: &Store,
    principal: Principal,
    key: String,
) -> Result<data::InitiativesData, PageError> {
    Ok(store
        .read(move |c| data::load_initiatives(c, principal.user_id, &key))
        .await?)
}

async fn initiatives_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<String>,
) -> PageResult {
    let data = load_initiatives(&store, principal, key).await?;
    Ok(ok(view::initiatives_page(&data)))
}

async fn initiatives_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<String>,
) -> PageResult {
    let data = load_initiatives(&store, principal, key).await?;
    Ok(ok(view::initiatives_fragment(&data)))
}

async fn load_initiative(
    store: &Store,
    principal: Principal,
    key: String,
    id: i64,
) -> Result<data::InitiativeData, PageError> {
    Ok(store
        .read(move |c| data::load_initiative(c, principal.user_id, &key, id))
        .await?)
}

async fn initiative_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = load_initiative(&store, principal, key, id).await?;
    Ok(ok(view::initiative_page(&data)))
}

async fn initiative_progress_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = load_initiative(&store, principal, key, id).await?;
    Ok(ok(view::initiative_progress_fragment(&data)))
}

#[derive(Deserialize)]
struct DocumentsQuery {
    parent: Option<i64>,
}

async fn load_documents(
    store: &Store,
    principal: Principal,
    key: String,
    parent: Option<i64>,
) -> Result<data::DocumentsData, PageError> {
    Ok(store
        .read(move |c| data::load_documents(c, principal.user_id, &key, parent))
        .await?)
}

async fn documents_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<String>,
    Query(query): Query<DocumentsQuery>,
) -> PageResult {
    let data = load_documents(&store, principal, key, query.parent).await?;
    Ok(ok(view::documents_page(&data)))
}

async fn documents_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<String>,
) -> PageResult {
    let data = load_documents(&store, principal, key, None).await?;
    Ok(ok(view::documents_fragment(&data)))
}

async fn load_document(
    store: &Store,
    principal: Principal,
    key: String,
    id: i64,
) -> Result<data::DocumentData, PageError> {
    Ok(store
        .read(move |c| data::load_document(c, principal.user_id, &key, id))
        .await?)
}

async fn document_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = load_document(&store, principal, key, id).await?;
    Ok(ok(view::document_page(&data)))
}

async fn document_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = load_document(&store, principal, key, id).await?;
    Ok(ok(view::document_fragment(&data)))
}

async fn history_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = store
        .read(move |c| data::load_history(c, principal.user_id, &key, id))
        .await?;
    Ok(ok(view::history_page(&data)))
}

async fn revision_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id, number)): Path<(String, i64, i64)>,
) -> PageResult {
    let data = store
        .read(move |c| data::load_revision(c, principal.user_id, &key, id, number))
        .await?;
    Ok(ok(view::revision_page(&data)))
}

async fn issue_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
) -> PageResult {
    let issue = store
        .read(move |c| data::load_issue(c, principal.user_id, &key, number))
        .await?;
    Ok(ok(view::issue_page(&issue)))
}

async fn runs_fragment(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> PageResult {
    let data = store
        .read(move |c| data::load_issue_runs(c, &key, number))
        .await?;
    Ok(ok(view::runs_fragment(&data)))
}

async fn load_run(
    store: &Store,
    principal: Principal,
    key: String,
    id: i64,
) -> Result<data::RunPageData, PageError> {
    Ok(store
        .read(move |c| data::load_run(c, principal.user_id, &key, id))
        .await?)
}

async fn run_page(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = load_run(&store, principal, key, id).await?;
    Ok(no_store(ok(view::run_page(&data))))
}

async fn run_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> PageResult {
    let data = load_run(&store, principal, key, id).await?;
    Ok(no_store(ok(view::run_fragment(&data))))
}

async fn activity_fragment(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> PageResult {
    let activity = store
        .read(move |c| data::load_activity(c, &key, number))
        .await?;
    Ok(ok(view::activity_fragment(&activity)))
}

async fn comments_fragment(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
) -> PageResult {
    let data = store
        .read(move |c| data::load_comments(c, principal.user_id, &key, number))
        .await?;
    Ok(ok(view::comments_fragment(&data)))
}
