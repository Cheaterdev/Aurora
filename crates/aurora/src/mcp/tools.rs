//! The tools of the MCP endpoint: definitions (name, description, JSON Schema,
//! annotations) and their implementations.
//!
//! Every tool calls the same store functions as the REST handlers, with the
//! key's [`Principal`] as the actor, in one transaction (`Store::write`) or one
//! read (`Store::read`). Name-or-id conveniences (a column by name or state
//! type, a label or a user by name) are resolved inside that transaction, so
//! a call is atomic and the lookup cannot go stale.
//!
//! A tool that fails returns a result with `isError: true` and a sentence that
//! says what to fix (the protocol-level errors are the transport's business).

use std::sync::OnceLock;

use aurora_store::agents;
use aurora_store::auth::{self, Principal};
use aurora_store::comments::{self, NewComment};
use aurora_store::documents::{self, DocumentPatch, NewDocument};
use aurora_store::initiatives::{self, InitiativeStatus};
use aurora_store::issues::{self, IssueFilter, IssuePatch, MoveTarget, NewIssue, ParentFilter};
use aurora_store::links::{self, LinkKind};
use aurora_store::projects::{self, StateType};
use aurora_store::rusqlite::Connection;
use aurora_store::search::{self, Kind, SearchQuery, Segment};
use aurora_store::{Rejection, Store, StoreError, now_millis};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::auth::Access;
use crate::routes::LinkTarget;

/// What a successful tool returns: a short sentence and the data.
struct Done {
    summary: String,
    data: Value,
}

type Outcome = Result<Done, String>;

const DEFAULT_ISSUE_PAGE: i64 = 25;
const MAX_ISSUE_PAGE: i64 = 100;
const DEFAULT_COMMENT_PAGE: usize = 30;
const MAX_COMMENT_PAGE: usize = 100;
const DEFAULT_DOCUMENT_PAGE: usize = 100;
const MAX_DOCUMENT_PAGE: usize = 500;
const DEFAULT_SEARCH_PAGE: i64 = 10;
const MAX_SEARCH_PAGE: i64 = 50;
const LIST_DESCRIPTION_CHARS: usize = 300;
const LIST_COMMENT_CHARS: usize = 2_000;
const DEFAULT_BODY_CHARS: usize = 50_000;

// ---- definitions ----

pub struct Spec {
    pub name: &'static str,
    description: &'static str,
    pub read_only: bool,
    destructive: bool,
    idempotent: bool,
    schema: Value,
}

