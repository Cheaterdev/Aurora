//! The REST API under `/api/v1`. Handlers only translate; the rules live in
//! `aurora_store::projects` and `aurora_store::issues`.
//!
//! Every API route is built in [`api_router`]. All of them sit behind the
//! authentication layer in [`crate::auth`] except `POST /setup` and
//! `POST /auth/sign-in`; `/health` lives outside the API.

use std::path::Path as FsPath;

use aurora_store::auth::{Principal, Role};
use aurora_store::comments::{self, NewComment};
use aurora_store::documents::{self, DocumentMove, DocumentPatch, DocumentTarget, NewDocument};
use aurora_store::events::{self, EventFilter, Start};
use aurora_store::initiatives::{self, InitiativePatch, InitiativeStatus, NewInitiative};
use aurora_store::issues::{self, IssueFilter, IssuePatch, MoveTarget, NewIssue, ParentFilter};
use aurora_store::links::{self, LinkKind};
use aurora_store::projects::{self, NewColumn, StateType};
use aurora_store::search::{self, Kind, SearchQuery};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post, put};
use axum::{Extension, Json, Router, middleware};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::Access;
use crate::gitwork::Git;
use crate::{agents, auth, idem, markdown, prompts, runs};

const DEFAULT_LABEL_COLOR: &str = "#808080";

/// One method of one route under `/api/v1`. `path` is relative to `/api/v1`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Endpoint {
    pub method: &'static str,
    pub path: &'static str,
    /// Reachable without credentials.
    pub public: bool,
}

/// A router that remembers what was registered on it, so the route table
/// that `GET /api/v1` serves and that the documentation test checks is the
/// registration itself. The `methods` of a route are checked against the real
/// router by `tests/api_routes.rs`, so they cannot drift from the handlers.
struct Table {
    router: Router<Store>,
    endpoints: Vec<Endpoint>,
    public: bool,
}

impl Table {
    fn new(public: bool) -> Self {
        Table {
            router: Router::new(),
            endpoints: Vec::new(),
            public,
        }
    }

    fn route(
        mut self,
        path: &'static str,
        methods: &'static [&'static str],
        handler: MethodRouter<Store>,
    ) -> Self {
        for &method in methods {
            self.endpoints.push(Endpoint {
                method,
                path,
                public: self.public,
            });
        }
        self.router = self.router.route(path, handler);
        self
    }
}

