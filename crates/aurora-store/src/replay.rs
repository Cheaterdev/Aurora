//! Rebuilds state from the event log alone and reads the same model from the
//! tables, so the two can be compared ([`replay`], [`table_snapshot`],
//! [`diff`]). It is the event/state agreement behind the invariant checker
//! (`invariants.rs`) and behind the replay test in `tests_events.rs`.
//!
//! # What is modelled
//!
//! Projects (key, name), columns (names), issue types, labels, issues (project,
//! number, title, type, column, assignee, parent, initiative, priority, labels,
//! archived), initiatives (name, status, target date), links, comments (issue,
//! parent, author, mentions, edited, deleted; only the ones a reader sees),
//! documents (parent, title, archived, revision, body length), document links
//! and registered agents (name, user, triggers, limits, allowed projects).
//!
//! # What the events do not carry, so it is not compared
//!
//! Issue and initiative descriptions (only ever in the log as "changed"),
//! comment and document body text (the log has the length only), the order of
//! columns and of issues inside a column (`position`), timestamps, the
//! agent's command, arguments, model, instructions, working directory and
//! description (the log never holds them: they may be secrets), users, API
//! keys and sessions (`user.*` and `key.*` events are accepted and ignored),
//! and the request log. Document bodies are covered by the revision chain
//! check instead.
//!
//! An event kind this module does not know is an error, so a new kind cannot be
//! added without teaching the replay about it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use rusqlite::Connection;
use serde_json::Value;

use crate::StoreError;
use crate::events::Event;

type Res<T> = Result<T, String>;

#[derive(Debug, PartialEq, Default)]
pub struct Snapshot {
    /// project id -> (key, name)
    pub projects: BTreeMap<i64, (String, String)>,
    /// project id -> column names, sorted
    pub columns: BTreeMap<i64, Vec<String>>,
    /// id -> (project id, name)
    pub types: BTreeMap<i64, (i64, String)>,
    /// id -> (project id, name, color)
    pub labels: BTreeMap<i64, (i64, String, String)>,
    /// issue id -> issue
    pub issues: BTreeMap<i64, IssueSnap>,
    /// initiative id -> initiative
    pub initiatives: BTreeMap<i64, InitiativeSnap>,
    /// link id -> (kind, project id, from number, to number); a `relates` link
    /// has the lower number first
    pub links: BTreeMap<i64, (String, i64, i64, i64)>,
    /// comment id -> comment, for the comments a reader sees
    pub comments: BTreeMap<i64, CommentSnap>,
    /// document id -> document, archived ones included
    pub docs: BTreeMap<i64, DocSnap>,
    /// document link id -> (document id, "issue" or "initiative", issue number or initiative id)
    pub doc_links: BTreeMap<i64, (i64, String, i64)>,
    /// agent id -> agent, without the command, arguments and instructions: the
    /// log never holds them
    pub agents: BTreeMap<i64, AgentSnap>,
}

#[derive(Debug, PartialEq)]
pub struct AgentSnap {
    pub name: String,
    pub user_id: i64,
    pub on_assign: bool,
    pub on_mention: bool,
    pub enabled: bool,
    pub max_concurrent: i64,
    pub timeout_seconds: i64,
    pub projects: Vec<String>,
}

/// What the log can say about a document: where it is, its title, its
/// revision number and the size of its body.
#[derive(Debug, PartialEq)]
pub struct DocSnap {
    pub project_id: i64,
    pub parent: Option<i64>,
    pub title: String,
    pub archived: bool,
    pub revision: i64,
    pub chars: i64,
}

#[derive(Debug, PartialEq)]
pub struct CommentSnap {
    pub issue_id: i64,
    pub parent: Option<i64>,
    pub author: i64,
    pub mentions: Vec<String>,
    pub edited: bool,
    pub deleted: bool,
}

/// The description is only in the log as a change, never at creation, so it
/// is not modelled.
#[derive(Debug, PartialEq)]
pub struct InitiativeSnap {
    pub project_id: i64,
    pub name: String,
    pub status: String,
    pub target_date: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct IssueSnap {
    pub project_id: i64,
    pub number: i64,
    pub title: String,
    pub type_id: i64,
    pub column_id: i64,
    pub assignee_id: Option<i64>,
    pub parent: Option<i64>,
    pub initiative: Option<i64>,
    pub priority: Option<String>,
    pub labels: Vec<i64>,
    pub archived: bool,
}

fn text(value: &Value) -> Res<String> {
    value
        .as_str()
        .map(String::from)
        .ok_or_else(|| format!("expected text, found {value}"))
}

fn int(value: &Value) -> Res<i64> {
    value
        .as_i64()
        .ok_or_else(|| format!("expected a number, found {value}"))
}

fn flag(value: &Value) -> Res<bool> {
    value
        .as_bool()
        .ok_or_else(|| format!("expected true or false, found {value}"))
}

fn list(value: &Value) -> Res<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| format!("expected a list, found {value}"))
}

