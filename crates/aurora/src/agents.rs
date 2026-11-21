//! REST handlers for the agent registry. The rules live in
//! `aurora_store::agents`; this module only maps them to HTTP.

use aurora_store::agents::{self, Agent, AgentPatch, NewAgent};
use aurora_store::auth::{self, Principal, Scope};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Map, Value, json};

use crate::auth::{Access, no_store};
use crate::routes::{ApiError, ApiJson, ApiResult, created, no_content};

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
        let agent = store
            .write(move |tx| agents::create_agent(tx, now_millis(), &principal, new))
            .await?;
        return Ok(created(agent));
    }
    // One transaction: a failure to issue the key must not leave a keyless agent.
    let (agent, key) = store
        .write(move |tx| {
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
            Ok((agent, key))
        })
        .await?;
    let info = key.info.clone();
    Ok(no_store(created(json!({
        "agent": agent,
        "key": key.key,
        "key_info": info,
    }))))
}

pub(crate) async fn update_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
    ApiJson(patch): ApiJson<AgentPatch>,
) -> ApiResult<Response> {
    let agent = store
        .write(move |tx| agents::update_agent(tx, now_millis(), &principal, id, patch))
        .await?;
    Ok(Json(agent).into_response())
}

pub(crate) async fn delete_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    store
        .write(move |tx| agents::delete_agent(tx, now_millis(), &principal, id))
        .await?;
    Ok(no_content())
}

pub(crate) async fn enable_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_enabled(store, principal, id, true).await
}

pub(crate) async fn disable_agent(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_enabled(store, principal, id, false).await
}

async fn set_enabled(
    store: Store,
    principal: Principal,
    id: i64,
    enabled: bool,
) -> ApiResult<Response> {
    let agent = store
        .write(move |tx| agents::set_enabled(tx, now_millis(), &principal, id, enabled))
        .await?;
    Ok(Json(agent).into_response())
}

fn invalid(reason: String) -> ApiError {
    StoreError::from(Rejection::Invalid(reason)).into()
}