fn api_tables() -> (Table, Table) {
    let public = Table::new(true)
        .route("/setup", &["POST"], post(auth::setup))
        .route("/auth/sign-in", &["POST"], post(auth::sign_in));
    let protected = Table::new(false)
        .route("/", &["GET"], get(api_index))
        .route("/auth/sign-out", &["POST"], post(auth::sign_out))
        .route("/auth/me", &["GET"], get(auth::me))
        .route(
            "/keys",
            &["GET", "POST"],
            get(auth::list_keys).post(auth::create_key),
        )
        .route("/keys/{id}", &["DELETE"], delete(auth::revoke_key))
        .route("/keys/{id}/rotate", &["POST"], post(auth::rotate_key))
        .route(
            "/users",
            &["GET", "POST"],
            get(auth::list_users).post(auth::create_user),
        )
        .route(
            "/agents",
            &["GET", "POST"],
            get(agents::list_agents).post(agents::create_agent),
        )
        .route(
            "/agents/{id}",
            &["GET", "PATCH", "DELETE"],
            get(agents::get_agent)
                .patch(agents::update_agent)
                .delete(agents::delete_agent),
        )
        .route("/agents/{id}/enable", &["POST"], post(agents::enable_agent))
        .route(
            "/agents/{id}/disable",
            &["POST"],
            post(agents::disable_agent),
        )
        .route(
            "/agents/{id}/templates",
            &["GET", "PUT"],
            get(prompts::list_templates).put(prompts::save_template),
        )
        .route(
            "/agents/{id}/templates/{template_id}",
            &["DELETE"],
            delete(prompts::delete_template),
        )
        .route(
            "/agents/{id}/prompt-preview",
            &["POST"],
            post(prompts::preview),
        )
        .route("/agents/{id}/runs", &["POST"], post(runs::start_run))
        .route("/runs", &["GET"], get(runs::list_runs))
        .route("/runs/{id}", &["GET"], get(runs::get_run))
        .route("/runs/{id}/cancel", &["POST"], post(runs::cancel_run))
        .route("/runs/{id}/transcript", &["GET"], get(runs::get_transcript))
        .route(
            "/runs/{id}/transcript.txt",
            &["GET"],
            get(runs::get_transcript_text),
        )
        .route("/prompt/variables", &["GET"], get(prompts::variables))
        .route(
            "/projects",
            &["GET", "POST"],
            get(list_projects).post(create_project),
        )
        .route(
            "/projects/{key}",
            &["GET", "PATCH"],
            get(get_project).patch(update_project),
        )
        .route(
            "/projects/{key}/columns",
            &["GET", "POST"],
            get(list_columns).post(create_column),
        )
        .route(
            "/projects/{key}/columns/order",
            &["PUT"],
            put(reorder_columns),
        )
        .route(
            "/projects/{key}/columns/{id}",
            &["PATCH", "DELETE"],
            patch(rename_column).delete(delete_column),
        )
        .route(
            "/projects/{key}/issue-types",
            &["GET", "POST"],
            get(list_issue_types).post(create_issue_type),
        )
        .route(
            "/projects/{key}/issue-types/{id}",
            &["PATCH", "DELETE"],
            patch(rename_issue_type).delete(delete_issue_type),
        )
        .route(
            "/projects/{key}/labels",
            &["GET", "POST"],
            get(list_labels).post(create_label),
        )
        .route(
            "/projects/{key}/labels/{id}",
            &["PATCH", "DELETE"],
            patch(update_label).delete(delete_label),
        )
        .route(
            "/projects/{key}/issues",
            &["GET", "POST"],
            get(list_issues).post(create_issue),
        )
        .route(
            "/projects/{key}/issues/{number}",
            &["GET", "PATCH"],
            get(get_issue).patch(update_issue),
        )
        .route(
            "/projects/{key}/issues/{number}/move",
            &["POST"],
            post(move_issue),
        )
        .route(
            "/projects/{key}/issues/{number}/archive",
            &["POST"],
            post(archive_issue),
        )
        .route(
            "/projects/{key}/issues/{number}/unarchive",
            &["POST"],
            post(unarchive_issue),
        )
        .route(
            "/projects/{key}/issues/{number}/events",
            &["GET"],
            get(issue_events),
        )
        .route(
            "/projects/{key}/issues/{number}/links",
            &["GET", "POST"],
            get(list_links).post(add_link),
        )
        .route(
            "/projects/{key}/links/{id}",
            &["DELETE"],
            delete(remove_link),
        )
        .route(
            "/projects/{key}/issues/{number}/comments",
            &["GET", "POST"],
            get(list_comments).post(add_comment),
        )
        .route(
            "/projects/{key}/comments/{id}",
            &["GET", "PATCH", "DELETE"],
            get(get_comment).patch(edit_comment).delete(delete_comment),
        )
        .route(
            "/projects/{key}/initiatives",
            &["GET", "POST"],
            get(list_initiatives).post(create_initiative),
        )
        .route(
            "/projects/{key}/initiatives/{id}",
            &["GET", "PATCH", "DELETE"],
            get(get_initiative)
                .patch(update_initiative)
                .delete(delete_initiative),
        )
        .route(
            "/projects/{key}/documents",
            &["GET", "POST"],
            get(list_documents).post(create_document),
        )
        .route(
            "/projects/{key}/documents/preview",
            &["POST"],
            post(preview_document),
        )
        .route(
            "/projects/{key}/documents/{id}",
            &["GET", "PATCH"],
            get(get_document).patch(update_document),
        )
        .route(
            "/projects/{key}/documents/{id}/move",
            &["POST"],
            post(move_document),
        )
        .route(
            "/projects/{key}/documents/{id}/archive",
            &["POST"],
            post(archive_document),
        )
        .route(
            "/projects/{key}/documents/{id}/unarchive",
            &["POST"],
            post(unarchive_document),
        )
        .route(
            "/projects/{key}/documents/{id}/revisions",
            &["GET"],
            get(list_revisions),
        )
        .route(
            "/projects/{key}/documents/{id}/revisions/{number}",
            &["GET"],
            get(get_revision),
        )
        .route(
            "/projects/{key}/documents/{id}/restore",
            &["POST"],
            post(restore_revision),
        )
        .route(
            "/projects/{key}/documents/{id}/links",
            &["POST"],
            post(add_document_link),
        )
        .route(
            "/projects/{key}/documents/{id}/links/{link_id}",
            &["DELETE"],
            delete(remove_document_link),
        )
        .route("/search", &["GET"], get(search_all))
        .route("/events", &["GET"], get(list_events))
        .route("/consumers", &["GET"], get(list_consumers))
        .route(
            "/consumers/{name}",
            &["PUT", "DELETE"],
            put(register_consumer).delete(delete_consumer),
        )
        .route("/consumers/{name}/events", &["GET"], get(poll_consumer))
        .route("/consumers/{name}/ack", &["POST"], post(ack_consumer));
    (public, protected)
}

