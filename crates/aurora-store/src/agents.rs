//! The agent registry: which agent users the runner may start, with what
//! command, on which triggers and in which projects.
//!
//! An agent is an `agent` row attached to a user with role `agent`. Users with
//! that role but no row stay valid users (they can still act through their
//! keys); they are only "not registered": the runner ignores them and the
//! project restriction below does not apply to them.
//!
//! # Allowed projects: an explicit allow-list, default deny
//!
//! `agent_project` lists the projects a registered agent may access. **An
//! empty list means the agent may access no project at all.** There is no
//! "all projects" value: to allow everything, list everything. The HTTP layer
//! enforces the list on the agent's keys (see `docs/api.md`), the dispatch
//! helper [`triggers_for_event`] only wakes an agent for events of a listed
//! project. Humans (owner, member) and unregistered agent users are not
//! restricted by it: version 1 has no per-project permissions for them.
//!
//! # Secrets
//!
//! **Nothing secret belongs in an agent row.** `command`, `args`,
//! `instructions` and `work_dir` are stored in clear text and shown to the
//! owner. Environment variables carrying secrets are out of scope for the
//! registry; the agent's API keys are separate rows in `api_key` (hashed).
//! Because a person may paste a secret into these fields anyway, their
//! contents are never copied into events: a change to them is logged as
//! `{"changed": true}` only.
//!
//! # Permissions
//!
//! Only the owner creates, edits, enables, disables and deletes agents
//! (`Forbidden` otherwise). Any authenticated principal may read, but only the
//! owner sees `command`, `args`, `model`, `instructions` and `work_dir`
//! ([`AgentPrivate`]); everybody else gets name, description, triggers,
//! limits, enabled and projects, so one agent cannot read another's command
//! line.
//!
//! # Events
//!
//! Entity kind `agent`, the agent's id as entity id, no project, the acting
//! owner as actor, written in the transaction of the change:
//!
//! * `agent.created` `{name, user_id, on_assign, on_mention, enabled,
//!   max_concurrent, timeout_seconds, projects: [key]}`
//! * `agent.updated` `{changes: {field: {from, to}}}` for `on_assign`,
//!   `on_mention`, `enabled`, `max_concurrent`, `timeout_seconds` and
//!   `projects`; `{"changed": true}` for `description`, `command`, `args`,
//!   `model`, `instructions` and `work_dir`
//! * `agent.deleted` `{name, user_id}`; the user and its keys stay, the agent's
//!   prompt template overrides go with it
//!
//! The `instructions` are the agent's default prompt template (see
//! [`crate::prompt`]) and must pass its validation: plain text is always valid,
//! unknown `{{variables}}` and unbalanced blocks are refused.

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::StoreError;
use crate::auth::{self, Principal, Role};
use crate::events::Event;
use crate::issues::present;
use crate::projects::{Rejection, conflict, emit_as, invalid, not_found, refusal};

pub const MAX_COMMAND_CHARS: usize = 500;
pub const MAX_ARGS: usize = 50;
pub const MAX_ARG_CHARS: usize = 2000;
pub const MAX_INSTRUCTIONS_CHARS: usize = 20_000;
pub const MAX_WORK_DIR_CHARS: usize = 1000;
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
pub const MAX_MODEL_CHARS: usize = 200;
pub const DEFAULT_MAX_CONCURRENT: i64 = 1;
pub const DEFAULT_TIMEOUT_SECONDS: i64 = 3600;
const CONCURRENT_RANGE: (i64, i64) = (1, 16);
const TIMEOUT_RANGE: (i64, i64) = (60, 86_400);

/// What only the owner sees of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentPrivate {
    pub command: String,
    pub args: Vec<String>,
    pub model: Option<String>,
    pub instructions: String,
    /// Absolute path; `None` means the runner's default.
    pub work_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Agent {
    pub id: i64,
    pub user_id: i64,
    /// The agent user's name.
    pub name: String,
    pub description: String,
    pub on_assign: bool,
    pub on_mention: bool,
    pub enabled: bool,
    pub max_concurrent: i64,
    pub timeout_seconds: i64,
    /// Keys of the allowed projects, sorted. Empty means none.
    pub projects: Vec<String>,
    pub created: i64,
    pub updated: i64,
    /// `None` unless the reader is the owner; then flattened into the object.
    #[serde(flatten)]
    pub private: Option<AgentPrivate>,
}