fn strings(value: &Value) -> Res<Vec<String>> {
    list(value)?.iter().map(text).collect()
}

fn ids(value: &Value) -> Res<Vec<i64>> {
    list(value)?.iter().map(int).collect()
}

fn changes(payload: &Value) -> Res<&serde_json::Map<String, Value>> {
    payload["changes"]
        .as_object()
        .ok_or_else(|| "no changes object".to_string())
}

fn entry<'a, V>(map: &'a mut BTreeMap<i64, V>, id: i64, what: &str) -> Res<&'a mut V> {
    map.get_mut(&id)
        .ok_or_else(|| format!("{what} {id} does not exist at this point of the log"))
}

fn doc_target(target: &Value) -> Res<(String, i64)> {
    match text(&target["kind"])?.as_str() {
        "issue" => Ok(("issue".to_string(), int(&target["number"])?)),
        "initiative" => Ok(("initiative".to_string(), int(&target["id"])?)),
        other => Err(format!("unknown document link target {other}")),
    }
}

pub fn link_snap(kind: &str, pid: i64, from: i64, to: i64) -> (String, i64, i64, i64) {
    if kind == "relates" {
        (kind.to_string(), pid, from.min(to), from.max(to))
    } else {
        (kind.to_string(), pid, from, to)
    }
}

/// What readers see: a deleted reply is gone, a deleted top-level comment only
/// stays while a live reply hangs off it, and a deleted comment has no mentions.
pub fn visible(comments: &mut BTreeMap<i64, CommentSnap>) {
    let with_replies: BTreeSet<i64> = comments
        .values()
        .filter(|c| !c.deleted)
        .filter_map(|c| c.parent)
        .collect();
    let gone: Vec<i64> = comments
        .iter()
        .filter(|(id, c)| c.deleted && (c.parent.is_some() || !with_replies.contains(id)))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        comments.remove(&id);
    }
    for c in comments.values_mut().filter(|c| c.deleted) {
        c.mentions.clear();
    }
}

/// The events that have no project: users, keys and the agent registry. `true`
/// when `e` was one of them.
fn apply_registry(s: &mut Snapshot, e: &Event) -> Res<bool> {
    let p = &e.payload;
    match e.kind.as_str() {
        // Users and keys are not modelled here.
        "user.created" | "key.created" | "key.rotated" | "key.revoked" => {}
        "agent.created" => {
            s.agents.insert(
                e.entity_id,
                AgentSnap {
                    name: text(&p["name"])?,
                    user_id: int(&p["user_id"])?,
                    on_assign: flag(&p["on_assign"])?,
                    on_mention: flag(&p["on_mention"])?,
                    enabled: flag(&p["enabled"])?,
                    max_concurrent: int(&p["max_concurrent"])?,
                    timeout_seconds: int(&p["timeout_seconds"])?,
                    projects: strings(&p["projects"])?,
                },
            );
        }
        "agent.updated" => {
            let agent = entry(&mut s.agents, e.entity_id, "agent")?;
            for (field, change) in changes(p)? {
                let to = &change["to"];
                match field.as_str() {
                    "on_assign" => agent.on_assign = flag(to)?,
                    "on_mention" => agent.on_mention = flag(to)?,
                    "enabled" => agent.enabled = flag(to)?,
                    "max_concurrent" => agent.max_concurrent = int(to)?,
                    "timeout_seconds" => agent.timeout_seconds = int(to)?,
                    "projects" => agent.projects = strings(to)?,
                    // Text that may hold a secret: the log only says it changed.
                    "description" | "command" | "args" | "model" | "instructions" | "work_dir" => {
                        if change != &serde_json::json!({ "changed": true }) {
                            return Err(format!("{field} change carries text: {change}"));
                        }
                    }
                    other => return Err(format!("unknown agent field {other}")),
                }
            }
        }
        "agent.deleted" => {
            s.agents.remove(&e.entity_id);
        }
        _ => return Ok(false),
    }
    if e.project_id.is_some() {
        return Err(format!("{} must have no project", e.kind));
    }
    Ok(true)
}