/// Every method of every route under `/api/v1`, in registration order.
pub fn api_endpoints() -> Vec<Endpoint> {
    let (public, protected) = api_tables();
    public
        .endpoints
        .into_iter()
        .chain(protected.endpoints)
        .collect()
}

/// `store` is only needed to build the authentication layer.
pub fn api_router(store: Store) -> Router<Store> {
    let (public, protected) = api_tables();
    // Layers added later run first: authentication, then the request id.
    let protected = protected
        .router
        .layer(middleware::from_fn(idem::layer))
        .layer(middleware::from_fn_with_state(store, auth::require_auth));
    public.router.merge(protected)
}

/// The route table, so a client can discover the API; see `docs/api.md`.
async fn api_index() -> Json<serde_json::Value> {
    Json(json!({
        "name": "aurora",
        "version": env!("CARGO_PKG_VERSION"),
        "docs": "docs/api.md",
        "mcp": "/mcp",
        "endpoints": api_endpoints(),
    }))
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        let (status, message) = match err {
            StoreError::Rejected(Rejection::NotFound(what)) => {
                (StatusCode::NOT_FOUND, format!("{what} not found"))
            }
            StoreError::Rejected(Rejection::Conflict(reason)) => (StatusCode::CONFLICT, reason),
            StoreError::Rejected(Rejection::Invalid(reason)) => {
                (StatusCode::UNPROCESSABLE_ENTITY, reason)
            }
            StoreError::Rejected(Rejection::Unauthorized(reason)) => {
                (StatusCode::UNAUTHORIZED, reason)
            }
            StoreError::Rejected(Rejection::Forbidden(reason)) => (StatusCode::FORBIDDEN, reason),
            other => {
                tracing::error!(error = %other, "store operation failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        ApiError { status, message }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": self.message }))).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// `Json` whose rejections use the API's error body.
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection @ JsonRejection::MissingJsonContentType(_)) => Err(ApiError {
                status: rejection.status(),
                message: "expected a JSON body (Content-Type: application/json)".into(),
            }),
            Err(rejection) => Err(ApiError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message: format!("invalid JSON body: {}", rejection.body_text()),
            }),
        }
    }
}

/// `Query` whose rejections use the API's error body.
pub(crate) struct ApiQuery<T>(pub(crate) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(ApiQuery(value)),
            Err(rejection) => Err(ApiError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message: format!("invalid query: {}", rejection.body_text()),
            }),
        }
    }
}

fn invalid(reason: String) -> ApiError {
    StoreError::from(Rejection::Invalid(reason)).into()
}

fn parse_state_type(text: &str) -> Result<StateType, ApiError> {
    StateType::parse(text).ok_or_else(|| {
        invalid(format!(
            "unknown state_type {text}; use backlog, unstarted, started, completed or canceled"
        ))
    })
}

pub(crate) fn created<T: serde::Serialize>(value: T) -> Response {
    (StatusCode::CREATED, Json(value)).into_response()
}

pub(crate) fn no_content() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::CONTENT_TYPE, "application/json")],
    )
        .into_response()
}

// ---- projects ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProject {
    key: String,
    name: String,
    columns: Option<Vec<NewColumn>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody {
    name: String,
}

/// A project as the caller may see it: people also get the repository
/// settings, an agent's key never does.
fn project_view(project: &projects::Project, reader: &Principal) -> serde_json::Value {
    if reader.role == Role::Agent {
        serde_json::to_value(project).unwrap_or(serde_json::Value::Null)
    } else {
        project.for_people()
    }
}

/// A restricted agent sees only the projects on its allow-list.
async fn list_projects(
    State(store): State<Store>,
    Extension(reader): Extension<Principal>,
    Extension(access): Extension<Access>,
) -> ApiResult<Response> {
    let mut projects = store.read(projects::list_projects).await?;
    projects.retain(|project| access.allows(&project.key));
    let views: Vec<serde_json::Value> = projects
        .iter()
        .map(|project| project_view(project, &reader))
        .collect();
    Ok(Json(views).into_response())
}