/// An agent to register. Give exactly one of `user_id` (an existing agent
/// user) and `name` (creates the agent user in the same transaction).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAgent {
    pub user_id: Option<i64>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub model: Option<String>,
    pub instructions: Option<String>,
    pub work_dir: Option<String>,
    pub max_concurrent: Option<i64>,
    pub timeout_seconds: Option<i64>,
    pub on_assign: Option<bool>,
    pub on_mention: Option<bool>,
    pub enabled: Option<bool>,
    /// Project keys. Empty or absent: no project is allowed.
    #[serde(default)]
    pub projects: Vec<String>,
}

/// Only the fields that are present change; `null` clears `model` and
/// `work_dir`. `projects` replaces the whole allow-list.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPatch {
    pub description: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    pub model: Option<Option<String>>,
    pub instructions: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub work_dir: Option<Option<String>>,
    pub max_concurrent: Option<i64>,
    pub timeout_seconds: Option<i64>,
    pub on_assign: Option<bool>,
    pub on_mention: Option<bool>,
    pub enabled: Option<bool>,
    pub projects: Option<Vec<String>>,
}

fn forbidden() -> StoreError {
    Rejection::Forbidden("only the owner can manage agents".to_string()).into()
}

fn require_owner(actor: &Principal) -> Result<(), StoreError> {
    if actor.is_owner() {
        Ok(())
    } else {
        Err(forbidden())
    }
}

// ---- validation ----

fn chars(text: &str) -> usize {
    text.chars().count()
}

fn clean_command(text: &str) -> Result<String, StoreError> {
    let text = text.trim();
    let bad = text.chars().any(|c| matches!(c, '\0' | '\n' | '\r'));
    if (1..=MAX_COMMAND_CHARS).contains(&chars(text)) && !bad {
        Ok(text.to_string())
    } else {
        Err(invalid(format!(
            "command must be 1-{MAX_COMMAND_CHARS} characters without NUL or line breaks"
        )))
    }
}

fn clean_args(args: &[String]) -> Result<Vec<String>, StoreError> {
    if args.len() > MAX_ARGS {
        return Err(invalid(format!("at most {MAX_ARGS} arguments")));
    }
    for arg in args {
        if chars(arg) > MAX_ARG_CHARS || arg.contains('\0') {
            return Err(invalid(format!(
                "each argument must be at most {MAX_ARG_CHARS} characters without NUL"
            )));
        }
    }
    Ok(args.to_vec())
}

fn clean_model(model: Option<&str>) -> Result<Option<String>, StoreError> {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return Ok(None);
    };
    if chars(model) > MAX_MODEL_CHARS || model.chars().any(char::is_control) {
        return Err(invalid(format!(
            "model must be at most {MAX_MODEL_CHARS} characters without control characters"
        )));
    }
    Ok(Some(model.to_string()))
}

fn clean_instructions(text: &str) -> Result<String, StoreError> {
    if chars(text) > MAX_INSTRUCTIONS_CHARS || text.contains('\0') {
        return Err(invalid(format!(
            "instructions must be at most {MAX_INSTRUCTIONS_CHARS} characters without NUL"
        )));
    }
    // The instructions are the agent's default prompt template.
    crate::prompt::check_template("instructions", text)?;
    Ok(text.to_string())
}