/// Rebuilds the model event by event, so a long log need not be held in memory.
#[derive(Default)]
pub struct Replayer {
    snapshot: Snapshot,
}

impl Replayer {
    /// Applies the next event, oldest first. The error names the event that
    /// does not fit what came before it.
    pub fn push(&mut self, e: &Event) -> Result<(), String> {
        apply(&mut self.snapshot, e).map_err(|why| format!("event {} ({}): {why}", e.id, e.kind))
    }

    pub fn finish(self) -> Snapshot {
        let mut s = self.snapshot;
        visible(&mut s.comments);
        for names in s.columns.values_mut() {
            names.sort();
        }
        s
    }
}

/// Rebuilds the model from `log` (oldest first).
pub fn replay(log: &[Event]) -> Result<Snapshot, String> {
    let mut replayer = Replayer::default();
    for e in log {
        replayer.push(e)?;
    }
    Ok(replayer.finish())
}

fn apply(s: &mut Snapshot, e: &Event) -> Res<()> {
    if apply_registry(s, e)? {
        return Ok(());
    }
    let pid = e.project_id.ok_or("the event has no project")?;
    let p = &e.payload;
    match e.kind.as_str() {
        "project.created" => {
            s.projects
                .insert(pid, (text(&p["key"])?, text(&p["name"])?));
            s.columns.insert(pid, strings(&p["columns"])?);
        }
        "project.updated" => entry(&mut s.projects, pid, "project")?.1 = text(&p["name"])?,
        "column.created" => entry(&mut s.columns, pid, "project columns")?.push(text(&p["name"])?),
        "column.renamed" => {
            let names = entry(&mut s.columns, pid, "project columns")?;
            let old = text(&p["previous_name"])?;
            let at = names
                .iter()
                .position(|n| *n == old)
                .ok_or_else(|| format!("no column {old}"))?;
            names[at] = text(&p["name"])?;
        }
        "column.deleted" => {
            let names = entry(&mut s.columns, pid, "project columns")?;
            let old = text(&p["name"])?;
            let at = names
                .iter()
                .position(|n| *n == old)
                .ok_or_else(|| format!("no column {old}"))?;
            names.remove(at);
        }
        "column.reordered" => {}
        "issue_type.created" => {
            s.types.insert(e.entity_id, (pid, text(&p["name"])?));
        }
        "issue_type.renamed" => {
            entry(&mut s.types, e.entity_id, "issue type")?.1 = text(&p["name"])?
        }
        "issue_type.deleted" => {
            s.types.remove(&e.entity_id);
        }
        "label.created" => {
            s.labels
                .insert(e.entity_id, (pid, text(&p["name"])?, text(&p["color"])?));
        }
        "label.renamed" => entry(&mut s.labels, e.entity_id, "label")?.1 = text(&p["name"])?,
        "label.recolored" => entry(&mut s.labels, e.entity_id, "label")?.2 = text(&p["color"])?,
        "label.deleted" => {
            s.labels.remove(&e.entity_id);
            for issue in s.issues.values_mut().filter(|i| i.project_id == pid) {
                issue.labels.retain(|l| *l != e.entity_id);
            }
        }
        "issue.created" => {
            s.issues.insert(
                e.entity_id,
                IssueSnap {
                    project_id: pid,
                    number: int(&p["number"])?,
                    title: text(&p["title"])?,
                    type_id: int(&p["type_id"])?,
                    column_id: int(&p["column_id"])?,
                    assignee_id: p["assignee_id"].as_i64(),
                    parent: p["parent"].as_i64(),
                    initiative: p["initiative"].as_i64(),
                    priority: p["priority"].as_str().map(String::from),
                    labels: ids(&p["labels"])?,
                    archived: false,
                },
            );
        }
        "initiative.created" => {
            s.initiatives.insert(
                e.entity_id,
                InitiativeSnap {
                    project_id: pid,
                    name: text(&p["name"])?,
                    status: text(&p["status"])?,
                    target_date: p["target_date"].as_str().map(String::from),
                },
            );
        }
        "initiative.updated" => {
            let initiative = entry(&mut s.initiatives, e.entity_id, "initiative")?;
            for (field, change) in changes(p)? {
                let to = &change["to"];
                match field.as_str() {
                    "name" => initiative.name = text(to)?,
                    "status" => initiative.status = text(to)?,
                    "target_date" => initiative.target_date = to.as_str().map(String::from),
                    "description" => {}
                    other => return Err(format!("unknown initiative field {other}")),
                }
            }
        }
        "initiative.deleted" => {
            // The deleted initiative's issues are unassigned by their own
            // issue.updated events, which come first.
            if s.issues.values().any(|i| i.initiative == Some(e.entity_id)) {
                return Err("issues still point at the deleted initiative".into());
            }
            s.initiatives.remove(&e.entity_id);
        }
        "issue.updated" => {
            let issue = entry(&mut s.issues, e.entity_id, "issue")?;
            for (field, change) in changes(p)? {
                let to = &change["to"];
                match field.as_str() {
                    "title" => issue.title = text(to)?,
                    "type_id" => issue.type_id = int(to)?,
                    "assignee_id" => issue.assignee_id = to.as_i64(),
                    "parent" => issue.parent = to.as_i64(),
                    "initiative" => issue.initiative = to.as_i64(),
                    "priority" => issue.priority = to.as_str().map(String::from),
                    "labels" => issue.labels = ids(to)?,
                    // The description is only in the log as a change, never at creation.
                    "description" => {}
                    other => return Err(format!("unknown issue field {other}")),
                }
            }
        }
        "issue.moved" => {
            entry(&mut s.issues, e.entity_id, "issue")?.column_id = int(&p["to"]["column_id"])?;
        }
        "issue.archived" => entry(&mut s.issues, e.entity_id, "issue")?.archived = true,
        "issue.unarchived" => entry(&mut s.issues, e.entity_id, "issue")?.archived = false,
        "link.added" => {
            let snap = link_snap(
                &text(&p["kind"])?,
                pid,
                int(&p["from"]["number"])?,
                int(&p["to"]["number"])?,
            );
            s.links.insert(int(&p["link_id"])?, snap);
        }
        "link.removed" => {
            s.links.remove(&int(&p["link_id"])?);
        }
        "comment.created" => {
            s.comments.insert(
                int(&p["comment_id"])?,
                CommentSnap {
                    issue_id: e.entity_id,
                    parent: p["parent"].as_i64(),
                    author: int(&p["author"]["id"])?,
                    mentions: strings(&p["mentions"])?,
                    edited: false,
                    deleted: false,
                },
            );
        }
        "comment.edited" => {
            let c = entry(&mut s.comments, int(&p["comment_id"])?, "comment")?;
            c.mentions = strings(&p["mentions"])?;
            c.edited = true;
        }
        "comment.deleted" => {
            entry(&mut s.comments, int(&p["comment_id"])?, "comment")?.deleted = true;
        }
        "mention.created" => {
            // No state of its own: it names a user the comment mentions.
            let c = entry(&mut s.comments, int(&p["comment_id"])?, "comment")?;
            if !c.mentions.contains(&text(&p["user_name"])?) || c.issue_id != e.entity_id {
                return Err("the mention does not match its comment".into());
            }
        }
        "document.created" => {
            s.docs.insert(
                e.entity_id,
                DocSnap {
                    project_id: pid,
                    parent: p["parent"].as_i64(),
                    title: text(&p["title"])?,
                    archived: false,
                    revision: int(&p["revision"])?,
                    chars: int(&p["chars"])?,
                },
            );
        }
        "document.updated" => {
            let doc = entry(&mut s.docs, e.entity_id, "document")?;
            doc.revision = int(&p["revision"])?;
            doc.chars = int(&p["chars"])?;
            for (field, change) in changes(p)? {
                match field.as_str() {
                    "title" => doc.title = text(&change["to"])?,
                    other => return Err(format!("unknown document field {other}")),
                }
            }
        }
        "document.moved" => {
            entry(&mut s.docs, e.entity_id, "document")?.parent = p["to_parent"].as_i64();
        }
        "document.archived" => entry(&mut s.docs, e.entity_id, "document")?.archived = true,
        "document.unarchived" => entry(&mut s.docs, e.entity_id, "document")?.archived = false,
        "document.restored" => {
            let doc = entry(&mut s.docs, e.entity_id, "document")?;
            doc.title = text(&p["title"])?;
            doc.revision = int(&p["revision"])?;
            doc.chars = int(&p["chars"])?;
        }
        "document.link_added" => {
            let (kind, target) = doc_target(&p["target"])?;
            s.doc_links
                .insert(int(&p["link_id"])?, (e.entity_id, kind, target));
        }
        "document.link_removed" => {
            s.doc_links.remove(&int(&p["link_id"])?);
        }
        other => return Err(format!("the replay does not know the event kind {other}")),
    }
    Ok(())
}

