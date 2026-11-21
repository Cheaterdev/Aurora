//! REST handlers for runs. The rules live in `aurora_store::runs`, the
//! process handling in [`crate::runner`]; this module only maps them to HTTP.
//!
//! The first message and the output of a run can hold secrets: only the owner
//! and members get them. A registered agent's key sees runs of its allowed
//! projects without either.

use std::sync::{Arc, Mutex};

use aurora_store::auth::{Principal, Role};
use aurora_store::runs::{self, Created, Outcome, RunFilter, RunReason, RunStatus};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use crate::auth::Access;
use crate::idem;
use crate::routes::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::runner::{Runner, StartRequest};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartBody {
    /// `KEY-n`.
    issue: String,
    /// The first message, replacing the rendered one; sent verbatim.
    message: Option<String>,
    /// A comment of the issue the template may refer to.
    trigger_comment_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    issue: Option<String>,
    agent: Option<i64>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

fn invalid(reason: impl Into<String>) -> ApiError {
    StoreError::from(Rejection::Invalid(reason.into())).into()
}

fn not_for_agents(principal: &Principal, what: &str) -> Result<(), ApiError> {
    if principal.role == Role::Agent {
        return Err(StoreError::from(Rejection::Forbidden(format!("agents cannot {what}"))).into());
    }
    Ok(())
}

fn parse_issue(text: &str) -> Result<(String, i64), ApiError> {
    text.rsplit_once('-')
        .and_then(|(key, number)| Some((key.to_string(), number.parse::<i64>().ok()?)))
        .filter(|(key, number)| !key.is_empty() && *number > 0)
        .ok_or_else(|| invalid("issue must look like KEY-12"))
}

fn runner_missing() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "the runner is not running on this server",
    )
}

/// Starts a run by hand. The row, its key and its message are written in the
/// transaction that also records the request id; the process starts after the
/// commit, and only when this request was the one that applied (a replay
/// finds nothing to start).
pub(crate) async fn start_run(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(runner): Extension<Option<Runner>>,
    Path(agent_id): Path<i64>,
    ApiJson(body): ApiJson<StartBody>,
) -> ApiResult<Response> {
    not_for_agents(&principal, "start runs")?;
    let runner = runner.ok_or_else(runner_missing)?;
    let (project, number) = parse_issue(&body.issue)?;
    let request = StartRequest {
        agent_id,
        project,
        number,
        reason: RunReason::Manual,
        trigger_event_id: None,
        comment_id: body.trigger_comment_id,
        actor: Some(principal.user_id),
        message: body.message,
    };
    let applied: Arc<Mutex<Option<Created>>> = Arc::new(Mutex::new(None));
    let slot = applied.clone();
    let creator = runner.clone();
    let response = idem::json(&store, StatusCode::CREATED, move |tx| {
        let created = creator.create_in_tx(tx, now_millis(), &request)?;
        let detail = runs::get_run(tx, created.run.id)?;
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(created);
        Ok(detail)
    })
    .await?;
    let created = applied
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(created) = created {
        runner.launch(created);
    }
    Ok(response)
}

pub(crate) async fn list_runs(
    State(store): State<Store>,
    Extension(access): Extension<Access>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    let issue = query.issue.as_deref().map(parse_issue).transpose()?;
    if let Some((key, _)) = &issue {
        access.check_project(key)?;
    }
    let status = match query.status.as_deref() {
        None | Some("") => None,
        Some(text) => Some(RunStatus::parse(text).ok_or_else(|| {
            invalid("status must be starting, running, succeeded, failed, interrupted or canceled")
        })?),
    };
    let filter = RunFilter {
        issue,
        agent_id: query.agent,
        status,
        allowed_projects: access.allowed().map(<[String]>::to_vec),
        limit: query.limit,
        offset: query.offset,
    };
    let list = store.read(move |c| runs::list_runs(c, &filter)).await?;
    Ok(Json(list).into_response())
}

pub(crate) async fn get_run(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let detail = store.read(move |c| runs::get_run(c, id)).await?;
    access.check_project(&detail.run.project)?;
    if principal.role == Role::Agent {
        return Ok(Json(detail.run).into_response());
    }
    Ok(Json(detail).into_response())
}

/// Cancels a run: it becomes `canceled` and its key is revoked in this
/// request's transaction; the process is stopped right after the commit.
pub(crate) async fn cancel_run(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(runner): Extension<Option<Runner>>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    not_for_agents(&principal, "cancel runs")?;
    let runner = runner.ok_or_else(runner_missing)?;
    let applied = Arc::new(Mutex::new(false));
    let flag = applied.clone();
    let user = principal.user_id;
    let response = idem::json(&store, StatusCode::OK, move |tx| {
        let changed = runs::finish_run(
            tx,
            now_millis(),
            Some(user),
            id,
            Outcome::new(RunStatus::Canceled),
        )?;
        if !changed {
            return Err(StoreError::from(Rejection::Conflict(
                "the run already finished".into(),
            )));
        }
        *flag.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        runs::get_run(tx, id)
    })
    .await?;
    let ran = *applied
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if ran {
        runner.stop_process(id, Some(user)).await;
    }
    Ok(response)
}