async fn create_project(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Extension(access): Extension<Access>,
    ApiJson(body): ApiJson<NewProject>,
) -> ApiResult<Response> {
    if !access.is_open() {
        return Err(StoreError::from(Rejection::Forbidden(
            "this agent may not create projects; it is limited to its allowed projects".into(),
        ))
        .into());
    }
    idem::json(&store, StatusCode::CREATED, move |tx| {
        projects::create_project(
            tx,
            now_millis(),
            &actor,
            &body.key,
            &body.name,
            body.columns.as_deref(),
        )
    })
    .await
}

async fn get_project(
    State(store): State<Store>,
    Extension(reader): Extension<Principal>,
    Path(key): Path<String>,
) -> ApiResult<Response> {
    let project = store.read(move |c| projects::get_project(c, &key)).await?;
    Ok(Json(project_view(&project, &reader)).into_response())
}

/// Checks the repository settings of a patch against git, before anything is
/// written, and replaces a repository path by the repository's canonical top
/// level folder. The store rules (owner only, lengths) apply afterwards.
async fn check_repository_patch(
    store: &Store,
    key: &str,
    patch: &mut projects::ProjectPatch,
) -> ApiResult<()> {
    let git = Git::system();
    let project = {
        let key = key.to_string();
        store.read(move |c| projects::get_project(c, &key)).await?
    };
    let blank = |text: &Option<String>| text.as_deref().is_none_or(|t| t.trim().is_empty());
    if let Some(path) = &patch.repo_path
        && !blank(path)
    {
        let given = path.as_deref().unwrap_or_default().trim().to_string();
        let top = git.check_repository(&given).await.map_err(invalid)?;
        patch.repo_path = Some(Some(top));
    }
    let repo = match &patch.repo_path {
        Some(path) if blank(path) => None,
        Some(path) => path.clone(),
        None => project.repo_path.clone(),
    };
    let base = match &patch.base_branch {
        Some(branch) if blank(branch) => None,
        Some(branch) => branch.as_deref().map(|b| b.trim().to_string()),
        None => project.base_branch.clone(),
    };
    let moved = patch.repo_path.is_some() || patch.base_branch.is_some();
    if let (true, Some(base)) = (moved, &base) {
        let Some(repo) = &repo else {
            return Err(invalid(
                "set repo_path before base_branch: there is no repository to take it from".into(),
            ));
        };
        git.check_branch(FsPath::new(repo), base).await.map_err(|why| {
            if patch.base_branch.is_some() {
                invalid(why)
            } else {
                invalid(format!(
                    "{why}; this repository does not have the project's base branch, change base_branch too"
                ))
            }
        })?;
    }
    Ok(())
}

/// Changes the name, the rules text, the repository settings, or any of them,
/// in one write. The repository settings are for the owner and are checked
/// against git first.
async fn update_project(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(mut patch): ApiJson<projects::ProjectPatch>,
) -> ApiResult<Response> {
    if patch.touches_repo() {
        if !actor.is_owner() {
            return Err(StoreError::from(Rejection::Forbidden(
                "only the owner can change the repository settings of a project".into(),
            ))
            .into());
        }
        check_repository_patch(&store, &key, &mut patch).await?;
    }
    idem::json(&store, StatusCode::OK, move |tx| {
        let project = projects::update_project(tx, now_millis(), &actor, &key, patch)?;
        Ok(project_view(&project, &actor))
    })
    .await
}

// ---- columns ----

#[derive(Deserialize)]
struct ColumnFilter {
    state_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewColumnBody {
    name: String,
    state_type: StateType,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderBody {
    ids: Vec<i64>,
}

async fn list_columns(
    State(store): State<Store>,
    Path(key): Path<String>,
    Query(filter): Query<ColumnFilter>,
) -> ApiResult<Response> {
    let state_type = filter
        .state_type
        .as_deref()
        .map(parse_state_type)
        .transpose()?;
    let columns = store
        .read(move |c| match state_type {
            Some(state_type) => projects::columns_by_state_type(c, &key, state_type),
            None => projects::list_columns(c, &key),
        })
        .await?;
    Ok(Json(columns).into_response())
}

async fn create_column(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewColumnBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        projects::create_column(tx, now_millis(), &actor, &key, &body.name, body.state_type)
    })
    .await
}

async fn rename_column(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        projects::rename_column(tx, now_millis(), &actor, &key, id, &body.name)
    })
    .await
}

