//! Gathering what a run's first message may refer to, read-only and bounded:
//! a fixed number of queries however many comments, links or sub-issues the
//! issue has, and every list capped.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::lang::{Vars, cut_text};
use crate::StoreError;
use crate::documents::documents_of_issue;
use crate::links::links_of;
use crate::projects::{StateType, get_project, invalid, not_found};

/// The last this many comments (not deleted) are in the context.
pub const MAX_COMMENTS: i64 = 30;
/// A comment body in the context is cut to this many characters.
pub const MAX_COMMENT_CHARS: usize = 4_000;
/// Any other list (labels, sub-issues, links, documents) is cut to this many items.
pub const MAX_LIST_ITEMS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerKind {
    Assigned,
    Mentioned,
    Schedule,
    Manual,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerKind::Assigned => "assigned",
            TriggerKind::Mentioned => "mentioned",
            TriggerKind::Schedule => "schedule",
            TriggerKind::Manual => "manual",
        }
    }

    pub fn parse(text: &str) -> Option<TriggerKind> {
        Some(match text {
            "assigned" => TriggerKind::Assigned,
            "mentioned" => TriggerKind::Mentioned,
            "schedule" => TriggerKind::Schedule,
            "manual" => TriggerKind::Manual,
            _ => return None,
        })
    }
}

/// Why and by whom a run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerCtx {
    pub kind: TriggerKind,
    /// The comment that mentioned the agent; it must be a comment of the issue.
    pub comment_id: Option<i64>,
    /// Who caused the run. When absent for a mention, the comment's author.
    pub actor_id: Option<i64>,
}

impl TriggerCtx {
    pub fn new(kind: TriggerKind) -> TriggerCtx {
        TriggerCtx {
            kind,
            comment_id: None,
            actor_id: None,
        }
    }
}

/// Everything [`context_for`] and [`super::render_prompt`] need to know.
#[derive(Debug, Clone, Copy)]
pub struct PromptRequest<'a> {
    pub agent_id: i64,
    /// The issue is `project` + `number`.
    pub project: &'a str,
    pub number: i64,
    pub trigger: TriggerCtx,
    /// The working copy of the run; `None` until one exists.
    pub workdir: Option<&'a str>,
    /// The git branch of the run's working copy; `None` when there is none.
    pub branch: Option<&'a str>,
    /// The run's id; `None` outside a run (a preview).
    pub run_id: Option<i64>,
    /// The server's public base URL, if configured (no trailing slash).
    pub public_url: Option<&'a str>,
    /// Use this text instead of the stored templates (a person edited the
    /// message by hand, or a preview of unsaved text).
    pub override_template: Option<&'a str>,
}

/// The values for one render, and what the template resolution needs to know.
#[derive(Debug, Clone)]
pub struct Context {
    pub vars: Vars,
    pub project_id: i64,
    pub issue_type_id: i64,
    /// Something was cut or left out to keep the context within its limits.
    pub truncated: bool,
}