// ---- the same model, read from the tables ----

/// The model as the tables hold it, for comparison with [`replay`]. Reads with
/// plain SQL so that no page limit or visibility rule of the API gets in the way.
pub fn table_snapshot(conn: &Connection) -> Result<Snapshot, StoreError> {
    let mut s = Snapshot::default();

    let mut stmt = conn.prepare("SELECT id, key, name FROM project")?;
    for row in stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))? {
        let (id, value) = row?;
        s.projects.insert(id, value);
        s.columns.entry(id).or_default();
    }
    let mut stmt = conn.prepare(r#"SELECT project_id, name FROM "column""#)?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (project, name) = row?;
        s.columns.entry(project).or_default().push(name);
    }
    for names in s.columns.values_mut() {
        names.sort();
    }

    let mut stmt = conn.prepare("SELECT id, project_id, name FROM issue_type")?;
    for row in stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))? {
        let (id, value) = row?;
        s.types.insert(id, value);
    }
    let mut stmt = conn.prepare("SELECT id, project_id, name, color FROM label")?;
    for row in stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))? {
        let (id, value) = row?;
        s.labels.insert(id, value);
    }

    let mut stmt = conn.prepare(
        "SELECT i.id, i.project_id, i.number, i.title, i.type_id, i.column_id, i.assignee_id,
                (SELECT p.number FROM issue p WHERE p.id = i.parent_id),
                i.initiative_id, i.priority, i.archived
         FROM issue i",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            IssueSnap {
                project_id: r.get(1)?,
                number: r.get(2)?,
                title: r.get(3)?,
                type_id: r.get(4)?,
                column_id: r.get(5)?,
                assignee_id: r.get(6)?,
                parent: r.get(7)?,
                initiative: r.get(8)?,
                priority: r.get(9)?,
                labels: Vec::new(),
                archived: r.get::<_, i64>(10)? != 0,
            },
        ))
    })?;
    for row in rows {
        let (id, issue) = row?;
        s.issues.insert(id, issue);
    }
    let mut stmt =
        conn.prepare("SELECT issue_id, label_id FROM issue_label ORDER BY issue_id, label_id")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
        let (issue, label) = row?;
        if let Some(issue) = s.issues.get_mut(&issue) {
            issue.labels.push(label);
        }
    }

    let mut stmt =
        conn.prepare("SELECT id, project_id, name, status, target_date FROM initiative")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            InitiativeSnap {
                project_id: r.get(1)?,
                name: r.get(2)?,
                status: r.get(3)?,
                target_date: r.get(4)?,
            },
        ))
    })?;
    for row in rows {
        let (id, initiative) = row?;
        s.initiatives.insert(id, initiative);
    }

    let mut stmt = conn.prepare(
        "SELECT l.id, l.kind, f.project_id, f.number, t.number
         FROM issue_link l JOIN issue f ON f.id = l.from_issue_id JOIN issue t ON t.id = l.to_issue_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (id, kind, project, from, to) = row?;
        s.links.insert(id, link_snap(&kind, project, from, to));
    }

    let mut stmt =
        conn.prepare("SELECT id, issue_id, parent_id, author_id, edited, deleted FROM comment")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            CommentSnap {
                issue_id: r.get(1)?,
                parent: r.get(2)?,
                author: r.get(3)?,
                mentions: Vec::new(),
                edited: r.get::<_, Option<i64>>(4)?.is_some(),
                deleted: r.get::<_, Option<i64>>(5)?.is_some(),
            },
        ))
    })?;
    for row in rows {
        let (id, comment) = row?;
        s.comments.insert(id, comment);
    }
    let mut stmt = conn.prepare(
        "SELECT m.comment_id, u.name FROM comment_mention m JOIN user u ON u.id = m.user_id
         ORDER BY m.comment_id, m.position",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (comment, name) = row?;
        if let Some(comment) = s.comments.get_mut(&comment) {
            comment.mentions.push(name);
        }
    }
    visible(&mut s.comments);

    let mut stmt = conn.prepare(
        "SELECT id, project_id, parent_id, title, archived, revision, length(body) FROM document",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            DocSnap {
                project_id: r.get(1)?,
                parent: r.get(2)?,
                title: r.get(3)?,
                archived: r.get::<_, i64>(4)? != 0,
                revision: r.get(5)?,
                chars: r.get(6)?,
            },
        ))
    })?;
    for row in rows {
        let (id, doc) = row?;
        s.docs.insert(id, doc);
    }
    let mut stmt = conn.prepare(
        "SELECT l.id, l.document_id, CASE WHEN l.issue_id IS NULL THEN 'initiative' ELSE 'issue' END,
                COALESCE(i.number, l.initiative_id)
         FROM document_link l LEFT JOIN issue i ON i.id = l.issue_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            (r.get(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?),
        ))
    })?;
    for row in rows {
        let (id, value) = row?;
        s.doc_links.insert(id, value);
    }

    let mut stmt = conn.prepare(
        "SELECT a.id, u.name, a.user_id, a.on_assign, a.on_mention, a.enabled, a.max_concurrent,
                a.timeout_seconds
         FROM agent a JOIN user u ON u.id = a.user_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            AgentSnap {
                name: r.get(1)?,
                user_id: r.get(2)?,
                on_assign: r.get::<_, i64>(3)? != 0,
                on_mention: r.get::<_, i64>(4)? != 0,
                enabled: r.get::<_, i64>(5)? != 0,
                max_concurrent: r.get(6)?,
                timeout_seconds: r.get(7)?,
                projects: Vec::new(),
            },
        ))
    })?;
    for row in rows {
        let (id, agent) = row?;
        s.agents.insert(id, agent);
    }
    let mut stmt = conn.prepare(
        "SELECT ap.agent_id, p.key FROM agent_project ap JOIN project p ON p.id = ap.project_id
         ORDER BY ap.agent_id, p.key",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (agent, key) = row?;
        if let Some(agent) = s.agents.get_mut(&agent) {
            agent.projects.push(key);
        }
    }
    Ok(s)
}

