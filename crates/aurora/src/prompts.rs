//! REST handlers for prompt templates (the first message of a run). The rules
//! live in `aurora_store::prompt`; this module only maps them to HTTP.

use aurora_store::auth::{Principal, Role};
use aurora_store::prompt::{
    self, DEFAULT_TEMPLATE, MAX_COMMENT_CHARS, MAX_COMMENTS, MAX_DEPTH, MAX_OUTPUT_CHARS,
    MAX_TEMPLATE_CHARS, PromptRequest, TriggerCtx, TriggerKind,
};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use crate::idem;
use crate::routes::{ApiJson, ApiResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveBody {
    /// The project key.
    project: String,
    issue_type_id: Option<i64>,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewBody {
    /// The project key of the issue to preview with.
    project: String,
    number: i64,
    /// Defaults to `manual`.
    trigger: Option<TriggerKind>,
    /// For `mentioned`: the comment that mentions the agent.
    comment_id: Option<i64>,
    /// Render this text instead of the stored template (an unsaved edit).
    template: Option<String>,
}

/// The agent's overrides and what applies when none does. Owner only.
pub(crate) async fn list_templates(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let (templates, default_source) = store
        .read(move |c| {
            let templates = prompt::list_templates(c, &principal, id)?;
            let source = prompt::agent_default_source(c, id)?;
            Ok((templates, source))
        })
        .await?;
    Ok(Json(json!({
        "default_source": default_source,
        "templates": templates,
    }))
    .into_response())
}

/// Creates or replaces one override: `201` when it is new, `200` otherwise.
pub(crate) async fn save_template(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<SaveBody>,
) -> ApiResult<Response> {
    idem::json_status(&store, move |tx| {
        let saved = prompt::save_template(
            tx,
            now_millis(),
            &principal,
            id,
            Some(&body.project),
            body.issue_type_id,
            &body.body,
        )?;
        let status = if saved.created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((status, saved.template))
    })
    .await
}

pub(crate) async fn delete_template(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path((id, template_id)): Path<(i64, i64)>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        prompt::delete_template(tx, now_millis(), &principal, id, template_id)
    })
    .await
}

/// The exact first message a run of this agent would get for an issue. It
/// changes nothing, so it takes no request id. Owners and members may ask;
/// an agent's key may not.
pub(crate) async fn preview(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
    ApiJson(body): ApiJson<PreviewBody>,
) -> ApiResult<Response> {
    if principal.role == Role::Agent {
        return Err(StoreError::from(Rejection::Forbidden(
            "agents cannot preview prompts".to_string(),
        ))
        .into());
    }
    let public_url = store.public_url().map(str::to_string);
    let rendered = store
        .read(move |c| {
            let request = PromptRequest {
                agent_id: id,
                project: &body.project,
                number: body.number,
                trigger: TriggerCtx {
                    kind: body.trigger.unwrap_or(TriggerKind::Manual),
                    comment_id: body.comment_id,
                    actor_id: Some(principal.user_id),
                },
                workdir: None,
                branch: None,
                run_id: None,
                public_url: public_url.as_deref(),
                override_template: body.template.as_deref(),
            };
            prompt::render_prompt(c, now_millis(), &request)
        })
        .await?;
    Ok(Json(rendered).into_response())
}

/// The variables a template may use, the limits and the built-in default.
pub(crate) async fn variables() -> Json<serde_json::Value> {
    Json(json!({
        "variables": prompt::variable_catalogue(),
        "limits": {
            "max_template_chars": MAX_TEMPLATE_CHARS,
            "max_depth": MAX_DEPTH,
            "max_output_chars": MAX_OUTPUT_CHARS,
            "max_comments": MAX_COMMENTS,
            "max_comment_chars": MAX_COMMENT_CHARS,
        },
        "default_template": DEFAULT_TEMPLATE,
    }))
}