/// `2026-03-01T09:30:00Z` for Unix milliseconds.
pub fn iso8601(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn text(value: impl Into<String>) -> Value {
    Value::String(value.into())
}

struct Capped<T> {
    items: Vec<T>,
    cut: bool,
}

/// Fetched with one more row than the cap, to know whether it was cut.
fn cap<T>(mut items: Vec<T>, max: usize) -> Capped<T> {
    let cut = items.len() > max;
    items.truncate(max);
    Capped { items, cut }
}

fn issue_ref(key: &str, number: i64, title: &str, state: StateType) -> Value {
    json!({
        "key": format!("{key}-{number}"),
        "title": title,
        "state_type": state.as_str(),
    })
}

fn comment_body(body: &str, truncated: &mut bool) -> String {
    match cut_text(body, MAX_COMMENT_CHARS) {
        Some(cut) => {
            *truncated = true;
            cut
        }
        None => body.to_string(),
    }
}

/// Builds the context of one run (or preview), reading with a fixed number of
/// queries. Errors: the agent, project or issue does not exist (not found),
/// the trigger comment is not a comment of the issue (invalid).
pub fn context_for(
    conn: &Connection,
    now: i64,
    request: &PromptRequest<'_>,
) -> Result<Context, StoreError> {
    let agent_name: String = conn
        .query_row(
            "SELECT u.name FROM agent a JOIN user u ON u.id = a.user_id WHERE a.id = ?1",
            [request.agent_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| not_found("agent"))?;
    let project = get_project(conn, request.project)?;
    let key = project.key.as_str();

    struct IssueRow {
        id: i64,
        title: String,
        description: String,
        type_id: i64,
        type_name: String,
        column: String,
        state_type: StateType,
        priority: Option<String>,
        assignee: Option<String>,
        parent: Option<i64>,
        initiative: Option<String>,
    }
    let issue = conn
        .query_row(
            "SELECT i.id, i.title, i.description, i.type_id, t.name, c.name, c.state_type,
                    i.priority, au.name, par.number, n.name
             FROM issue i
             JOIN issue_type t ON t.id = i.type_id
             JOIN \"column\" c ON c.id = i.column_id
             LEFT JOIN user au ON au.id = i.assignee_id
             LEFT JOIN issue par ON par.id = i.parent_id
             LEFT JOIN initiative n ON n.id = i.initiative_id
             WHERE i.project_id = ?1 AND i.number = ?2",
            params![project.id, request.number],
            |row| {
                Ok(IssueRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    description: row.get(2)?,
                    type_id: row.get(3)?,
                    type_name: row.get(4)?,
                    column: row.get(5)?,
                    state_type: row.get(6)?,
                    priority: row.get(7)?,
                    assignee: row.get(8)?,
                    parent: row.get(9)?,
                    initiative: row.get(10)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| not_found("issue"))?;
    let mut truncated = false;

    let labels = {
        let mut stmt = conn.prepare(
            "SELECT l.name FROM issue_label il JOIN label l ON l.id = il.label_id
             WHERE il.issue_id = ?1 ORDER BY l.name LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![issue.id, (MAX_LIST_ITEMS + 1) as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        cap(rows, MAX_LIST_ITEMS)
    };
    let children = {
        let mut stmt = conn.prepare(
            "SELECT i.number, i.title, c.state_type FROM issue i
             JOIN \"column\" c ON c.id = i.column_id
             WHERE i.parent_id = ?1 AND i.archived = 0
             ORDER BY c.position, i.position, i.id LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![issue.id, (MAX_LIST_ITEMS + 1) as i64], |row| {
                Ok(issue_ref(
                    key,
                    row.get(0)?,
                    &row.get::<_, String>(1)?,
                    row.get(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        cap(rows, MAX_LIST_ITEMS)
    };

    let links = links_of(conn, key, issue.id)?;
    let refs = |list: &[crate::links::LinkedIssue], open_only: bool| {
        let all: Vec<Value> = list
            .iter()
            .filter(|l| !l.archived)
            .filter(|l| {
                !open_only || !matches!(l.state_type, StateType::Completed | StateType::Canceled)
            })
            .map(|l| issue_ref(key, l.number, &l.title, l.state_type))
            .collect();
        cap(all, MAX_LIST_ITEMS)
    };
    let blocks = refs(&links.blocks, false);
    let blocked_by = refs(&links.blocked_by, false);
    let related = refs(&links.related, false);
    let blockers = refs(&links.blocked_by, true);
    let documents = cap(
        documents_of_issue(conn, issue.id)?
            .into_iter()
            .map(|d| json!({ "title": d.title, "id": d.id }))
            .collect(),
        MAX_LIST_ITEMS,
    );

    let total_comments: i64 = conn.query_row(
        "SELECT COUNT(*) FROM comment WHERE issue_id = ?1 AND deleted IS NULL",
        [issue.id],
        |row| row.get(0),
    )?;
    let mut recent = {
        let mut stmt = conn.prepare(
            "SELECT c.id, u.name, u.kind, c.created, c.body
             FROM comment c JOIN user u ON u.id = c.author_id
             WHERE c.issue_id = ?1 AND c.deleted IS NULL
             ORDER BY c.id DESC LIMIT ?2",
        )?;
        stmt.query_map(params![issue.id, MAX_COMMENTS], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    recent.reverse();
    let omitted = (total_comments - i64::try_from(recent.len()).unwrap_or(0)).max(0);
    if omitted > 0 {
        truncated = true;
    }

    let mut trigger_text = String::new();
    let mut trigger_author: Option<i64> = None;
    let mut trigger_comment_id = None;
    if let Some(comment_id) = request.trigger.comment_id {
        let found: Option<(i64, i64, String, Option<i64>)> = conn
            .query_row(
                "SELECT issue_id, author_id, body, deleted FROM comment WHERE id = ?1",
                [comment_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        match found {
            Some((issue_id, author, body, deleted)) if issue_id == issue.id => {
                trigger_author = Some(author);
                if deleted.is_none() {
                    trigger_comment_id = Some(comment_id);
                    trigger_text = comment_body(&body, &mut truncated);
                }
            }
            _ => {
                return Err(invalid(format!(
                    "comment {comment_id} is not a comment of {key}-{}",
                    request.number
                )));
            }
        }
    }
    let actor_id = request.trigger.actor_id.or(trigger_author);
    let actor: String = match actor_id {
        Some(id) => conn
            .query_row("SELECT name FROM user WHERE id = ?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?,
        None => None,
    }
    .unwrap_or_default();

    let comments: Vec<Value> = recent
        .into_iter()
        .map(|(id, author, kind, created, body)| {
            json!({
                "author": author,
                "kind": kind,
                "created": iso8601(created),
                "body": comment_body(&body, &mut truncated),
                "is_trigger": trigger_comment_id == Some(id),
            })
        })
        .collect();

    for flagged in [
        labels.cut,
        children.cut,
        blocks.cut,
        blocked_by.cut,
        related.cut,
        blockers.cut,
        documents.cut,
    ] {
        truncated |= flagged;
    }

    let base = request.public_url.map(|url| url.trim_end_matches('/'));
    let path = format!("/p/{key}/{}", request.number);
    let kind = request.trigger.kind;
    let mut vars: Map<String, Value> = Map::new();
    let mut put = |name: &str, value: Value| {
        vars.insert(name.to_string(), value);
    };
    put("issue.key", text(format!("{key}-{}", request.number)));
    put("issue.number", json!(request.number));
    put("issue.title", text(issue.title));
    put("issue.description", text(issue.description));
    put("issue.type", text(issue.type_name));
    put("issue.column", text(issue.column));
    put("issue.state_type", text(issue.state_type.as_str()));
    put("issue.priority", text(issue.priority.unwrap_or_default()));
    put("issue.assignee", text(issue.assignee.unwrap_or_default()));
    put("issue.labels", json!(labels.items));
    put(
        "issue.parent",
        text(
            issue
                .parent
                .map_or_else(String::new, |number| format!("{key}-{number}")),
        ),
    );
    put("issue.children", Value::Array(children.items));
    put(
        "issue.url",
        text(match base {
            Some(base) => format!("{base}{path}"),
            None => path,
        }),
    );
    put("project.key", text(project.key.clone()));
    put("project.name", text(project.name.clone()));
    put("project.rules", text(project.rules.clone()));
    put(
        "initiative.name",
        text(issue.initiative.unwrap_or_default()),
    );
    put("comments", Value::Array(comments));
    put("comments.omitted", json!(omitted));
    put("links.blocks", Value::Array(blocks.items));
    put("links.blocked_by", Value::Array(blocked_by.items));
    put("links.related", Value::Array(related.items));
    put("blockers", Value::Array(blockers.items));
    put("documents", Value::Array(documents.items));
    put("trigger", text(kind.as_str()));
    put("trigger.assigned", json!(kind == TriggerKind::Assigned));
    put("trigger.mentioned", json!(kind == TriggerKind::Mentioned));
    put("trigger.schedule", json!(kind == TriggerKind::Schedule));
    put("trigger.manual", json!(kind == TriggerKind::Manual));
    put("trigger.comment", text(trigger_text));
    put("trigger.actor", text(actor));
    put("agent.name", text(agent_name));
    put("workdir", text(request.workdir.unwrap_or_default()));
    put(
        "run.id",
        text(request.run_id.map_or_else(String::new, |id| id.to_string())),
    );
    put("run.branch", text(request.branch.unwrap_or_default()));
    put(
        "server.url",
        text(match base {
            Some(base) => format!("{base}/mcp"),
            None => "/mcp".to_string(),
        }),
    );
    put("now", text(iso8601(now)));

    Ok(Context {
        vars: vars.into_iter().collect(),
        project_id: project.id,
        issue_type_id: issue.type_id,
        truncated,
    })
}