async fn reorder_columns(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<OrderBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        projects::reorder_columns(tx, now_millis(), &actor, &key, &body.ids)
    })
    .await
}

async fn delete_column(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        projects::delete_column(tx, now_millis(), &actor, &key, id)
    })
    .await
}

// ---- issue types ----

async fn list_issue_types(
    State(store): State<Store>,
    Path(key): Path<String>,
) -> ApiResult<Response> {
    let types = store
        .read(move |c| projects::list_issue_types(c, &key))
        .await?;
    Ok(Json(types).into_response())
}

async fn create_issue_type(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        projects::create_issue_type(tx, now_millis(), &actor, &key, &body.name)
    })
    .await
}

async fn rename_issue_type(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        projects::rename_issue_type(tx, now_millis(), &actor, &key, id, &body.name)
    })
    .await
}

async fn delete_issue_type(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        projects::delete_issue_type(tx, now_millis(), &actor, &key, id)
    })
    .await
}

// ---- labels ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLabelBody {
    name: String,
    color: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelPatch {
    name: Option<String>,
    color: Option<String>,
}

async fn list_labels(State(store): State<Store>, Path(key): Path<String>) -> ApiResult<Response> {
    let labels = store.read(move |c| projects::list_labels(c, &key)).await?;
    Ok(Json(labels).into_response())
}

async fn create_label(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewLabelBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        let color = body.color.as_deref().unwrap_or(DEFAULT_LABEL_COLOR);
        projects::create_label(tx, now_millis(), &actor, &key, &body.name, color)
    })
    .await
}

async fn update_label(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<LabelPatch>,
) -> ApiResult<Response> {
    if body.name.is_none() && body.color.is_none() {
        return Err(
            StoreError::from(Rejection::Invalid("give a name, a color or both".into())).into(),
        );
    }
    // One transaction, so a bad color does not leave the rename behind.
    idem::json(&store, StatusCode::OK, move |tx| {
        let now = now_millis();
        let mut label = None;
        if let Some(name) = &body.name {
            label = Some(projects::rename_label(tx, now, &actor, &key, id, name)?);
        }
        if let Some(color) = &body.color {
            label = Some(projects::recolor_label(tx, now, &actor, &key, id, color)?);
        }
        label.ok_or_else(|| StoreError::from(Rejection::NotFound("label".into())))
    })
    .await
}

async fn delete_label(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        projects::delete_label(tx, now_millis(), &actor, &key, id)
    })
    .await
}

// ---- issues ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueQuery {
    column_id: Option<i64>,
    state_type: Option<String>,
    label_id: Option<i64>,
    assignee_id: Option<i64>,
    /// An issue number, or `none` for top-level issues.
    parent: Option<String>,
    include_archived: Option<bool>,
    archived_only: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl IssueQuery {
    fn into_filter(self) -> Result<IssueFilter, ApiError> {
        let parent = match self.parent.as_deref() {
            None => None,
            Some("none") => Some(ParentFilter::None),
            Some(text) => Some(ParentFilter::Number(text.parse().map_err(|_| {
                invalid(format!(
                    "parent must be an issue number or none, not {text}"
                ))
            })?)),
        };
        Ok(IssueFilter {
            column_id: self.column_id,
            state_type: self
                .state_type
                .as_deref()
                .map(parse_state_type)
                .transpose()?,
            label_id: self.label_id,
            assignee_id: self.assignee_id,
            parent,
            include_archived: self.include_archived.unwrap_or(false),
            archived_only: self.archived_only.unwrap_or(false),
            limit: self.limit,
            offset: self.offset,
        })
    }
}

async fn list_issues(
    State(store): State<Store>,
    Path(key): Path<String>,
    ApiQuery(query): ApiQuery<IssueQuery>,
) -> ApiResult<Response> {
    let filter = query.into_filter()?;
    let list = store
        .read(move |c| issues::list_issues(c, &key, &filter))
        .await?;
    Ok(Json(list).into_response())
}

async fn create_issue(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewIssue>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        issues::create_issue(tx, now_millis(), &actor, &key, body)
    })
    .await
}

async fn get_issue(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let issue = store
        .read(move |c| issues::get_issue(c, &key, number))
        .await?;
    Ok(Json(issue).into_response())
}

async fn update_issue(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
    ApiJson(body): ApiJson<IssuePatch>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        issues::update_issue(tx, now_millis(), &actor, &key, number, body)
    })
    .await
}

async fn move_issue(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
    ApiJson(body): ApiJson<MoveTarget>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        issues::move_issue(tx, now_millis(), &actor, &key, number, body)
    })
    .await
}

