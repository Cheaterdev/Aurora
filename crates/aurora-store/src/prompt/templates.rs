//! Stored overrides, resolution and rendering.
//!
//! # Permissions
//!
//! Template text is as sensitive as an agent's instructions: only the owner
//! lists, saves and deletes templates (`Forbidden` otherwise). Rendering is a
//! plain read for code that already checked who may ask (the runner, the
//! preview endpoint).
//!
//! # Events
//!
//! Entity kind `agent`, the agent's id as entity id, no project, the acting
//! owner as actor, in the transaction of the change:
//!
//! * `prompt_template.saved` `{agent_id, project, issue_type, created,
//!   body: {changed: true, chars}}` (`project` is the key, `issue_type` the
//!   type's id or null). Saving the text a template already has writes nothing.
//! * `prompt_template.deleted` `{agent_id, project, issue_type}`
//!
//! Deleting an agent deletes its templates with it (the database cascades);
//! the replay drops them on `agent.deleted`.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde_json::json;

use super::builtin::DEFAULT_TEMPLATE;
use super::context::{PromptRequest, context_for};
use super::lang::{self, MAX_TEMPLATE_CHARS, Template};
use crate::StoreError;
use crate::auth::Principal;
use crate::projects::{Rejection, conflict, emit_as, invalid, not_found, refusal};

/// Where the text of a rendered message came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateSource {
    /// The built-in default.
    Builtin,
    /// The agent's own `instructions`.
    Agent,
    /// An override for the issue's project.
    Project,
    /// An override for the issue's project and issue type.
    ProjectType,
    /// Text given with the request: a message edited by hand, or an unsaved
    /// preview.
    Inline,
}

impl TemplateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            TemplateSource::Builtin => "builtin",
            TemplateSource::Agent => "agent",
            TemplateSource::Project => "project",
            TemplateSource::ProjectType => "project_type",
            TemplateSource::Inline => "inline",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedTemplate {
    pub body: String,
    pub source: TemplateSource,
    /// The `prompt_template` row for `Project` and `ProjectType`.
    pub template_id: Option<i64>,
}

/// An override as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptTemplate {
    pub id: i64,
    pub agent_id: i64,
    /// The project key.
    pub project: String,
    pub issue_type_id: Option<i64>,
    pub issue_type: Option<String>,
    pub body: String,
    pub created: i64,
    pub updated: i64,
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

/// Validates template text; the error names the line and column and starts
/// with `what` ("template", "instructions").
pub fn check_template(what: &str, text: &str) -> Result<(), StoreError> {
    lang::parse(text)
        .map(|_| ())
        .map_err(|err| invalid(format!("{what}: {err}")))
}

fn agent_instructions(conn: &Connection, agent_id: i64) -> Result<String, StoreError> {
    conn.query_row(
        "SELECT instructions FROM agent WHERE id = ?1",
        [agent_id],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| not_found("agent"))
}

/// What a run of this agent uses when no override applies: its own
/// instructions if they are not blank, else the built-in default.
pub fn agent_default_source(
    conn: &Connection,
    agent_id: i64,
) -> Result<TemplateSource, StoreError> {
    Ok(if agent_instructions(conn, agent_id)?.trim().is_empty() {
        TemplateSource::Builtin
    } else {
        TemplateSource::Agent
    })
}

