//! REST handlers for the agent registry. The rules live in
//! `aurora_store::agents`; this module only maps them to HTTP.

use aurora_store::agents::{self, Agent, AgentPatch, NewAgent};
use aurora_store::auth::{self, Principal, Scope};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Map, Value, json};

use crate::auth::{Access, no_store};
use crate::idem;
use crate::routes::{ApiError, ApiJson, ApiResult};
use crate::runner::Runner;

/// Name of the key `create_key: true` issues.
const AGENT_KEY_NAME: &str = "agent key";

/// A restricted agent may read the registry, but only sees its own kind of
/// scope: the project lists are cut down to the projects it may access.
fn scoped(mut agent: Agent, access: &Access) -> Agent {
    if let Some(allowed) = access.allowed() {
        agent.projects.retain(|key| allowed.contains(key));
    }
    agent
}

pub(crate) async fn list_agents(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<Access>,
) -> ApiResult<Response> {
    let list = store
        .read(move |c| agents::list_agents(c, &principal))
        .await?;
    let list: Vec<Agent> = list.into_iter().map(|a| scoped(a, &access)).collect();
    Ok(Json(list).into_response())
}

pub(crate) async fn get_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let agent = store
        .read(move |c| agents::get_agent(c, &principal, id))
        .await?;
    Ok(Json(scoped(agent, &access)).into_response())
}

pub(crate) async fn create_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    ApiJson(mut body): ApiJson<Map<String, Value>>,
) -> ApiResult<Response> {
    let with_key = match body.remove("create_key") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => flag,
        Some(_) => return Err(invalid("create_key must be true or false".into())),
    };
    let new: NewAgent = serde_json::from_value(Value::Object(body))
        .map_err(|err| invalid(format!("invalid agent: {err}")))?;
    if !with_key {
        return idem::json(&store, StatusCode::CREATED, move |tx| {
            agents::create_agent(tx, now_millis(), &principal, new)
        })
        .await;
    }
    // One transaction: a failure to issue the key must not leave a keyless agent.
    idem::secret(&store, StatusCode::CREATED, move |tx| {
        let now = now_millis();
        let agent = agents::create_agent(tx, now, &principal, new)?;
        let key = auth::create_key(
            tx,
            now,
            &principal,
            agent.user_id,
            AGENT_KEY_NAME,
            Scope::Write,
        )?;
        Ok(json!({
            "agent": agent,
            "key": key.key,
            "key_info": key.info,
        }))
    })
    .await
    .map(no_store)
}

pub(crate) async fn update_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(runner): Extension<Option<Runner>>,
    Path(id): Path<i64>,
    ApiJson(patch): ApiJson<AgentPatch>,
) -> ApiResult<Response> {
    let response = idem::json(&store, StatusCode::OK, move |tx| {
        agents::update_agent(tx, now_millis(), &principal, id, patch)
    })
    .await?;
    // A changed limit, switch or project list may free or end queued runs.
    wake(&runner);
    Ok(response)
}

fn wake(runner: &Option<Runner>) {
    if let Some(runner) = runner {
        runner.wake();
    }
}

pub(crate) async fn delete_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    idem::empty(&store, move |tx| {
        agents::delete_agent(tx, now_millis(), &principal, id)
    })
    .await
}

pub(crate) async fn enable_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(runner): Extension<Option<Runner>>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_enabled(store, principal, runner, id, true).await
}

pub(crate) async fn disable_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Extension(runner): Extension<Option<Runner>>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_enabled(store, principal, runner, id, false).await
}

async fn set_enabled(
    store: Store,
    principal: Principal,
    runner: Option<Runner>,
    id: i64,
    enabled: bool,
) -> ApiResult<Response> {
    let response = idem::json(&store, StatusCode::OK, move |tx| {
        agents::set_enabled(tx, now_millis(), &principal, id, enabled)
    })
    .await?;
    wake(&runner);
    Ok(response)
}

fn invalid(reason: String) -> ApiError {
    StoreError::from(Rejection::Invalid(reason)).into()
}