fn clean_description(text: &str) -> Result<String, StoreError> {
    let text = text.trim();
    if chars(text) > MAX_DESCRIPTION_CHARS || text.contains('\0') {
        return Err(invalid(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    Ok(text.to_string())
}

/// Absolute in either style (`/x`, `\x`, `C:\x`, `C:/x`) so the check does not
/// depend on the platform the registry was edited on.
fn looks_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn clean_work_dir(dir: Option<&str>) -> Result<Option<String>, StoreError> {
    let Some(dir) = dir.map(str::trim).filter(|d| !d.is_empty()) else {
        return Ok(None);
    };
    let bad = dir.chars().any(|c| matches!(c, '\0' | '\n' | '\r'));
    if chars(dir) > MAX_WORK_DIR_CHARS || bad || !looks_absolute(dir) {
        return Err(invalid(format!(
            "work_dir must be an absolute path of at most {MAX_WORK_DIR_CHARS} characters, or empty for the runner's default"
        )));
    }
    Ok(Some(dir.to_string()))
}

fn in_range(what: &str, value: i64, (min, max): (i64, i64)) -> Result<i64, StoreError> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(invalid(format!("{what} must be {min}-{max}")))
    }
}

/// The ids of the named projects, sorted by key and without duplicates.
fn resolve_projects(conn: &Connection, keys: &[String]) -> Result<Vec<(i64, String)>, StoreError> {
    let mut found: Vec<(i64, String)> = Vec::new();
    for key in keys {
        let id: Option<i64> = conn
            .query_row("SELECT id FROM project WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        let id = id.ok_or_else(|| invalid(format!("unknown project {key}")))?;
        if !found.iter().any(|(existing, _)| *existing == id) {
            found.push((id, key.clone()));
        }
    }
    found.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(found)
}

// ---- reading ----

const AGENT_SELECT: &str = "SELECT a.id, a.user_id, u.name, a.description, a.command, a.args,
        a.model, a.instructions, a.work_dir, a.max_concurrent, a.timeout_seconds,
        a.on_assign, a.on_mention, a.enabled, a.created, a.updated
     FROM agent a JOIN user u ON u.id = a.user_id";

fn agent_row(row: &Row<'_>) -> rusqlite::Result<Agent> {
    let args: String = row.get(5)?;
    let args: Vec<String> = serde_json::from_str(&args)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(5, Type::Text, Box::new(err)))?;
    Ok(Agent {
        id: row.get(0)?,
        user_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        on_assign: row.get(11)?,
        on_mention: row.get(12)?,
        enabled: row.get(13)?,
        max_concurrent: row.get(9)?,
        timeout_seconds: row.get(10)?,
        projects: Vec::new(),
        created: row.get(14)?,
        updated: row.get(15)?,
        private: Some(AgentPrivate {
            command: row.get(4)?,
            args,
            model: row.get(6)?,
            instructions: row.get(7)?,
            work_dir: row.get(8)?,
        }),
    })
}

fn project_keys(conn: &Connection, agent_id: i64) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT p.key FROM agent_project ap JOIN project p ON p.id = ap.project_id
         WHERE ap.agent_id = ?1 ORDER BY p.key",
    )?;
    let rows = stmt.query_map([agent_id], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn with_projects(conn: &Connection, mut agent: Agent) -> Result<Agent, StoreError> {
    agent.projects = project_keys(conn, agent.id)?;
    Ok(agent)
}

/// The complete configuration, whoever asks. For the runner and for code that
/// already checked permissions; the permission-checked readers are
/// [`get_agent`] and [`list_agents`].
pub fn agent_config(conn: &Connection, id: i64) -> Result<Agent, StoreError> {
    let agent = conn
        .query_row(&format!("{AGENT_SELECT} WHERE a.id = ?1"), [id], agent_row)
        .optional()?
        .ok_or_else(|| not_found("agent"))?;
    with_projects(conn, agent)
}

/// The registry entry of an agent user, if it has one. The complete
/// configuration, like [`agent_config`].
pub fn agent_for_user(conn: &Connection, user_id: i64) -> Result<Option<Agent>, StoreError> {
    let agent = conn
        .query_row(
            &format!("{AGENT_SELECT} WHERE a.user_id = ?1"),
            [user_id],
            agent_row,
        )
        .optional()?;
    agent.map(|agent| with_projects(conn, agent)).transpose()
}

fn redact(mut agent: Agent, reader: &Principal) -> Agent {
    if !reader.is_owner() {
        agent.private = None;
    }
    agent
}

pub fn get_agent(conn: &Connection, reader: &Principal, id: i64) -> Result<Agent, StoreError> {
    agent_config(conn, id).map(|agent| redact(agent, reader))
}

/// All agents by id. Everything but the owner gets them without
/// [`AgentPrivate`].
pub fn list_agents(conn: &Connection, reader: &Principal) -> Result<Vec<Agent>, StoreError> {
    let mut stmt = conn.prepare(&format!("{AGENT_SELECT} ORDER BY a.id"))?;
    let rows = stmt.query_map([], agent_row)?;
    let mut out = Vec::new();
    for agent in rows {
        out.push(redact(with_projects(conn, agent?)?, reader));
    }
    Ok(out)
}

/// What the HTTP layer needs to restrict an agent's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAccess {
    pub agent_id: i64,
    pub enabled: bool,
    /// The allowed project keys. Empty: none.
    pub projects: Vec<String>,
}