async fn archive_issue(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        issues::archive_issue(tx, now_millis(), &actor, &key, number)
    })
    .await
}

async fn unarchive_issue(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        issues::unarchive_issue(tx, now_millis(), &actor, &key, number)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityQuery {
    limit: Option<i64>,
}

/// The newest events about one issue, oldest first.
async fn issue_events(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
    ApiQuery(query): ApiQuery<ActivityQuery>,
) -> ApiResult<Response> {
    let list = store
        .read(move |c| {
            let detail = issues::get_issue(c, &key, number)?;
            events::entity_events(c, "issue", detail.issue.id, query.limit.unwrap_or(50))
        })
        .await?;
    Ok(Json(json!({ "events": list })).into_response())
}

// ---- links ----

/// The other issue: a number in this project, or `KEY-n` (which must name this project).
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum LinkTarget {
    Number(i64),
    Identifier(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LinkDirection {
    /// This issue blocks `to`.
    Blocks,
    /// `to` blocks this issue.
    BlockedBy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLinkBody {
    kind: LinkKind,
    to: LinkTarget,
    direction: Option<LinkDirection>,
}

impl LinkTarget {
    pub(crate) fn number_in(self, key: &str) -> Result<i64, ApiError> {
        match self {
            LinkTarget::Number(number) => Ok(number),
            LinkTarget::Identifier(text) => {
                let (other, number) = text
                    .rsplit_once('-')
                    .and_then(|(k, n)| Some((k, n.parse::<i64>().ok()?)))
                    .ok_or_else(|| {
                        invalid(format!("to must be an issue number or KEY-n, not {text}"))
                    })?;
                if other == key {
                    Ok(number)
                } else {
                    Err(invalid(format!(
                        "links must connect issues of the same project; {text} is not in {key}"
                    )))
                }
            }
        }
    }
}

async fn list_links(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let found = store
        .read(move |c| links::list_links(c, &key, number))
        .await?;
    Ok(Json(found).into_response())
}

async fn add_link(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
    ApiJson(body): ApiJson<NewLinkBody>,
) -> ApiResult<Response> {
    let to = body.to.number_in(&key)?;
    let blocked_by = matches!(body.direction, Some(LinkDirection::BlockedBy));
    if blocked_by && body.kind != LinkKind::Blocks {
        return Err(invalid(
            "direction blocked_by only applies to kind blocks".into(),
        ));
    }
    idem::json(&store, StatusCode::CREATED, move |tx| {
        let now = now_millis();
        if blocked_by {
            links::add_blocker(tx, now, &actor, &key, number, to)
        } else {
            links::add_link(tx, now, &actor, &key, number, to, body.kind)
        }
    })
    .await
}

async fn remove_link(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        links::remove_link(tx, now_millis(), &actor, &key, id)
    })
    .await
}

// ---- comments ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentBody {
    body: String,
}

async fn list_comments(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let found = store
        .read(move |c| comments::list_comments(c, &key, number))
        .await?;
    Ok(Json(found).into_response())
}

async fn add_comment(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, number)): Path<(String, i64)>,
    ApiJson(body): ApiJson<NewComment>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        comments::add_comment(tx, now_millis(), &actor, &key, number, body)
    })
    .await
}

async fn get_comment(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let comment = store
        .read(move |c| comments::get_comment(c, &key, id))
        .await?;
    Ok(Json(comment).into_response())
}

async fn edit_comment(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<CommentBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        comments::edit_comment(tx, now_millis(), &actor, &key, id, &body.body)
    })
    .await
}

async fn delete_comment(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        comments::delete_comment(tx, now_millis(), &actor, &key, id)
    })
    .await
}

// ---- initiatives ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitiativeQuery {
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitiativeIssuesQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_initiatives(
    State(store): State<Store>,
    Path(key): Path<String>,
    ApiQuery(query): ApiQuery<InitiativeQuery>,
) -> ApiResult<Response> {
    let status = query
        .status
        .as_deref()
        .map(|text| {
            InitiativeStatus::parse(text).ok_or_else(|| {
                invalid(format!(
                    "unknown status {text}; use planned, active, paused, completed or canceled"
                ))
            })
        })
        .transpose()?;
    let list = store
        .read(move |c| initiatives::list_initiatives(c, now_millis(), &key, status))
        .await?;
    Ok(Json(list).into_response())
}