/// The template that applies, most specific first: the agent's override for
/// (project, issue type), for the project, the agent's instructions, the
/// built-in default.
pub fn resolve_template(
    conn: &Connection,
    agent_id: i64,
    project_id: i64,
    issue_type_id: Option<i64>,
) -> Result<ResolvedTemplate, StoreError> {
    let instructions = agent_instructions(conn, agent_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, issue_type_id, body FROM prompt_template
         WHERE agent_id = ?1 AND project_id = ?2
           AND (issue_type_id IS NULL OR issue_type_id IS ?3)",
    )?;
    let rows = stmt
        .query_map(params![agent_id, project_id, issue_type_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let specific = rows.iter().find(|(_, ty, _)| ty.is_some());
    let general = rows.iter().find(|(_, ty, _)| ty.is_none());
    if let Some((id, _, body)) = specific {
        return Ok(ResolvedTemplate {
            body: body.clone(),
            source: TemplateSource::ProjectType,
            template_id: Some(*id),
        });
    }
    if let Some((id, _, body)) = general {
        return Ok(ResolvedTemplate {
            body: body.clone(),
            source: TemplateSource::Project,
            template_id: Some(*id),
        });
    }
    if instructions.trim().is_empty() {
        Ok(ResolvedTemplate {
            body: DEFAULT_TEMPLATE.to_string(),
            source: TemplateSource::Builtin,
            template_id: None,
        })
    } else {
        Ok(ResolvedTemplate {
            body: instructions,
            source: TemplateSource::Agent,
            template_id: None,
        })
    }
}

/// The exact first message of a run, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rendered {
    pub text: String,
    pub chars: usize,
    pub template_source: TemplateSource,
    pub template_id: Option<i64>,
    /// Something was shortened or left out to keep within the limits (see
    /// `lang` for the order).
    pub truncated: bool,
    /// The catalogue names the template referred to, sorted.
    pub variables_used: Vec<String>,
}

/// Renders the first message for `request`. This is the single function the
/// runner calls to start a run and the preview calls to show one; the text it
/// returns is exactly what is sent and what the run stores.
///
/// Errors: the agent, project or issue does not exist (not found); the trigger
/// comment is not on the issue, or the template text that applies does not
/// pass validation (invalid; the message names the template's source and the
/// line and column). `now` is a parameter so a test can fix the time.
pub fn render_prompt(
    conn: &Connection,
    now: i64,
    request: &PromptRequest<'_>,
) -> Result<Rendered, StoreError> {
    let context = context_for(conn, now, request)?;
    let (body, source, template_id) = match request.override_template {
        Some(text) => (text.to_string(), TemplateSource::Inline, None),
        None => {
            let resolved = resolve_template(
                conn,
                request.agent_id,
                context.project_id,
                Some(context.issue_type_id),
            )?;
            (resolved.body, resolved.source, resolved.template_id)
        }
    };
    let template = lang::parse(&body)
        .map_err(|err| invalid(format!("the {} template: {err}", source.as_str())))?;
    let output = lang::render(&template, &context.vars);
    Ok(Rendered {
        chars: output.text.chars().count(),
        text: output.text,
        template_source: source,
        template_id,
        truncated: context.truncated || output.truncated,
        variables_used: template.variables_used(),
    })
}

/// The parsed built-in default, for callers that want to inspect it.
pub fn builtin_template() -> Result<Template, StoreError> {
    lang::parse(DEFAULT_TEMPLATE).map_err(|err| invalid(format!("the built-in template: {err}")))
}

// ---- overrides ----

const TEMPLATE_SELECT: &str = "SELECT t.id, t.agent_id, p.key, t.issue_type_id, it.name, t.body,
        t.created, t.updated
     FROM prompt_template t
     JOIN project p ON p.id = t.project_id
     LEFT JOIN issue_type it ON it.id = t.issue_type_id";

fn template_row(row: &Row<'_>) -> rusqlite::Result<PromptTemplate> {
    Ok(PromptTemplate {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        project: row.get(2)?,
        issue_type_id: row.get(3)?,
        issue_type: row.get(4)?,
        body: row.get(5)?,
        created: row.get(6)?,
        updated: row.get(7)?,
    })
}

fn get_template(conn: &Connection, id: i64) -> Result<PromptTemplate, StoreError> {
    conn.query_row(
        &format!("{TEMPLATE_SELECT} WHERE t.id = ?1"),
        [id],
        template_row,
    )
    .optional()?
    .ok_or_else(|| not_found("template"))
}

/// The agent's overrides, by project key then issue type. Owner only.
pub fn list_templates(
    conn: &Connection,
    reader: &Principal,
    agent_id: i64,
) -> Result<Vec<PromptTemplate>, StoreError> {
    require_owner(reader)?;
    agent_instructions(conn, agent_id)?;
    let mut stmt = conn.prepare(&format!(
        "{TEMPLATE_SELECT} WHERE t.agent_id = ?1 ORDER BY p.key, t.issue_type_id IS NOT NULL, t.issue_type_id"
    ))?;
    let rows = stmt.query_map([agent_id], template_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// What [`save_template`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveOutcome {
    pub template: PromptTemplate,
    /// A new row was made.
    pub created: bool,
    /// Something was written (and an event). `false` when the text was
    /// already what is stored.
    pub changed: bool,
}

/// Creates or replaces the agent's override for `project` (and `issue_type_id`
/// when given). Owner only. `project` is required: an issue type belongs to a
/// project, and the agent-wide default is the agent's `instructions`.
pub fn save_template(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    agent_id: i64,
    project: Option<&str>,
    issue_type_id: Option<i64>,
    body: &str,
) -> Result<SaveOutcome, StoreError> {
    require_owner(actor)?;
    agent_instructions(tx, agent_id)?;
    let Some(project_key) = project else {
        return Err(invalid(
            "a template override needs a project; the agent's default template is its instructions",
        ));
    };
    let project = tx
        .query_row(
            "SELECT id FROM project WHERE key = ?1",
            [project_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| invalid(format!("unknown project {project_key}")))?;
    if let Some(type_id) = issue_type_id {
        let owner: Option<i64> = tx
            .query_row(
                "SELECT project_id FROM issue_type WHERE id = ?1",
                [type_id],
                |row| row.get(0),
            )
            .optional()?;
        match owner {
            None => return Err(invalid(format!("unknown issue type {type_id}"))),
            Some(owner) if owner != project => {
                return Err(invalid(format!(
                    "issue type {type_id} does not belong to project {project_key}"
                )));
            }
            Some(_) => {}
        }
    }
    if body.trim().is_empty() {
        return Err(invalid(
            "the template must not be empty; delete the override to go back to the default",
        ));
    }
    if body.chars().count() > MAX_TEMPLATE_CHARS || body.contains('\0') {
        return Err(invalid(format!(
            "the template must be at most {MAX_TEMPLATE_CHARS} characters without NUL"
        )));
    }
    check_template("template", body)?;

    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, body FROM prompt_template
             WHERE agent_id = ?1 AND project_id = ?2 AND COALESCE(issue_type_id, 0) = COALESCE(?3, 0)",
            params![agent_id, project, issue_type_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let chars = body.chars().count();
    let (id, created) = match existing {
        Some((id, old)) if old == body => {
            return Ok(SaveOutcome {
                template: get_template(tx, id)?,
                created: false,
                changed: false,
            });
        }
        Some((id, _)) => {
            tx.execute(
                "UPDATE prompt_template SET body = ?1, updated = ?2 WHERE id = ?3",
                params![body, now, id],
            )?;
            (id, false)
        }
        None => {
            tx.execute(
                "INSERT INTO prompt_template (agent_id, project_id, issue_type_id, body, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![agent_id, project, issue_type_id, body, now],
            )
            .map_err(|err| refusal(err, || "this override already exists".to_string()))?;
            (tx.last_insert_rowid(), true)
        }
    };
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "agent",
        agent_id,
        "prompt_template.saved",
        &json!({
            "agent_id": agent_id,
            "project": project_key,
            "issue_type": issue_type_id,
            "created": created,
            "body": { "changed": true, "chars": chars },
        }),
    )?;
    Ok(SaveOutcome {
        template: get_template(tx, id)?,
        created,
        changed: true,
    })
}

/// Deletes one of the agent's overrides. Owner only.
pub fn delete_template(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    agent_id: i64,
    template_id: i64,
) -> Result<(), StoreError> {
    require_owner(actor)?;
    agent_instructions(tx, agent_id)?;
    let template = get_template(tx, template_id)?;
    if template.agent_id != agent_id {
        return Err(not_found("template"));
    }
    tx.execute("DELETE FROM prompt_template WHERE id = ?1", [template_id])?;
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "agent",
        agent_id,
        "prompt_template.deleted",
        &json!({
            "agent_id": agent_id,
            "project": template.project,
            "issue_type": template.issue_type_id,
        }),
    )
}

/// Refuses to delete an issue type that overrides still point at.
pub(crate) fn ensure_type_unused(conn: &Connection, type_id: i64) -> Result<(), StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM prompt_template WHERE issue_type_id = ?1",
        [type_id],
        |row| row.get(0),
    )?;
    if count > 0 {
        return Err(conflict(format!(
            "the issue type is used by {count} prompt template override(s); delete them first"
        )));
    }
    Ok(())
}