/// `None` for a user that is not a registered agent (not restricted).
pub fn agent_access(conn: &Connection, user_id: i64) -> Result<Option<AgentAccess>, StoreError> {
    let found: Option<(i64, bool)> = conn
        .query_row(
            "SELECT id, enabled FROM agent WHERE user_id = ?1",
            [user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    found
        .map(|(agent_id, enabled)| {
            Ok(AgentAccess {
                agent_id,
                enabled,
                projects: project_keys(conn, agent_id)?,
            })
        })
        .transpose()
}

// ---- writing ----

pub fn create_agent(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    new: NewAgent,
) -> Result<Agent, StoreError> {
    require_owner(actor)?;
    let command = clean_command(&new.command)?;
    let args = clean_args(&new.args)?;
    let model = clean_model(new.model.as_deref())?;
    let instructions = clean_instructions(new.instructions.as_deref().unwrap_or(""))?;
    let work_dir = clean_work_dir(new.work_dir.as_deref())?;
    let description = clean_description(new.description.as_deref().unwrap_or(""))?;
    let max_concurrent = in_range(
        "max_concurrent",
        new.max_concurrent.unwrap_or(DEFAULT_MAX_CONCURRENT),
        CONCURRENT_RANGE,
    )?;
    let timeout_seconds = in_range(
        "timeout_seconds",
        new.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        TIMEOUT_RANGE,
    )?;
    let on_assign = new.on_assign.unwrap_or(true);
    let on_mention = new.on_mention.unwrap_or(true);
    let enabled = new.enabled.unwrap_or(true);
    let projects = resolve_projects(tx, &new.projects)?;

    let user = match (new.user_id, new.name.as_deref()) {
        (Some(id), None) => {
            let user = auth::get_user(tx, id)?;
            if user.role != Role::Agent {
                return Err(invalid(format!("user {} is not an agent user", user.name)));
            }
            if user.disabled {
                return Err(conflict("the user is disabled"));
            }
            user
        }
        (None, Some(name)) => auth::create_agent_user(tx, now, actor, name)?,
        _ => return Err(invalid("give exactly one of user_id and name")),
    };
    let args_json = serde_json::to_string(&args).map_err(|_| StoreError::Panicked)?;
    tx.execute(
        "INSERT INTO agent (user_id, description, command, args, model, instructions, work_dir,
                            max_concurrent, timeout_seconds, on_assign, on_mention, enabled,
                            created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            user.id,
            description,
            command,
            args_json,
            model,
            instructions,
            work_dir,
            max_concurrent,
            timeout_seconds,
            on_assign,
            on_mention,
            enabled,
            now
        ],
    )
    .map_err(|err| {
        refusal(err, || {
            format!("{} is already registered as an agent", user.name)
        })
    })?;
    let id = tx.last_insert_rowid();
    for (project_id, _) in &projects {
        tx.execute(
            "INSERT INTO agent_project (agent_id, project_id) VALUES (?1, ?2)",
            params![id, project_id],
        )?;
    }
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "agent",
        id,
        "agent.created",
        &json!({
            "name": user.name,
            "user_id": user.id,
            "on_assign": on_assign,
            "on_mention": on_mention,
            "enabled": enabled,
            "max_concurrent": max_concurrent,
            "timeout_seconds": timeout_seconds,
            "projects": projects.iter().map(|(_, key)| key.as_str()).collect::<Vec<_>>(),
        }),
    )?;
    agent_config(tx, id)
}

fn from_to(changes: &mut Map<String, Value>, field: &str, from: Value, to: Value) {
    changes.insert(field.to_string(), json!({ "from": from, "to": to }));
}

/// Free text that may hold a pasted secret: the event only says it changed.
fn text_changed(changes: &mut Map<String, Value>, field: &str) {
    changes.insert(field.to_string(), json!({ "changed": true }));
}