impl Spec {
    fn definition(&self) -> Value {
        let title = self
            .name
            .split('_')
            .enumerate()
            .map(|(i, word)| {
                if i == 0 {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                        .unwrap_or_default()
                } else {
                    word.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        json!({
            "name": self.name,
            "title": title,
            "description": self.description,
            "inputSchema": self.schema,
            "annotations": {
                "title": title,
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": false,
            },
        })
    }
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn integer(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

fn nullable_integer(description: &str) -> Value {
    json!({ "type": ["integer", "null"], "description": description })
}

fn boolean(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

fn object(properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let properties: Map<String, Value> = properties
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

const ISSUE_HELP: &str = "KEY-n, e.g. TRK-12 (alternative to project + number)";

fn issue_props() -> Vec<(&'static str, Value)> {
    vec![
        ("issue", string(ISSUE_HELP)),
        (
            "project",
            string("Project key; only with number, instead of issue"),
        ),
        ("number", integer("Issue number; only with project")),
    ]
}

fn with(
    mut base: Vec<(&'static str, Value)>,
    more: Vec<(&'static str, Value)>,
) -> Vec<(&'static str, Value)> {
    base.extend(more);
    base
}

fn column_props() -> Vec<(&'static str, Value)> {
    vec![
        ("column_id", integer("Column id (from get_project)")),
        ("column", string("Column name (case-insensitive)")),
        (
            "state_type",
            json!({
                "type": "string",
                "enum": ["backlog", "unstarted", "started", "completed", "canceled"],
                "description": "The first column of this state type, in board order",
            }),
        ),
    ]
}

fn priority_schema(nullable: bool) -> Value {
    if nullable {
        json!({
            "type": ["string", "null"],
            "enum": ["urgent", "high", "medium", "low", null],
            "description": "Priority; null clears it",
        })
    } else {
        json!({
            "type": "string",
            "enum": ["urgent", "high", "medium", "low"],
            "description": "Priority",
        })
    }
}

fn labels_schema(what: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": ["integer", "string"] },
        "description": what,
    })
}

fn link_to_schema() -> Value {
    json!({
        "type": ["integer", "string"],
        "description": "The other issue: a number in the same project or KEY-n",
    })
}

fn read_spec(name: &'static str, description: &'static str, schema: Value) -> Spec {
    Spec {
        name,
        description,
        read_only: true,
        destructive: false,
        idempotent: true,
        schema,
    }
}

fn write_spec(
    name: &'static str,
    description: &'static str,
    schema: Value,
    destructive: bool,
    idempotent: bool,
) -> Spec {
    Spec {
        name,
        description,
        read_only: false,
        destructive,
        idempotent,
        schema,
    }
}

fn build_specs() -> Vec<Spec> {
    let project = || ("project", string("Project key, e.g. TRK"));
    vec![
        read_spec(
            "list_projects",
            "List the projects with their key and name.",
            object(vec![], &[]),
        ),
        read_spec(
            "get_project",
            "Describe one project: its columns with state types (use state types, not names, to decide what a column means), issue types, labels and initiatives with progress. Call this before creating or moving issues to learn the ids.",
            object(vec![project()], &["project"]),
        ),
        read_spec(
            "list_issues",
            "List the issues of a project, ordered by column and position, with filters. Descriptions are shortened; use get_issue for the full text. Results are paged: pass next_offset as offset to continue.",
            object(
                with(
                    vec![project()],
                    vec![
                        ("column_id", integer("Only issues in this column id")),
                        ("column", string("Only issues in the column with this name")),
                        (
                            "state_type",
                            json!({
                                "type": "string",
                                "enum": ["backlog", "unstarted", "started", "completed", "canceled"],
                                "description": "Only issues in columns of this state type",
                            }),
                        ),
                        ("label_id", integer("Only issues with this label id")),
                        ("label", string("Only issues with the label of this name")),
                        (
                            "assignee_id",
                            integer("Only issues assigned to this user id"),
                        ),
                        (
                            "parent",
                            json!({
                                "type": ["integer", "string"],
                                "description": "Only children of this issue number, or the text none for top-level issues",
                            }),
                        ),
                        (
                            "include_archived",
                            boolean("Include archived issues (default false)"),
                        ),
                        ("archived_only", boolean("Only archived issues")),
                        ("limit", integer("Page size, default 25, at most 100")),
                        ("offset", integer("Issues to skip, default 0")),
                    ],
                ),
                &["project"],
            ),
        ),
        read_spec(
            "get_issue",
            "Read one issue completely: fields, column, labels, children, links (blocks, blocked_by, related), linked documents and the number of comments. Use list_comments for the comments.",
            object(issue_props(), &[]),
        ),
        write_spec(
            "create_issue",
            "Create an issue. type is needed unless the project has exactly one issue type; the column defaults to the first backlog column.",
            object(
                with(
                    with(
                        vec![
                            project(),
                            ("title", string("Title, at most 300 characters")),
                            (
                                "description",
                                string("Description (Markdown), at most 50000 characters"),
                            ),
                            ("type_id", integer("Issue type id (from get_project)")),
                            ("type", string("Issue type name, instead of type_id")),
                        ],
                        column_props(),
                    ),
                    vec![
                        ("assignee_id", integer("User id to assign")),
                        (
                            "assignee",
                            string("User name to assign, instead of assignee_id"),
                        ),
                        ("parent", link_to_schema()),
                        ("initiative", integer("Initiative id of the same project")),
                        ("priority", priority_schema(false)),
                        ("labels", labels_schema("Label ids or names")),
                    ],
                ),
                &["project", "title"],
            ),
            false,
            false,
        ),
        write_spec(
            "update_issue",
            "Change fields of an issue; only the arguments you pass change, and null clears description, assignee_id, assignee, parent, initiative and priority. labels replaces the whole label set. To change the column use move_issue.",
            object(
                with(
                    issue_props(),
                    vec![
                        ("title", string("New title")),
                        (
                            "description",
                            json!({"type": ["string", "null"], "description": "New description; null clears it"}),
                        ),
                        ("type_id", integer("Issue type id")),
                        ("type", string("Issue type name, instead of type_id")),
                        ("assignee_id", nullable_integer("User id; null unassigns")),
                        (
                            "assignee",
                            json!({"type": ["string", "null"], "description": "User name, instead of assignee_id; null unassigns"}),
                        ),
                        (
                            "parent",
                            json!({"type": ["integer", "string", "null"], "description": "Parent issue number or KEY-n of the same project; null makes it top-level"}),
                        ),
                        (
                            "initiative",
                            nullable_integer("Initiative id; null clears it"),
                        ),
                        ("priority", priority_schema(true)),
                        (
                            "labels",
                            labels_schema("The full new set of label ids or names"),
                        ),
                    ],
                ),
                &[],
            ),
            false,
            true,
        ),
        write_spec(
            "move_issue",
            "Move an issue to a column, naming it by column_id, column (name) or state_type (the first column of that state type, e.g. completed). before or after (issue numbers in the target column) place it; without them it goes to the end.",
            object(
                with(
                    with(issue_props(), column_props()),
                    vec![("before", link_to_schema()), ("after", link_to_schema())],
                ),
                &[],
            ),
            false,
            true,
        ),
        write_spec(
            "archive_issue",
            "Archive an issue (soft delete: it keeps its number and can be restored with unarchive_issue).",
            object(issue_props(), &[]),
            false,
            true,
        ),
        write_spec(
            "unarchive_issue",
            "Restore an archived issue.",
            object(issue_props(), &[]),
            false,
            true,
        ),
        write_spec(
            "add_comment",
            "Comment on an issue. @name mentions a user (an agent user is notified). parent replies to a top-level comment.",
            object(
                with(
                    issue_props(),
                    vec![
                        ("body", string("Plain text, 1 to 20000 characters")),
                        (
                            "parent",
                            integer(
                                "Comment id of a live top-level comment of the issue, to reply",
                            ),
                        ),
                    ],
                ),
                &["body"],
            ),
            false,
            false,
        ),
        read_spec(
            "list_comments",
            "List the comments of an issue, oldest first, each with its replies. Long bodies are shortened.",
            object(
                with(
                    issue_props(),
                    vec![
                        (
                            "limit",
                            integer("Top-level comments per page, default 30, at most 100"),
                        ),
                        ("offset", integer("Top-level comments to skip")),
                    ],
                ),
                &[],
            ),
        ),
        write_spec(
            "edit_comment",
            "Replace the body of a comment. Only its author or the owner may.",
            object(
                vec![
                    ("project", string("Project key (or pass issue)")),
                    ("issue", string(ISSUE_HELP)),
                    ("comment_id", integer("Comment id")),
                    ("body", string("New plain text body")),
                ],
                &["comment_id", "body"],
            ),
            false,
            true,
        ),
        write_spec(
            "delete_comment",
            "Delete a comment (its replies stay). Only its author or the owner may.",
            object(
                vec![
                    ("project", string("Project key (or pass issue)")),
                    ("issue", string(ISSUE_HELP)),
                    ("comment_id", integer("Comment id")),
                ],
                &["comment_id"],
            ),
            true,
            true,
        ),
        write_spec(
            "link_issues",
            "Link two issues of one project. kind blocks: issue blocks to; blocked_by: to blocks issue; relates: symmetric. A link that would close a blocking cycle is refused.",
            object(
                with(
                    issue_props(),
                    vec![
                        ("to", link_to_schema()),
                        (
                            "kind",
                            json!({"type": "string", "enum": ["blocks", "blocked_by", "relates"], "description": "How issue relates to to"}),
                        ),
                    ],
                ),
                &["to", "kind"],
            ),
            false,
            false,
        ),
        write_spec(
            "unlink_issues",
            "Remove a link by its link id (shown by get_issue as link_id).",
            object(
                vec![
                    ("project", string("Project key (or pass issue)")),
                    ("issue", string(ISSUE_HELP)),
                    ("link_id", integer("Link id")),
                ],
                &["link_id"],
            ),
            true,
            true,
        ),
        read_spec(
            "search",
            "Full-text search over issues, comments and documents. An identifier such as TRK-12 finds the issue directly. Results are paged.",
            object(
                vec![
                    ("q", string("Words to find, or an identifier")),
                    ("project", string("Limit to one project key")),
                    (
                        "kinds",
                        json!({"type": "array", "items": {"type": "string", "enum": ["issue", "comment", "document"]}, "description": "Limit to these kinds; default all"}),
                    ),
                    (
                        "include_archived",
                        boolean("Include archived issues and documents"),
                    ),
                    ("limit", integer("Hits per page, default 10, at most 50")),
                    ("offset", integer("Hits to skip")),
                ],
                &["q"],
            ),
        ),
        read_spec(
            "list_documents",
            "List the documents of a project as a tree (depth-first, with depth), without bodies.",
            object(
                vec![
                    project(),
                    ("include_archived", boolean("Include archived documents")),
                    ("limit", integer("Default 100, at most 500")),
                    ("offset", integer("Documents to skip")),
                ],
                &["project"],
            ),
        ),
        read_spec(
            "get_document",
            "Read a document: Markdown body, revision number, ancestors, children and links. Pass the revision as base_revision to update_document.",
            object(
                vec![
                    project(),
                    ("id", integer("Document id")),
                    (
                        "max_body_chars",
                        integer("Cut the body after this many characters; default 50000"),
                    ),
                ],
                &["project", "id"],
            ),
        ),
        write_spec(
            "create_document",
            "Create a document (Markdown), optionally under a parent document.",
            object(
                vec![
                    project(),
                    ("title", string("Title, at most 200 characters")),
                    ("body", string("Markdown body, at most 200000 characters")),
                    ("parent", integer("Parent document id")),
                ],
                &["project", "title"],
            ),
            false,
            false,
        ),
        write_spec(
            "update_document",
            "Change a document's title, body or parent; each change of title or body makes a new revision. Pass base_revision (from get_document) so that an edit made meanwhile by someone else is reported as a conflict instead of overwritten.",
            object(
                vec![
                    project(),
                    ("id", integer("Document id")),
                    ("title", string("New title")),
                    (
                        "body",
                        string("New Markdown body (replaces the whole body)"),
                    ),
                    (
                        "parent",
                        nullable_integer("New parent document id; null makes it top-level"),
                    ),
                    (
                        "base_revision",
                        integer("The revision this edit is based on"),
                    ),
                ],
                &["project", "id"],
            ),
            false,
            true,
        ),
        read_spec(
            "list_initiatives",
            "List the initiatives of a project with status, target date, progress and health.",
            object(
                vec![
                    project(),
                    (
                        "status",
                        json!({"type": "string", "enum": ["planned", "active", "paused", "completed", "canceled"], "description": "Only this status"}),
                    ),
                ],
                &["project"],
            ),
        ),
        read_spec(
            "get_current_user",
            "Who you are: the user of the API key, its role and the key's scope.",
            object(vec![], &[]),
        ),
        read_spec(
            "list_agents",
            "List the registered agents with their name, description, triggers (on_assign, on_mention), enabled flag, limits and allowed projects. Never their command line. Assign an issue to an agent's user or mention it with @name to wake it.",
            object(vec![], &[]),
        ),
    ]
}

fn specs() -> &'static [Spec] {
    static SPECS: OnceLock<Vec<Spec>> = OnceLock::new();
    SPECS.get_or_init(build_specs)
}

/// The `tools` array of `tools/list`.
pub fn definitions() -> Vec<Value> {
    specs().iter().map(Spec::definition).collect()
}

// ---- arguments ----

/// The arguments object, read key by key; what is left over is an error.
struct Args {
    map: Map<String, Value>,
}

fn expected(key: &str, what: &str) -> String {
    format!("argument `{key}` must be {what}")
}

fn as_string(key: &str, value: Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text),
        _ => Err(expected(key, "a string")),
    }
}

fn as_i64(key: &str, value: Value) -> Result<i64, String> {
    match &value {
        Value::Number(n) => n.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| expected(key, "an integer"))
}

impl Args {
    fn new(value: Value) -> Result<Args, String> {
        match value {
            Value::Object(map) => Ok(Args { map }),
            _ => Err("arguments must be a JSON object".into()),
        }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.map.remove(key)
    }

    fn opt_str(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => as_string(key, value).map(Some),
        }
    }

    fn req_str(&mut self, key: &str) -> Result<String, String> {
        self.opt_str(key)?
            .ok_or_else(|| format!("argument `{key}` is required"))
    }

    fn opt_i64(&mut self, key: &str) -> Result<Option<i64>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => as_i64(key, value).map(Some),
        }
    }

    fn req_i64(&mut self, key: &str) -> Result<i64, String> {
        self.opt_i64(key)?
            .ok_or_else(|| format!("argument `{key}` is required"))
    }

    fn opt_bool(&mut self, key: &str) -> Result<Option<bool>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(b)),
            Some(_) => Err(expected(key, "true or false")),
        }
    }

    /// Absent stays `None`, `null` is `Some(None)`: PATCH semantics.
    fn patch<T>(
        &mut self,
        key: &str,
        convert: impl FnOnce(&str, Value) -> Result<T, String>,
    ) -> Result<Option<Option<T>>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(value) => convert(key, value).map(|v| Some(Some(v))),
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.map.is_empty() {
            return Ok(());
        }
        let mut names: Vec<&str> = self.map.keys().map(String::as_str).collect();
        names.sort_unstable();
        Err(format!(
            "unknown argument(s): {}; see the tool's inputSchema",
            names.join(", ")
        ))
    }
}