async fn create_initiative(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewInitiative>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        initiatives::create_initiative(tx, now_millis(), &actor, &key, body)
    })
    .await
}

async fn get_initiative(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
    ApiQuery(query): ApiQuery<InitiativeIssuesQuery>,
) -> ApiResult<Response> {
    let detail = store
        .read(move |c| {
            initiatives::get_initiative(c, now_millis(), &key, id, query.limit, query.offset)
        })
        .await?;
    Ok(Json(detail).into_response())
}

async fn update_initiative(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<InitiativePatch>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        initiatives::update_initiative(tx, now_millis(), &actor, &key, id, body)
    })
    .await
}

async fn delete_initiative(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        initiatives::delete_initiative(tx, now_millis(), &actor, &key, id)
    })
    .await
}

// ---- documents ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentQuery {
    include_archived: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    revision: i64,
    base_revision: Option<i64>,
}

/// Exactly one of the two.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentLinkBody {
    issue: Option<i64>,
    initiative: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBody {
    body: String,
}

async fn list_documents(
    State(store): State<Store>,
    Path(key): Path<String>,
    ApiQuery(query): ApiQuery<DocumentQuery>,
) -> ApiResult<Response> {
    let include_archived = query.include_archived.unwrap_or(false);
    let list = store
        .read(move |c| documents::list_documents(c, &key, include_archived))
        .await?;
    Ok(Json(list).into_response())
}

async fn create_document(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewDocument>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::CREATED, move |tx| {
        documents::create_document(tx, now_millis(), &actor, &key, body)
    })
    .await
}

async fn get_document(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let document = store
        .read(move |c| documents::get_document(c, &key, id))
        .await?;
    Ok(Json(document).into_response())
}

async fn update_document(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<DocumentPatch>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        documents::update_document(tx, now_millis(), &actor, &key, id, body)
    })
    .await
}

async fn move_document(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<DocumentMove>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        documents::move_document(tx, now_millis(), &actor, &key, id, body)
    })
    .await
}

async fn archive_document(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        documents::archive_document(tx, now_millis(), &actor, &key, id)
    })
    .await
}

async fn unarchive_document(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        documents::unarchive_document(tx, now_millis(), &actor, &key, id)
    })
    .await
}

async fn list_revisions(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let list = store
        .read(move |c| documents::list_revisions(c, &key, id))
        .await?;
    Ok(Json(list).into_response())
}

async fn get_revision(
    State(store): State<Store>,
    Path((key, id, number)): Path<(String, i64, i64)>,
) -> ApiResult<Response> {
    let revision = store
        .read(move |c| documents::get_revision(c, &key, id, number))
        .await?;
    Ok(Json(revision).into_response())
}

async fn restore_revision(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<RestoreBody>,
) -> ApiResult<Response> {
    idem::json(&store, StatusCode::OK, move |tx| {
        documents::restore_revision(
            tx,
            now_millis(),
            &actor,
            &key,
            id,
            body.revision,
            body.base_revision,
        )
    })
    .await
}

async fn add_document_link(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<DocumentLinkBody>,
) -> ApiResult<Response> {
    let target = match (body.issue, body.initiative) {
        (Some(number), None) => DocumentTarget::Issue(number),
        (None, Some(initiative)) => DocumentTarget::Initiative(initiative),
        _ => return Err(invalid("give exactly one of issue and initiative".into())),
    };
    idem::json(&store, StatusCode::CREATED, move |tx| {
        documents::add_link(tx, now_millis(), &actor, &key, id, target)
    })
    .await
}

async fn remove_document_link(
    State(store): State<Store>,
    Extension(actor): Extension<Principal>,
    Path((key, id, link_id)): Path<(String, i64, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        documents::remove_link(tx, now_millis(), &actor, &key, id, link_id)
    })
    .await
}

/// The server-rendered HTML of a draft body, for the editor's Preview button;
/// the same renderer as the document page, so what is previewed is what is shown.
async fn preview_document(
    State(store): State<Store>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<PreviewBody>,
) -> ApiResult<Response> {
    store.read(move |c| projects::get_project(c, &key)).await?;
    if body.body.chars().count() > documents::MAX_BODY_CHARS {
        return Err(invalid(format!(
            "body must be at most {} characters",
            documents::MAX_BODY_CHARS
        )));
    }
    let html = markdown::render(&body.body).into_string();
    Ok(Json(json!({ "html": html })).into_response())
}