pub fn update_agent(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    id: i64,
    patch: AgentPatch,
) -> Result<Agent, StoreError> {
    require_owner(actor)?;
    let current = agent_config(tx, id)?;
    let Some(old) = current.private.clone() else {
        return Err(StoreError::Panicked);
    };
    let mut next = current.clone();
    let mut next_private = old.clone();
    let mut changes = Map::new();

    if let Some(description) = &patch.description {
        next.description = clean_description(description)?;
        if next.description != current.description {
            text_changed(&mut changes, "description");
        }
    }
    if let Some(command) = &patch.command {
        next_private.command = clean_command(command)?;
        if next_private.command != old.command {
            text_changed(&mut changes, "command");
        }
    }
    if let Some(args) = &patch.args {
        next_private.args = clean_args(args)?;
        if next_private.args != old.args {
            text_changed(&mut changes, "args");
        }
    }
    if let Some(model) = &patch.model {
        next_private.model = clean_model(model.as_deref())?;
        if next_private.model != old.model {
            text_changed(&mut changes, "model");
        }
    }
    if let Some(instructions) = &patch.instructions {
        next_private.instructions = clean_instructions(instructions)?;
        if next_private.instructions != old.instructions {
            text_changed(&mut changes, "instructions");
        }
    }
    if let Some(dir) = &patch.work_dir {
        next_private.work_dir = clean_work_dir(dir.as_deref())?;
        if next_private.work_dir != old.work_dir {
            text_changed(&mut changes, "work_dir");
        }
    }
    if let Some(value) = patch.max_concurrent {
        next.max_concurrent = in_range("max_concurrent", value, CONCURRENT_RANGE)?;
        if next.max_concurrent != current.max_concurrent {
            from_to(
                &mut changes,
                "max_concurrent",
                json!(current.max_concurrent),
                json!(next.max_concurrent),
            );
        }
    }
    if let Some(value) = patch.timeout_seconds {
        next.timeout_seconds = in_range("timeout_seconds", value, TIMEOUT_RANGE)?;
        if next.timeout_seconds != current.timeout_seconds {
            from_to(
                &mut changes,
                "timeout_seconds",
                json!(current.timeout_seconds),
                json!(next.timeout_seconds),
            );
        }
    }
    for (field, wanted, was, target) in [
        (
            "on_assign",
            patch.on_assign,
            current.on_assign,
            &mut next.on_assign,
        ),
        (
            "on_mention",
            patch.on_mention,
            current.on_mention,
            &mut next.on_mention,
        ),
        ("enabled", patch.enabled, current.enabled, &mut next.enabled),
    ] {
        if let Some(value) = wanted {
            *target = value;
            if value != was {
                from_to(&mut changes, field, json!(was), json!(value));
            }
        }
    }
    let new_projects = match &patch.projects {
        Some(keys) => Some(resolve_projects(tx, keys)?),
        None => None,
    };
    if let Some(list) = &new_projects {
        next.projects = list.iter().map(|(_, key)| key.clone()).collect();
        if next.projects != current.projects {
            from_to(
                &mut changes,
                "projects",
                json!(current.projects),
                json!(next.projects),
            );
        }
    }

    if changes.is_empty() {
        return Ok(current);
    }
    let args_json = serde_json::to_string(&next_private.args).map_err(|_| StoreError::Panicked)?;
    tx.execute(
        "UPDATE agent SET description = ?1, command = ?2, args = ?3, model = ?4,
                          instructions = ?5, work_dir = ?6, max_concurrent = ?7,
                          timeout_seconds = ?8, on_assign = ?9, on_mention = ?10,
                          enabled = ?11, updated = ?12
         WHERE id = ?13",
        params![
            next.description,
            next_private.command,
            args_json,
            next_private.model,
            next_private.instructions,
            next_private.work_dir,
            next.max_concurrent,
            next.timeout_seconds,
            next.on_assign,
            next.on_mention,
            next.enabled,
            now,
            id
        ],
    )?;
    if let Some(list) = &new_projects {
        tx.execute("DELETE FROM agent_project WHERE agent_id = ?1", [id])?;
        for (project_id, _) in list {
            tx.execute(
                "INSERT INTO agent_project (agent_id, project_id) VALUES (?1, ?2)",
                params![id, project_id],
            )?;
        }
    }
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "agent",
        id,
        "agent.updated",
        &json!({ "changes": changes }),
    )?;
    agent_config(tx, id)
}

/// Enable or disable. Does nothing, and logs nothing, when it already is so.
pub fn set_enabled(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    id: i64,
    enabled: bool,
) -> Result<Agent, StoreError> {
    let patch = AgentPatch {
        enabled: Some(enabled),
        ..AgentPatch::default()
    };
    update_agent(tx, now, actor, id, patch)
}