fn parse_identifier(text: &str) -> Result<(String, i64), String> {
    text.trim()
        .rsplit_once('-')
        .and_then(|(key, number)| Some((key.to_string(), number.parse::<i64>().ok()?)))
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| format!("{text:?} is not an issue identifier like TRK-12"))
}

struct IssueRef {
    key: String,
    number: i64,
}

fn take_issue(args: &mut Args) -> Result<IssueRef, String> {
    let issue = args.opt_str("issue")?;
    let project = args.opt_str("project")?;
    let number = args.opt_i64("number")?;
    match issue {
        Some(text) => {
            let (key, parsed) = parse_identifier(&text)?;
            if project.as_ref().is_some_and(|p| *p != key) || number.is_some_and(|n| n != parsed) {
                return Err(format!(
                    "issue {text} contradicts project/number; pass only issue"
                ));
            }
            Ok(IssueRef {
                key,
                number: parsed,
            })
        }
        None => match (project, number) {
            (Some(key), Some(number)) => Ok(IssueRef { key, number }),
            _ => Err("name the issue with `issue` (KEY-n) or with `project` and `number`".into()),
        },
    }
}

/// The project of tools that address a comment or a link: `project`, or the
/// project part of `issue`.
fn take_project_or_issue(args: &mut Args) -> Result<String, String> {
    let project = args.opt_str("project")?;
    let issue = args.opt_str("issue")?;
    match (project, issue) {
        (Some(key), _) => Ok(key),
        (None, Some(text)) => parse_identifier(&text).map(|(key, _)| key),
        (None, None) => Err("pass `project` (or `issue`)".into()),
    }
}

fn issue_number_in(key: &str, value: Value) -> Result<i64, String> {
    let target: LinkTarget = serde_json::from_value(value)
        .map_err(|_| "expected an issue number or KEY-n".to_string())?;
    target
        .number_in(key)
        .map_err(|err| err.message().to_string())
}

fn take_issue_number(args: &mut Args, name: &str, key: &str) -> Result<Option<i64>, String> {
    args.take(name)
        .filter(|v| !v.is_null())
        .map(|v| issue_number_in(key, v).map_err(|e| format!("argument `{name}`: {e}")))
        .transpose()
}

enum ColumnSel {
    Id(i64),
    Name(String),
    State(StateType),
}

fn parse_state(text: &str) -> Result<StateType, String> {
    StateType::parse(text).ok_or_else(|| {
        format!("unknown state_type {text}; use backlog, unstarted, started, completed or canceled")
    })
}