// ---- events and consumers ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventQuery {
    after: Option<i64>,
    limit: Option<i64>,
    project: Option<String>,
    entity_kind: Option<String>,
    kind_prefix: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollQuery {
    limit: Option<i64>,
    project: Option<String>,
    entity_kind: Option<String>,
    kind_prefix: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartBody {
    start: StartAt,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum StartAt {
    Beginning,
    Latest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckBody {
    up_to: i64,
}

fn event_filter(
    project: Option<String>,
    entity_kind: Option<String>,
    kind_prefix: Option<String>,
) -> EventFilter {
    EventFilter {
        project,
        entity_kind,
        kind_prefix,
    }
}

async fn list_events(
    State(store): State<Store>,
    Extension(access): Extension<Access>,
    ApiQuery(query): ApiQuery<EventQuery>,
) -> ApiResult<Response> {
    if let Some(key) = &query.project {
        access.check_project(key)?;
    }
    let allowed: Option<Vec<String>> = access.allowed().map(<[String]>::to_vec);
    let filter = event_filter(query.project, query.entity_kind, query.kind_prefix);
    let list = store
        .read(move |c| {
            events::read_events_in(
                c,
                query.after.unwrap_or(0),
                query.limit,
                &filter,
                allowed.as_deref(),
            )
        })
        .await?;
    Ok(Json(json!({ "events": list })).into_response())
}

// ---- search ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    q: Option<String>,
    project: Option<String>,
    /// Comma separated: `issue`, `comment`, `document`.
    kind: Option<String>,
    include_archived: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn search_all(
    State(store): State<Store>,
    Extension(access): Extension<Access>,
    ApiQuery(params): ApiQuery<SearchParams>,
) -> ApiResult<Response> {
    if let Some(key) = &params.project {
        access.check_project(key)?;
    }
    let allowed: Option<Vec<String>> = access.allowed().map(<[String]>::to_vec);
    let kinds = params.kind.as_deref().map(Kind::parse_list).transpose()?;
    let query = SearchQuery {
        q: params.q.unwrap_or_default(),
        project: params.project,
        kinds,
        include_archived: params.include_archived.unwrap_or(false),
        limit: params.limit,
        offset: params.offset,
    };
    let echo = query.q.clone();
    let limit = query.limit.unwrap_or(search::DEFAULT_LIMIT);
    let offset = query.offset.unwrap_or(0);
    let results = store
        .read(move |c| search::search_in(c, &query, allowed.as_deref()))
        .await?;
    Ok(Json(json!({
        "query": echo,
        "total": results.total,
        "total_capped": results.total_capped,
        "limit": limit,
        "offset": offset,
        "hits": results.hits,
    }))
    .into_response())
}

async fn list_consumers(State(store): State<Store>) -> ApiResult<Response> {
    let consumers = store.read(events::list_consumers).await?;
    Ok(Json(consumers).into_response())
}

async fn register_consumer(
    State(store): State<Store>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<StartBody>,
) -> ApiResult<Response> {
    let start = match body.start {
        StartAt::Beginning => Start::Beginning,
        StartAt::Latest => Start::Latest,
    };
    idem::json_status(&store, move |tx| {
        let (cursor, is_new) = events::register_consumer(tx, now_millis(), &name, start)?;
        let status = if is_new {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((status, cursor))
    })
    .await
}

async fn delete_consumer(
    State(store): State<Store>,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    events::validate_name(&name)?;
    idem::empty(&store, move |tx| events::delete_consumer(tx, &name)).await
}

async fn poll_consumer(
    State(store): State<Store>,
    Path(name): Path<String>,
    ApiQuery(query): ApiQuery<PollQuery>,
) -> ApiResult<Response> {
    events::validate_name(&name)?;
    let filter = event_filter(query.project, query.entity_kind, query.kind_prefix);
    let (cursor, list) = store
        .read(move |c| {
            let cursor = events::get_cursor(c, &name)?;
            let list = events::read_events(c, cursor.last_event_id, query.limit, &filter)?;
            Ok((cursor, list))
        })
        .await?;
    if !list.is_empty() {
        aurora_store::failpoints::hit(aurora_store::failpoints::Point::BeforeEventDelivery);
    }
    Ok(Json(json!({ "consumer": cursor, "events": list })).into_response())
}

async fn ack_consumer(
    State(store): State<Store>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<AckBody>,
) -> ApiResult<Response> {
    events::validate_name(&name)?;
    idem::json(&store, StatusCode::OK, move |tx| {
        events::ack(tx, now_millis(), &name, body.up_to)
    })
    .await
}
