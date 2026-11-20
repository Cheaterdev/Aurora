//! The REST API under `/api/v1`. Handlers only translate; the rules live in
//! `aurora_store::projects` and `aurora_store::issues`.
//!
//! There is no authentication yet (TRK-2). Every API route is built in
//! [`api_router`], so one auth layer added there covers all of them.

use aurora_store::issues::{self, IssueFilter, IssuePatch, MoveTarget, NewIssue, ParentFilter};
use aurora_store::projects::{self, NewColumn, StateType};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

const DEFAULT_LABEL_COLOR: &str = "#808080";

pub fn api_router() -> Router<Store> {
    Router::new()
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{key}", get(get_project).patch(rename_project))
        .route(
            "/projects/{key}/columns",
            get(list_columns).post(create_column),
        )
        .route("/projects/{key}/columns/order", put(reorder_columns))
        .route(
            "/projects/{key}/columns/{id}",
            patch(rename_column).delete(delete_column),
        )
        .route(
            "/projects/{key}/issue-types",
            get(list_issue_types).post(create_issue_type),
        )
        .route(
            "/projects/{key}/issue-types/{id}",
            patch(rename_issue_type).delete(delete_issue_type),
        )
        .route(
            "/projects/{key}/labels",
            get(list_labels).post(create_label),
        )
        .route(
            "/projects/{key}/labels/{id}",
            patch(update_label).delete(delete_label),
        )
        .route(
            "/projects/{key}/issues",
            get(list_issues).post(create_issue),
        )
        .route(
            "/projects/{key}/issues/{number}",
            get(get_issue).patch(update_issue),
        )
        .route("/projects/{key}/issues/{number}/move", post(move_issue))
        .route(
            "/projects/{key}/issues/{number}/archive",
            post(archive_issue),
        )
        .route(
            "/projects/{key}/issues/{number}/unarchive",
            post(unarchive_issue),
        )
}

pub struct ApiError {
    status: StatusCode,
    message: String,
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
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// `Json` whose rejections use the API's error body.
struct ApiJson<T>(T);

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
struct ApiQuery<T>(T);

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

fn created<T: serde::Serialize>(value: T) -> Response {
    (StatusCode::CREATED, Json(value)).into_response()
}

fn no_content() -> Response {
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

async fn list_projects(State(store): State<Store>) -> ApiResult<Response> {
    let projects = store.read(projects::list_projects).await?;
    Ok(Json(projects).into_response())
}

async fn create_project(
    State(store): State<Store>,
    ApiJson(body): ApiJson<NewProject>,
) -> ApiResult<Response> {
    let project = store
        .write(move |tx| {
            projects::create_project(
                tx,
                now_millis(),
                &body.key,
                &body.name,
                body.columns.as_deref(),
            )
        })
        .await?;
    Ok(created(project))
}

async fn get_project(State(store): State<Store>, Path(key): Path<String>) -> ApiResult<Response> {
    let project = store.read(move |c| projects::get_project(c, &key)).await?;
    Ok(Json(project).into_response())
}

async fn rename_project(
    State(store): State<Store>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    let project = store
        .write(move |tx| projects::rename_project(tx, now_millis(), &key, &body.name))
        .await?;
    Ok(Json(project).into_response())
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
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewColumnBody>,
) -> ApiResult<Response> {
    let column = store
        .write(move |tx| {
            projects::create_column(tx, now_millis(), &key, &body.name, body.state_type)
        })
        .await?;
    Ok(created(column))
}

async fn rename_column(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    let column = store
        .write(move |tx| projects::rename_column(tx, now_millis(), &key, id, &body.name))
        .await?;
    Ok(Json(column).into_response())
}

async fn reorder_columns(
    State(store): State<Store>,
    Path(key): Path<String>,
    ApiJson(body): ApiJson<OrderBody>,
) -> ApiResult<Response> {
    let columns = store
        .write(move |tx| projects::reorder_columns(tx, now_millis(), &key, &body.ids))
        .await?;
    Ok(Json(columns).into_response())
}

async fn delete_column(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    store
        .write(move |tx| projects::delete_column(tx, now_millis(), &key, id))
        .await?;
    Ok(no_content())
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
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    let issue_type = store
        .write(move |tx| projects::create_issue_type(tx, now_millis(), &key, &body.name))
        .await?;
    Ok(created(issue_type))
}

async fn rename_issue_type(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    let issue_type = store
        .write(move |tx| projects::rename_issue_type(tx, now_millis(), &key, id, &body.name))
        .await?;
    Ok(Json(issue_type).into_response())
}

async fn delete_issue_type(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    store
        .write(move |tx| projects::delete_issue_type(tx, now_millis(), &key, id))
        .await?;
    Ok(no_content())
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
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewLabelBody>,
) -> ApiResult<Response> {
    let label = store
        .write(move |tx| {
            let color = body.color.as_deref().unwrap_or(DEFAULT_LABEL_COLOR);
            projects::create_label(tx, now_millis(), &key, &body.name, color)
        })
        .await?;
    Ok(created(label))
}

async fn update_label(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
    ApiJson(body): ApiJson<LabelPatch>,
) -> ApiResult<Response> {
    if body.name.is_none() && body.color.is_none() {
        return Err(
            StoreError::from(Rejection::Invalid("give a name, a color or both".into())).into(),
        );
    }
    // One transaction, so a bad color does not leave the rename behind.
    let label = store
        .write(move |tx| {
            let now = now_millis();
            let mut label = None;
            if let Some(name) = &body.name {
                label = Some(projects::rename_label(tx, now, &key, id, name)?);
            }
            if let Some(color) = &body.color {
                label = Some(projects::recolor_label(tx, now, &key, id, color)?);
            }
            label.ok_or_else(|| StoreError::from(Rejection::NotFound("label".into())))
        })
        .await?;
    Ok(Json(label).into_response())
}

async fn delete_label(
    State(store): State<Store>,
    Path((key, id)): Path<(String, i64)>,
) -> ApiResult<Response> {
    store
        .write(move |tx| projects::delete_label(tx, now_millis(), &key, id))
        .await?;
    Ok(no_content())
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
    Path(key): Path<String>,
    ApiJson(body): ApiJson<NewIssue>,
) -> ApiResult<Response> {
    let issue = store
        .write(move |tx| issues::create_issue(tx, now_millis(), &key, body))
        .await?;
    Ok(created(issue))
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
    Path((key, number)): Path<(String, i64)>,
    ApiJson(body): ApiJson<IssuePatch>,
) -> ApiResult<Response> {
    let issue = store
        .write(move |tx| issues::update_issue(tx, now_millis(), &key, number, body))
        .await?;
    Ok(Json(issue).into_response())
}

async fn move_issue(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
    ApiJson(body): ApiJson<MoveTarget>,
) -> ApiResult<Response> {
    let issue = store
        .write(move |tx| issues::move_issue(tx, now_millis(), &key, number, body))
        .await?;
    Ok(Json(issue).into_response())
}

async fn archive_issue(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let issue = store
        .write(move |tx| issues::archive_issue(tx, now_millis(), &key, number))
        .await?;
    Ok(Json(issue).into_response())
}

async fn unarchive_issue(
    State(store): State<Store>,
    Path((key, number)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let issue = store
        .write(move |tx| issues::unarchive_issue(tx, now_millis(), &key, number))
        .await?;
    Ok(Json(issue).into_response())
}