fn take_column(args: &mut Args) -> Result<Option<ColumnSel>, String> {
    let id = args.opt_i64("column_id")?;
    let name = args.opt_str("column")?;
    let state = args.opt_str("state_type")?;
    match (id, name, state) {
        (None, None, None) => Ok(None),
        (Some(id), None, None) => Ok(Some(ColumnSel::Id(id))),
        (None, Some(name), None) => Ok(Some(ColumnSel::Name(name))),
        (None, None, Some(state)) => Ok(Some(ColumnSel::State(parse_state(&state)?))),
        _ => Err("give only one of column_id, column and state_type".into()),
    }
}

enum Named {
    Id(i64),
    Name(String),
}

fn as_named(key: &str, value: Value) -> Result<Named, String> {
    match value {
        Value::String(text) => Ok(text.trim().parse().map_or(Named::Name(text), Named::Id)),
        Value::Number(n) => n
            .as_i64()
            .map(Named::Id)
            .ok_or_else(|| expected(key, "ids or names")),
        _ => Err(expected(key, "ids or names")),
    }
}

fn take_labels(args: &mut Args) -> Result<Option<Vec<Named>>, String> {
    match args.take("labels") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| as_named("labels", item))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(expected("labels", "an array of label ids or names")),
    }
}

// ---- resolving names inside the caller's transaction ----

fn rejected(reason: String) -> StoreError {
    StoreError::from(Rejection::Invalid(reason))
}