/// Removes the registry entry only: the user and its keys remain, so the user
/// becomes an unregistered agent user (and is no longer restricted). Disable
/// the agent instead to cut its keys off.
pub fn delete_agent(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    id: i64,
) -> Result<(), StoreError> {
    require_owner(actor)?;
    let agent = agent_config(tx, id)?;
    tx.execute("DELETE FROM agent WHERE id = ?1", [id])?;
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "agent",
        id,
        "agent.deleted",
        &json!({ "name": agent.name, "user_id": agent.user_id }),
    )
}

// ---- dispatch ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerReason {
    /// The issue was assigned to the agent's user.
    Assigned,
    /// A comment mentioned the agent's user.
    Mentioned,
}

/// A reason to start a run, found by [`triggers_for_event`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trigger {
    pub event_id: i64,
    pub agent_id: i64,
    pub user_id: i64,
    pub reason: TriggerReason,
    pub project: String,
    /// `KEY-n`.
    pub issue: String,
    /// The comment that mentioned the agent.
    pub comment_id: Option<i64>,
    /// Who caused the event.
    pub actor: Option<i64>,
}

/// The runs one event asks for, judged by the registry as it is when called:
///
/// * `issue.created` with an `assignee_id`, and `issue.updated` whose
///   `assignee_id` change has a new assignee, are `Assigned` for that
///   assignee only (the previous assignee and a cleared assignee wake nobody);
/// * `mention.created` with `user_kind` `agent` is `Mentioned`, unless the
///   mentioned user wrote the comment.
///
/// The user must be a registered, enabled agent whose matching trigger is on
/// and whose allowed projects include the event's project (an empty list
/// allows none). Every other event kind gives nothing. Pure: it reads, never
/// writes, and the same event gives the same answer, so a runner that sees an
/// event twice (delivery is at least once) must deduplicate on `event_id`.
/// An agent that assigns work to itself is not filtered out; `actor` is there
/// for a runner that wants to be careful about loops.
pub fn triggers_for_event(conn: &Connection, event: &Event) -> Result<Vec<Trigger>, StoreError> {
    let (Some(project_id), Some(project)) = (event.project_id, event.project.as_deref()) else {
        return Ok(Vec::new());
    };
    let payload = &event.payload;
    let (user_id, reason, comment_id) = match event.kind.as_str() {
        "issue.created" => (
            payload["assignee_id"].as_i64(),
            TriggerReason::Assigned,
            None,
        ),
        "issue.updated" => (
            payload["changes"]["assignee_id"]["to"].as_i64(),
            TriggerReason::Assigned,
            None,
        ),
        "mention.created" => {
            let mentioned = payload["user_id"].as_i64();
            let wanted = payload["user_kind"].as_str() == Some("agent")
                && mentioned != payload["author_id"].as_i64();
            (
                mentioned.filter(|_| wanted),
                TriggerReason::Mentioned,
                payload["comment_id"].as_i64(),
            )
        }
        _ => return Ok(Vec::new()),
    };
    let Some(user_id) = user_id else {
        return Ok(Vec::new());
    };
    let flag = match reason {
        TriggerReason::Assigned => "a.on_assign",
        TriggerReason::Mentioned => "a.on_mention",
    };
    let agent_id: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT a.id FROM agent a
                 JOIN user u ON u.id = a.user_id
                 JOIN agent_project ap ON ap.agent_id = a.id AND ap.project_id = ?2
                 WHERE a.user_id = ?1 AND a.enabled = 1 AND {flag} = 1
                   AND u.role = 'agent' AND u.disabled = 0"
            ),
            params![user_id, project_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(agent_id) = agent_id else {
        return Ok(Vec::new());
    };
    let issue = match event.kind.as_str() {
        "issue.created" => payload["identifier"].as_str().map(str::to_string),
        "mention.created" => payload["issue"]["identifier"].as_str().map(str::to_string),
        _ => conn
            .query_row(
                "SELECT number FROM issue WHERE id = ?1",
                [event.entity_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|number| format!("{project}-{number}")),
    };
    let Some(issue) = issue else {
        return Ok(Vec::new());
    };
    Ok(vec![Trigger {
        event_id: event.id,
        agent_id,
        user_id,
        reason,
        project: project.to_string(),
        issue,
        comment_id,
        actor: event.actor,
    }])
}