const MAX_DIFFS_PER_KIND: usize = 5;

fn diff_map<V: Debug + PartialEq>(
    kind: &str,
    from_log: &BTreeMap<i64, V>,
    from_tables: &BTreeMap<i64, V>,
    out: &mut Vec<String>,
) {
    let mut found = 0;
    let mut note = |line: String| {
        found += 1;
        if found <= MAX_DIFFS_PER_KIND {
            out.push(line);
        }
    };
    for (id, expected) in from_log {
        match from_tables.get(id) {
            None => note(format!("{kind} {id}: the log has it, the tables do not")),
            Some(actual) if actual != expected => note(format!(
                "{kind} {id}: the log says {expected:?}, the tables say {actual:?}"
            )),
            Some(_) => {}
        }
    }
    for id in from_tables.keys().filter(|id| !from_log.contains_key(id)) {
        note(format!("{kind} {id}: the tables have it, the log does not"));
    }
    if found > MAX_DIFFS_PER_KIND {
        out.push(format!(
            "{kind}: {} more differences",
            found - MAX_DIFFS_PER_KIND
        ));
    }
}

/// The differences between the model the log produces and the one the tables
/// hold, a few per entity kind; empty when they agree.
pub fn diff(from_log: &Snapshot, from_tables: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    diff_map(
        "project",
        &from_log.projects,
        &from_tables.projects,
        &mut out,
    );
    diff_map(
        "columns of project",
        &from_log.columns,
        &from_tables.columns,
        &mut out,
    );
    diff_map("issue type", &from_log.types, &from_tables.types, &mut out);
    diff_map("label", &from_log.labels, &from_tables.labels, &mut out);
    diff_map("issue", &from_log.issues, &from_tables.issues, &mut out);
    diff_map(
        "initiative",
        &from_log.initiatives,
        &from_tables.initiatives,
        &mut out,
    );
    diff_map("link", &from_log.links, &from_tables.links, &mut out);
    diff_map(
        "comment",
        &from_log.comments,
        &from_tables.comments,
        &mut out,
    );
    diff_map("document", &from_log.docs, &from_tables.docs, &mut out);
    diff_map(
        "document link",
        &from_log.doc_links,
        &from_tables.doc_links,
        &mut out,
    );
    diff_map("agent", &from_log.agents, &from_tables.agents, &mut out);
    out
}