fn resolve_column(conn: &Connection, key: &str, sel: ColumnSel) -> Result<i64, StoreError> {
    let columns = projects::list_columns(conn, key)?;
    let listing = || {
        columns
            .iter()
            .map(|c| format!("{} (id {}, {})", c.name, c.id, c.state_type.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let found = match &sel {
        ColumnSel::Id(id) => columns.iter().find(|c| c.id == *id),
        ColumnSel::Name(name) => columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)),
        ColumnSel::State(state) => columns.iter().find(|c| c.state_type == *state),
    };
    found.map(|c| c.id).ok_or_else(|| {
        rejected(format!(
            "no such column in {key}; its columns are: {}",
            listing()
        ))
    })
}

fn resolve_type(conn: &Connection, key: &str, name: &str) -> Result<i64, StoreError> {
    let types = projects::list_issue_types(conn, key)?;
    types
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .map(|t| t.id)
        .ok_or_else(|| {
            let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
            rejected(format!(
                "{key} has no issue type {name:?}; its types are: {}",
                names.join(", ")
            ))
        })
}

fn resolve_labels(
    conn: &Connection,
    key: &str,
    wanted: Vec<Named>,
) -> Result<Vec<i64>, StoreError> {
    let labels = projects::list_labels(conn, key)?;
    wanted
        .into_iter()
        .map(|item| match item {
            Named::Id(id) => Ok(id),
            Named::Name(name) => labels
                .iter()
                .find(|l| l.name.eq_ignore_ascii_case(&name))
                .map(|l| l.id)
                .ok_or_else(|| {
                    let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
                    rejected(format!(
                        "{key} has no label {name:?}; its labels are: {}",
                        names.join(", ")
                    ))
                }),
        })
        .collect()
}

fn resolve_user(conn: &Connection, name: &str) -> Result<i64, StoreError> {
    auth::list_users(conn)?
        .into_iter()
        .find(|u| u.name.eq_ignore_ascii_case(name) && !u.disabled)
        .map(|u| u.id)
        .ok_or_else(|| rejected(format!("no user named {name:?}")))
}

// ---- results ----

fn describe(err: StoreError) -> String {
    match err {
        StoreError::Rejected(Rejection::NotFound(what)) => {
            format!("{what} not found; check the project key, number or id")
        }
        StoreError::Rejected(Rejection::Conflict(reason)) => format!("conflict: {reason}"),
        StoreError::Rejected(Rejection::Invalid(reason)) => format!("invalid: {reason}"),
        StoreError::Rejected(Rejection::Forbidden(reason)) => format!("not allowed: {reason}"),
        StoreError::Rejected(Rejection::Unauthorized(reason)) => reason,
        other => {
            tracing::error!(error = %other, "store operation failed in an MCP tool");
            "internal error; nothing was reported back, try again".to_string()
        }
    }
}

fn js<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn done(summary: impl Into<String>, data: Value) -> Outcome {
    let data = if data.is_object() {
        data
    } else {
        json!({ "result": data })
    };
    Ok(Done {
        summary: summary.into(),
        data,
    })
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn render(outcome: Outcome) -> Value {
    match outcome {
        Ok(Done { summary, data }) => json!({
            "content": [text_block(&summary), text_block(&data.to_string())],
            "structuredContent": data,
            "isError": false,
        }),
        Err(message) => error_result(&message),
    }
}

fn error_result(message: &str) -> Value {
    json!({ "content": [text_block(message)], "isError": true })
}

fn truncated(text: &str, max: usize) -> Option<String> {
    (text.chars().count() > max).then(|| text.chars().take(max).collect())
}

/// Shortens `field` of a JSON object and marks it with `<field>_truncated`.
fn shorten(value: &mut Value, field: &str, max: usize) {
    if let Some(map) = value.as_object_mut() {
        let cut = map
            .get(field)
            .and_then(Value::as_str)
            .and_then(|text| truncated(text, max));
        if let Some(cut) = cut {
            map.insert(field.to_string(), Value::String(cut));
            map.insert(format!("{field}_truncated"), Value::Bool(true));
        }
    }
}

fn plain(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}

fn page_args(args: &mut Args) -> Result<(Option<i64>, Option<i64>), String> {
    Ok((args.opt_i64("limit")?, args.opt_i64("offset")?))
}

// ---- dispatch ----

/// Every project a restricted agent addresses must be on its list. Tools name
/// a project only through `project` and `issue` (`KEY-n`); the arguments that
/// point at other issues (`parent`, `to`, ...) must name the same project, which
/// the tools check themselves.
fn guard(access: &Access, arguments: &Value) -> Result<(), String> {
    if access.is_open() {
        return Ok(());
    }
    let named = |field: &str| arguments.get(field).and_then(Value::as_str);
    let from_issue = named("issue").and_then(|text| text.trim().rsplit_once('-').map(|p| p.0));
    for key in named("project").into_iter().chain(from_issue) {
        access
            .check_project(key)
            .map_err(|err| err.message().to_string())?;
    }
    Ok(())
}

/// Runs a tool. `None` means there is no such tool (a protocol error).
pub async fn call(
    store: &Store,
    principal: &Principal,
    access: &Access,
    name: &str,
    arguments: Value,
) -> Option<Value> {
    let spec = specs().iter().find(|s| s.name == name)?;
    if !spec.read_only && !principal.scope.allows_writes() {
        return Some(error_result(&format!(
            "this key is read-only, so {name} is not available; use a key with write scope"
        )));
    }
    if let Err(message) = guard(access, &arguments) {
        return Some(error_result(&message));
    }
    let outcome = match Args::new(arguments) {
        Ok(args) => run(store, *principal, access, name, args).await,
        Err(message) => Err(message),
    };
    Some(render(outcome))
}

async fn run(store: &Store, p: Principal, access: &Access, name: &str, args: Args) -> Outcome {
    match name {
        "list_projects" => list_projects(store, access, args).await,
        "list_agents" => list_agents(store, p, access, args).await,
        "get_project" => get_project(store, args).await,
        "list_issues" => list_issues(store, args).await,
        "get_issue" => get_issue(store, args).await,
        "create_issue" => create_issue(store, p, args).await,
        "update_issue" => update_issue(store, p, args).await,
        "move_issue" => move_issue(store, p, args).await,
        "archive_issue" => set_archived(store, p, args, true).await,
        "unarchive_issue" => set_archived(store, p, args, false).await,
        "add_comment" => add_comment(store, p, args).await,
        "list_comments" => list_comments(store, args).await,
        "edit_comment" => edit_comment(store, p, args).await,
        "delete_comment" => delete_comment(store, p, args).await,
        "link_issues" => link_issues(store, p, args).await,
        "unlink_issues" => unlink_issues(store, p, args).await,
        "search" => search_tool(store, access, args).await,
        "list_documents" => list_documents(store, args).await,
        "get_document" => get_document(store, args).await,
        "create_document" => create_document(store, p, args).await,
        "update_document" => update_document(store, p, args).await,
        "list_initiatives" => list_initiatives(store, args).await,
        "get_current_user" => get_current_user(store, p, args).await,
        _ => Err(format!("tool {name} is not implemented")),
    }
}

// ---- projects ----

async fn list_agents(store: &Store, p: Principal, access: &Access, args: Args) -> Outcome {
    args.finish()?;
    let mut list = store
        .read(move |c| agents::list_agents(c, &p))
        .await
        .map_err(describe)?;
    for agent in &mut list {
        // Even the owner's key gets no command line over MCP: model output
        // would carry it into a transcript.
        agent.private = None;
        if let Some(allowed) = access.allowed() {
            agent.projects.retain(|key| allowed.contains(key));
        }
    }
    let names: Vec<String> = list
        .iter()
        .map(|a| format!("{}{}", a.name, if a.enabled { "" } else { " (disabled)" }))
        .collect();
    done(
        format!("{} agent(s): {}", list.len(), names.join(", ")),
        json!({ "agents": list.iter().map(js).collect::<Vec<_>>() }),
    )
}

async fn list_projects(store: &Store, access: &Access, args: Args) -> Outcome {
    args.finish()?;
    let mut list = store
        .read(projects::list_projects)
        .await
        .map_err(describe)?;
    list.retain(|project| access.allows(&project.key));
    let names: Vec<String> = list
        .iter()
        .map(|p| format!("{} ({})", p.key, p.name))
        .collect();
    done(
        format!("{} project(s): {}", list.len(), names.join(", ")),
        json!({ "projects": list.iter().map(js).collect::<Vec<_>>() }),
    )
}

fn compact_initiative(view: &initiatives::InitiativeView) -> Value {
    let mut value = js(view);
    shorten(&mut value, "description", LIST_DESCRIPTION_CHARS);
    value
}

async fn get_project(store: &Store, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    args.finish()?;
    let found = store
        .read(move |c| {
            let project = projects::get_project(c, &key)?;
            Ok((
                project,
                projects::list_columns(c, &key)?,
                projects::list_issue_types(c, &key)?,
                projects::list_labels(c, &key)?,
                initiatives::list_initiatives(c, now_millis(), &key, None)?,
            ))
        })
        .await
        .map_err(describe)?;
    let (project, columns, types, labels, initiatives) = found;
    done(
        format!(
            "{} ({}): {} column(s), {} issue type(s), {} label(s), {} initiative(s)",
            project.key,
            project.name,
            columns.len(),
            types.len(),
            labels.len(),
            initiatives.len()
        ),
        json!({
            "project": project,
            "columns": columns,
            "issue_types": types,
            "labels": labels,
            "initiatives": initiatives.iter().map(compact_initiative).collect::<Vec<_>>(),
        }),
    )
}

async fn list_initiatives(store: &Store, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let status = args
        .opt_str("status")?
        .map(|text| {
            InitiativeStatus::parse(&text).ok_or_else(|| {
                format!("unknown status {text}; use planned, active, paused, completed or canceled")
            })
        })
        .transpose()?;
    args.finish()?;
    let list = store
        .read(move |c| initiatives::list_initiatives(c, now_millis(), &key, status))
        .await
        .map_err(describe)?;
    done(
        format!("{} initiative(s)", list.len()),
        json!({ "initiatives": list.iter().map(compact_initiative).collect::<Vec<_>>() }),
    )
}

async fn get_current_user(store: &Store, p: Principal, args: Args) -> Outcome {
    args.finish()?;
    let user = store
        .read(move |c| auth::get_user(c, p.user_id))
        .await
        .map_err(describe)?;
    done(
        format!(
            "{} (user {}, {}), key scope {}",
            user.name,
            user.id,
            user.role.as_str(),
            p.scope.as_str()
        ),
        json!({ "user": user, "scope": p.scope.as_str(), "role": p.role.as_str() }),
    )
}

// ---- issues ----

async fn list_issues(store: &Store, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let column_id = args.opt_i64("column_id")?;
    let column = args.opt_str("column")?;
    let state = args
        .opt_str("state_type")?
        .map(|t| parse_state(&t))
        .transpose()?;
    let label_id = args.opt_i64("label_id")?;
    let label = args.opt_str("label")?;
    let assignee_id = args.opt_i64("assignee_id")?;
    let parent = match args.take("parent") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text == "none" => Some(ParentFilter::None),
        Some(value) => Some(ParentFilter::Number(
            issue_number_in(&key, value)
                .map_err(|e| format!("argument `parent`: {e} (or the text none)"))?,
        )),
    };
    let include_archived = args.opt_bool("include_archived")?.unwrap_or(false);
    let archived_only = args.opt_bool("archived_only")?.unwrap_or(false);
    let (limit, offset) = page_args(&mut args)?;
    args.finish()?;
    if column_id.is_some() && column.is_some() {
        return Err("give only one of column_id and column".into());
    }
    if label_id.is_some() && label.is_some() {
        return Err("give only one of label_id and label".into());
    }
    let limit = limit.unwrap_or(DEFAULT_ISSUE_PAGE).clamp(1, MAX_ISSUE_PAGE);
    let found = store
        .read(move |c| {
            let column_id = match (column_id, column) {
                (Some(id), _) => Some(id),
                (None, Some(name)) => Some(resolve_column(c, &key, ColumnSel::Name(name))?),
                (None, None) => None,
            };
            let label_id = match (label_id, label) {
                (Some(id), _) => Some(id),
                (None, Some(name)) => resolve_labels(c, &key, vec![Named::Name(name)])?.pop(),
                (None, None) => None,
            };
            let filter = IssueFilter {
                column_id,
                state_type: state,
                label_id,
                assignee_id,
                parent,
                include_archived,
                archived_only,
                limit: Some(limit),
                offset,
            };
            let list = issues::list_issues(c, &key, &filter)?;
            Ok((list, projects::list_columns(c, &key)?))
        })
        .await
        .map_err(describe)?;
    let (list, columns) = found;
    let lines: Vec<String> = list
        .issues
        .iter()
        .map(|i| {
            let column = columns.iter().find(|c| c.id == i.column_id);
            format!(
                "{} [{}] {}",
                i.identifier,
                column.map_or("?", |c| c.name.as_str()),
                i.title
            )
        })
        .collect();
    let next = list.offset + i64::try_from(list.issues.len()).unwrap_or(0);
    let mut data = json!({
        "issues": list.issues.iter().map(|i| {
            let mut v = js(i);
            shorten(&mut v, "description", LIST_DESCRIPTION_CHARS);
            v
        }).collect::<Vec<_>>(),
        "total": list.total,
        "limit": list.limit,
        "offset": list.offset,
    });
    if next < list.total {
        data["next_offset"] = json!(next);
    }
    done(
        format!(
            "{} of {} issue(s) from offset {}:\n{}",
            list.issues.len(),
            list.total,
            list.offset,
            lines.join("\n")
        ),
        data,
    )
}

async fn get_issue(store: &Store, mut args: Args) -> Outcome {
    let target = take_issue(&mut args)?;
    args.finish()?;
    let (key, number) = (target.key, target.number);
    let found = store
        .read(move |c| {
            let detail = issues::get_issue(c, &key, number)?;
            let comments = comments::list_comments(c, &key, number)?;
            let count: usize = comments.iter().map(|top| 1 + top.replies.len()).sum();
            let columns = projects::list_columns(c, &key)?;
            let labels = projects::list_labels(c, &key)?;
            Ok((detail, count, columns, labels))
        })
        .await
        .map_err(describe)?;
    let (detail, count, columns, labels) = found;
    let column = columns.iter().find(|c| c.id == detail.issue.column_id);
    let label_names: Vec<&str> = labels
        .iter()
        .filter(|l| detail.issue.labels.contains(&l.id))
        .map(|l| l.name.as_str())
        .collect();
    let summary = format!(
        "{} {:?} in {} ({}){}; {} comment(s), {} child(ren)",
        detail.issue.identifier,
        detail.issue.title,
        column.map_or("?", |c| c.name.as_str()),
        column.map_or("?", |c| c.state_type.as_str()),
        if detail.issue.blocked {
            ", blocked"
        } else {
            ""
        },
        count,
        detail.children.len()
    );
    let mut data = js(&detail);
    data["comments_count"] = json!(count);
    data["label_names"] = json!(label_names);
    data["column"] = column.map_or(Value::Null, js);
    done(summary, data)
}

fn issue_done(verb: &str, detail: &issues::IssueDetail) -> Outcome {
    done(
        format!(
            "{verb} {} {:?}",
            detail.issue.identifier, detail.issue.title
        ),
        js(detail),
    )
}

fn parse_priority(key: &str, value: Value) -> Result<aurora_store::issues::Priority, String> {
    let text = as_string(key, value)?;
    issues::Priority::parse(&text)
        .ok_or_else(|| format!("unknown priority {text}; use urgent, high, medium or low"))
}

async fn create_issue(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let title = args.req_str("title")?;
    let description = args.opt_str("description")?;
    let type_id = args.opt_i64("type_id")?;
    let type_name = args.opt_str("type")?;
    let column = take_column(&mut args)?;
    let assignee_id = args.opt_i64("assignee_id")?;
    let assignee = args.opt_str("assignee")?;
    let parent = take_issue_number(&mut args, "parent", &key)?;
    let initiative = args.opt_i64("initiative")?;
    let priority = args.patch("priority", parse_priority)?.flatten();
    let labels = take_labels(&mut args)?;
    args.finish()?;
    if type_id.is_some() && type_name.is_some() {
        return Err("give only one of type_id and type".into());
    }
    if assignee_id.is_some() && assignee.is_some() {
        return Err("give only one of assignee_id and assignee".into());
    }
    let detail = store
        .write(move |tx| {
            let type_id = match type_name {
                Some(name) => Some(resolve_type(tx, &key, &name)?),
                None => type_id,
            };
            let column_id = column
                .map(|sel| resolve_column(tx, &key, sel))
                .transpose()?;
            let assignee_id = match assignee {
                Some(name) => Some(resolve_user(tx, &name)?),
                None => assignee_id,
            };
            let labels = labels
                .map(|l| resolve_labels(tx, &key, l))
                .transpose()?
                .unwrap_or_default();
            let new = NewIssue {
                title,
                description,
                type_id,
                column_id,
                assignee_id,
                parent,
                initiative,
                priority,
                labels,
            };
            issues::create_issue(tx, now_millis(), &p, &key, new)
        })
        .await
        .map_err(describe)?;
    issue_done("Created", &detail)
}

async fn update_issue(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let target = take_issue(&mut args)?;
    let (key, number) = (target.key, target.number);
    let title = args.opt_str("title")?;
    let description = args.patch("description", as_string)?;
    let type_id = args.opt_i64("type_id")?;
    let type_name = args.opt_str("type")?;
    let assignee_id = args.patch("assignee_id", as_i64)?;
    let assignee = args.patch("assignee", as_string)?;
    let parent_key = key.clone();
    let parent = args.patch("parent", |_, v| issue_number_in(&parent_key, v))?;
    let initiative = args.patch("initiative", as_i64)?;
    let priority = args.patch("priority", parse_priority)?;
    let labels = take_labels(&mut args)?;
    args.finish()?;
    if type_id.is_some() && type_name.is_some() {
        return Err("give only one of type_id and type".into());
    }
    if assignee_id.is_some() && assignee.is_some() {
        return Err("give only one of assignee_id and assignee".into());
    }
    let nothing = title.is_none()
        && description.is_none()
        && type_id.is_none()
        && type_name.is_none()
        && assignee_id.is_none()
        && assignee.is_none()
        && parent.is_none()
        && initiative.is_none()
        && priority.is_none()
        && labels.is_none();
    if nothing {
        return Err(
            "nothing to change: pass at least one of title, description, type, assignee, parent, initiative, priority, labels"
                .into(),
        );
    }
    let detail = store
        .write(move |tx| {
            let type_id = match type_name {
                Some(name) => Some(resolve_type(tx, &key, &name)?),
                None => type_id,
            };
            let assignee_id = match assignee {
                Some(Some(name)) => Some(Some(resolve_user(tx, &name)?)),
                Some(None) => Some(None),
                None => assignee_id,
            };
            let labels = labels.map(|l| resolve_labels(tx, &key, l)).transpose()?;
            let patch = IssuePatch {
                title,
                description,
                type_id,
                assignee_id,
                parent,
                initiative,
                priority,
                labels,
            };
            issues::update_issue(tx, now_millis(), &p, &key, number, patch)
        })
        .await
        .map_err(describe)?;
    issue_done("Updated", &detail)
}

async fn move_issue(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let target = take_issue(&mut args)?;
    let (key, number) = (target.key, target.number);
    let column =
        take_column(&mut args)?.ok_or("name the target with column_id, column or state_type")?;
    let before = take_issue_number(&mut args, "before", &key)?;
    let after = take_issue_number(&mut args, "after", &key)?;
    args.finish()?;
    if before.is_some() && after.is_some() {
        return Err("give only one of before and after".into());
    }
    let detail = store
        .write(move |tx| {
            let column_id = resolve_column(tx, &key, column)?;
            let target = MoveTarget {
                column_id,
                before,
                after,
                position: None,
            };
            issues::move_issue(tx, now_millis(), &p, &key, number, target)
        })
        .await
        .map_err(describe)?;
    issue_done("Moved", &detail)
}

async fn set_archived(store: &Store, p: Principal, mut args: Args, archive: bool) -> Outcome {
    let target = take_issue(&mut args)?;
    args.finish()?;
    let (key, number) = (target.key, target.number);
    let detail = store
        .write(move |tx| {
            if archive {
                issues::archive_issue(tx, now_millis(), &p, &key, number)
            } else {
                issues::unarchive_issue(tx, now_millis(), &p, &key, number)
            }
        })
        .await
        .map_err(describe)?;
    issue_done(if archive { "Archived" } else { "Unarchived" }, &detail)
}

// ---- comments ----

async fn add_comment(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let target = take_issue(&mut args)?;
    let body = args.req_str("body")?;
    let parent = args.opt_i64("parent")?;
    args.finish()?;
    let (key, number) = (target.key, target.number);
    let comment = store
        .write(move |tx| {
            comments::add_comment(
                tx,
                now_millis(),
                &p,
                &key,
                number,
                NewComment { body, parent },
            )
        })
        .await
        .map_err(describe)?;
    let mentions: Vec<&str> = comment.mentions.iter().map(|m| m.name.as_str()).collect();
    done(
        format!(
            "Added comment {} on {}{}",
            comment.id,
            comment.issue.identifier,
            if mentions.is_empty() {
                String::new()
            } else {
                format!(", mentioning {}", mentions.join(", "))
            }
        ),
        js(&comment),
    )
}

fn shrink_comment(value: &mut Value) {
    shorten(value, "body", LIST_COMMENT_CHARS);
    if let Some(replies) = value.get_mut("replies").and_then(Value::as_array_mut) {
        for reply in replies {
            shrink_comment(reply);
        }
    }
}

async fn list_comments(store: &Store, mut args: Args) -> Outcome {
    let target = take_issue(&mut args)?;
    let (limit, offset) = page_args(&mut args)?;
    args.finish()?;
    let limit = usize::try_from(limit.unwrap_or(DEFAULT_COMMENT_PAGE as i64))
        .unwrap_or(DEFAULT_COMMENT_PAGE)
        .clamp(1, MAX_COMMENT_PAGE);
    let offset = usize::try_from(offset.unwrap_or(0)).map_err(|_| "offset must not be negative")?;
    let (key, number) = (target.key, target.number);
    let all = store
        .read(move |c| comments::list_comments(c, &key, number))
        .await
        .map_err(describe)?;
    let total = all.len();
    let page: Vec<Value> = all
        .iter()
        .skip(offset)
        .take(limit)
        .map(|c| {
            let mut v = js(c);
            shrink_comment(&mut v);
            v
        })
        .collect();
    let mut data = json!({ "comments": page, "total": total, "limit": limit, "offset": offset });
    if offset + page.len() < total {
        data["next_offset"] = json!(offset + page.len());
    }
    done(
        format!(
            "{} of {total} top-level comment(s) from offset {offset}",
            page.len()
        ),
        data,
    )
}

async fn edit_comment(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let key = take_project_or_issue(&mut args)?;
    let id = args.req_i64("comment_id")?;
    let body = args.req_str("body")?;
    args.finish()?;
    let comment = store
        .write(move |tx| comments::edit_comment(tx, now_millis(), &p, &key, id, &body))
        .await
        .map_err(describe)?;
    done(
        format!(
            "Edited comment {} on {}",
            comment.id, comment.issue.identifier
        ),
        js(&comment),
    )
}

async fn delete_comment(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let key = take_project_or_issue(&mut args)?;
    let id = args.req_i64("comment_id")?;
    args.finish()?;
    store
        .write(move |tx| comments::delete_comment(tx, now_millis(), &p, &key, id))
        .await
        .map_err(describe)?;
    done(format!("Deleted comment {id}"), json!({ "deleted": id }))
}

// ---- links ----

async fn link_issues(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let target = take_issue(&mut args)?;
    let (key, number) = (target.key, target.number);
    let to = args.take("to").ok_or("argument `to` is required")?;
    let to = issue_number_in(&key, to).map_err(|e| format!("argument `to`: {e}"))?;
    let kind = args.req_str("kind")?;
    args.finish()?;
    let (kind, blocked_by) = match kind.as_str() {
        "blocks" => (LinkKind::Blocks, false),
        "blocked_by" => (LinkKind::Blocks, true),
        "relates" => (LinkKind::Relates, false),
        other => {
            return Err(format!(
                "unknown kind {other}; use blocks, blocked_by or relates"
            ));
        }
    };
    let link = store
        .write(move |tx| {
            let now = now_millis();
            if blocked_by {
                links::add_blocker(tx, now, &p, &key, number, to)
            } else {
                links::add_link(tx, now, &p, &key, number, to, kind)
            }
        })
        .await
        .map_err(describe)?;
    done(
        format!(
            "Linked: {} {} {} (link {})",
            link.from.identifier,
            link.kind.as_str(),
            link.to.identifier,
            link.id
        ),
        json!({ "link": link }),
    )
}

async fn unlink_issues(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let key = take_project_or_issue(&mut args)?;
    let id = args.req_i64("link_id")?;
    args.finish()?;
    store
        .write(move |tx| links::remove_link(tx, now_millis(), &p, &key, id))
        .await
        .map_err(describe)?;
    done(format!("Removed link {id}"), json!({ "removed": id }))
}

// ---- search ----

async fn search_tool(store: &Store, access: &Access, mut args: Args) -> Outcome {
    let q = args.req_str("q")?;
    let project = args.opt_str("project")?;
    let kinds = match args.take("kinds") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut kinds = Vec::new();
            for item in items {
                let text = as_string("kinds", item)?;
                kinds.push(Kind::parse(&text).ok_or_else(|| {
                    format!("unknown kind {text:?}; use issue, comment or document")
                })?);
            }
            Some(kinds)
        }
        Some(_) => return Err(expected("kinds", "an array of kinds")),
    };
    let include_archived = args.opt_bool("include_archived")?.unwrap_or(false);
    let (limit, offset) = page_args(&mut args)?;
    args.finish()?;
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_PAGE)
        .clamp(1, MAX_SEARCH_PAGE);
    let query = SearchQuery {
        q: q.clone(),
        project,
        kinds,
        include_archived,
        limit: Some(limit),
        offset,
    };
    let allowed: Option<Vec<String>> = access.allowed().map(<[String]>::to_vec);
    let results = store
        .read(move |c| search::search_in(c, &query, allowed.as_deref()))
        .await
        .map_err(describe)?;
    let hits: Vec<Value> = results
        .hits
        .iter()
        .map(|h| {
            json!({
                "kind": h.kind.as_str(),
                "project": h.project,
                "issue": h.issue,
                "comment_id": h.comment_id,
                "document": h.document,
                "title": plain(&h.title),
                "snippet": plain(&h.snippet),
                "archived": h.archived,
                "exact": h.exact,
            })
        })
        .collect();
    let lines: Vec<String> = results
        .hits
        .iter()
        .map(|h| {
            let what = match (&h.issue, &h.document) {
                (Some(i), _) => format!("{} {}", i.identifier, i.title),
                (None, Some(d)) => format!("document {} {}", d.id, d.title),
                _ => "?".to_string(),
            };
            format!("{}: {what}", h.kind.as_str())
        })
        .collect();
    let offset = offset.unwrap_or(0);
    done(
        format!(
            "{} of {}{} hit(s) for {q:?}:\n{}",
            hits.len(),
            results.total,
            if results.total_capped { "+" } else { "" },
            lines.join("\n")
        ),
        json!({
            "query": q,
            "total": results.total,
            "total_capped": results.total_capped,
            "limit": limit,
            "offset": offset,
            "hits": hits,
        }),
    )
}

// ---- documents ----

async fn list_documents(store: &Store, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let include_archived = args.opt_bool("include_archived")?.unwrap_or(false);
    let (limit, offset) = page_args(&mut args)?;
    args.finish()?;
    let limit = usize::try_from(limit.unwrap_or(DEFAULT_DOCUMENT_PAGE as i64))
        .unwrap_or(DEFAULT_DOCUMENT_PAGE)
        .clamp(1, MAX_DOCUMENT_PAGE);
    let offset = usize::try_from(offset.unwrap_or(0)).map_err(|_| "offset must not be negative")?;
    let list = store
        .read(move |c| documents::list_documents(c, &key, include_archived))
        .await
        .map_err(describe)?;
    let tree = documents::in_tree_order(list);
    let total = tree.len();
    let page: Vec<Value> = tree
        .iter()
        .skip(offset)
        .take(limit)
        .map(|(depth, d)| {
            let mut v = js(d);
            v["depth"] = json!(depth);
            v
        })
        .collect();
    let mut data = json!({ "documents": page, "total": total, "limit": limit, "offset": offset });
    if offset + page.len() < total {
        data["next_offset"] = json!(offset + page.len());
    }
    done(format!("{} of {total} document(s)", page.len()), data)
}

async fn get_document(store: &Store, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let id = args.req_i64("id")?;
    let max = args.opt_i64("max_body_chars")?;
    args.finish()?;
    let max = usize::try_from(max.unwrap_or(DEFAULT_BODY_CHARS as i64))
        .unwrap_or(DEFAULT_BODY_CHARS)
        .clamp(1, documents::MAX_BODY_CHARS);
    let detail = store
        .read(move |c| documents::get_document(c, &key, id))
        .await
        .map_err(describe)?;
    let mut data = js(&detail);
    data["body_chars"] = json!(detail.document.body.chars().count());
    shorten(&mut data, "body", max);
    done(
        format!(
            "Document {} {:?}, revision {}",
            detail.document.id, detail.document.title, detail.document.revision
        ),
        data,
    )
}

async fn create_document(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let title = args.req_str("title")?;
    let body = args.opt_str("body")?;
    let parent = args.opt_i64("parent")?;
    args.finish()?;
    let detail = store
        .write(move |tx| {
            documents::create_document(
                tx,
                now_millis(),
                &p,
                &key,
                NewDocument {
                    title,
                    body,
                    parent,
                },
            )
        })
        .await
        .map_err(describe)?;
    done(
        format!(
            "Created document {} {:?}",
            detail.document.id, detail.document.title
        ),
        js(&detail),
    )
}

async fn update_document(store: &Store, p: Principal, mut args: Args) -> Outcome {
    let key = args.req_str("project")?;
    let id = args.req_i64("id")?;
    let title = args.opt_str("title")?;
    let body = args.opt_str("body")?;
    let parent = args.patch("parent", as_i64)?;
    let base_revision = args.opt_i64("base_revision")?;
    args.finish()?;
    if title.is_none() && body.is_none() && parent.is_none() {
        return Err("nothing to change: pass title, body or parent".into());
    }
    let detail = store
        .write(move |tx| {
            documents::update_document(
                tx,
                now_millis(),
                &p,
                &key,
                id,
                DocumentPatch {
                    title,
                    body,
                    parent,
                    base_revision,
                },
            )
        })
        .await
        .map_err(describe)?;
    let mut data = js(&detail);
    shorten(&mut data, "body", DEFAULT_BODY_CHARS);
    done(
        format!(
            "Updated document {} {:?}, now revision {}",
            detail.document.id, detail.document.title, detail.document.revision
        ),
        data,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn unique(names: &[&str]) -> bool {
        names.iter().collect::<HashSet<_>>().len() == names.len()
    }

    #[test]
    fn tool_names_are_unique_and_schemas_are_objects() {
        let names: Vec<&str> = specs().iter().map(|s| s.name).collect();
        assert!(unique(&names), "{names:?}");
        for spec in specs() {
            assert_eq!(spec.schema["type"], "object", "{}", spec.name);
            assert!(spec.schema["properties"].is_object(), "{}", spec.name);
            let required = spec.schema["required"].as_array().unwrap();
            for name in required {
                assert!(
                    spec.schema["properties"]
                        .get(name.as_str().unwrap())
                        .is_some(),
                    "{}: required {name} is not a property",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn identifiers_parse() {
        assert_eq!(parse_identifier("TRK-12"), Ok(("TRK".into(), 12)));
        assert_eq!(parse_identifier("MY-KEY-3"), Ok(("MY-KEY".into(), 3)));
        assert!(parse_identifier("TRK").is_err());
        assert!(parse_identifier("-3").is_err());
        assert!(parse_identifier("TRK-x").is_err());
    }
}
