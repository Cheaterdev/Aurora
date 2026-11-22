//! The variable catalogue: the one table that validation, the documentation
//! test and the pages' "available variables" list all read. A variable that is
//! not in [`VARIABLES`] cannot be used in a template.
//!
//! Names are flat dotted strings: `trigger` and `trigger.comment` are two
//! separate entries, not a value and its child.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    Text,
    Number,
    Bool,
}

impl Scalar {
    pub fn label(self) -> &'static str {
        match self {
            Scalar::Text => "text",
            Scalar::Number => "number",
            Scalar::Bool => "boolean",
        }
    }
}

/// A field of a list item, used as `{{this.name}}` inside `{{#each}}`.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub scalar: Scalar,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum ItemKind {
    /// Each item is a string, used as `{{this}}`.
    Text,
    /// Each item has these fields.
    Record(&'static [Field]),
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Scalar(Scalar),
    List(ItemKind),
}

#[derive(Debug, Clone, Copy)]
pub struct Variable {
    pub name: &'static str,
    pub kind: Kind,
    pub description: &'static str,
    pub example: &'static str,
}

const fn text(name: &'static str, description: &'static str, example: &'static str) -> Variable {
    Variable {
        name,
        kind: Kind::Scalar(Scalar::Text),
        description,
        example,
    }
}

const fn number(name: &'static str, description: &'static str, example: &'static str) -> Variable {
    Variable {
        name,
        kind: Kind::Scalar(Scalar::Number),
        description,
        example,
    }
}

const fn flag(name: &'static str, description: &'static str, example: &'static str) -> Variable {
    Variable {
        name,
        kind: Kind::Scalar(Scalar::Bool),
        description,
        example,
    }
}

const fn list(
    name: &'static str,
    item: ItemKind,
    description: &'static str,
    example: &'static str,
) -> Variable {
    Variable {
        name,
        kind: Kind::List(item),
        description,
        example,
    }
}

const fn field(name: &'static str, scalar: Scalar, description: &'static str) -> Field {
    Field {
        name,
        scalar,
        description,
    }
}

const ISSUE_REF: &[Field] = &[
    field("key", Scalar::Text, "the issue key"),
    field("title", Scalar::Text, "the issue title"),
    field(
        "state_type",
        Scalar::Text,
        "backlog, unstarted, started, completed or canceled",
    ),
];

const COMMENT_FIELDS: &[Field] = &[
    field("author", Scalar::Text, "the author's name"),
    field("kind", Scalar::Text, "human or agent"),
    field("created", Scalar::Text, "ISO-8601 UTC time"),
    field(
        "body",
        Scalar::Text,
        "the comment text, cut at 4000 characters",
    ),
    field(
        "is_trigger",
        Scalar::Bool,
        "true for the comment that started this run",
    ),
];

const DOCUMENT_FIELDS: &[Field] = &[
    field("title", Scalar::Text, "the document title"),
    field("id", Scalar::Number, "the document id"),
];

/// Every variable a template may use, in the order the documentation and the
/// pages list them.
pub const VARIABLES: &[Variable] = &[
    text("issue.key", "The issue key.", "TRK-12"),
    number("issue.number", "The issue number within its project.", "12"),
    text("issue.title", "The issue title.", "Fix the login redirect"),
    text(
        "issue.description",
        "The issue description (may be empty). Inserted as it is, never read as template text.",
        "Users land on a blank page after signing in.",
    ),
    text("issue.type", "The issue type's name.", "Bug"),
    text(
        "issue.column",
        "The name of the issue's column.",
        "In Progress",
    ),
    text(
        "issue.state_type",
        "The state type of the issue's column: backlog, unstarted, started, completed or canceled.",
        "started",
    ),
    text(
        "issue.priority",
        "urgent, high, medium or low; empty when none is set.",
        "high",
    ),
    text(
        "issue.assignee",
        "The assignee's name; empty when unassigned.",
        "claude-code",
    ),
    list(
        "issue.labels",
        ItemKind::Text,
        "The label names, one per item (use {{this}}).",
        "bug, ui",
    ),
    text(
        "issue.parent",
        "The parent issue's key; empty for a top-level issue.",
        "TRK-3",
    ),
    list(
        "issue.children",
        ItemKind::Record(ISSUE_REF),
        "The issue's sub-issues that are not archived.",
        "TRK-13 Add a test (unstarted)",
    ),
    text(
        "issue.url",
        "Where the issue is shown: an absolute URL when the server knows its public URL (--public-url), otherwise the path /p/KEY/N.",
        "/p/TRK/12",
    ),
    text("project.key", "The project key.", "TRK"),
    text("project.name", "The project name.", "Tracker"),
    text(
        "project.rules",
        "The project's rules text (may be empty). Inserted as it is, never read as template text.",
        "Use British spelling.",
    ),
    text(
        "initiative.name",
        "The name of the issue's initiative; empty when it has none.",
        "Launch",
    ),
    list(
        "comments",
        ItemKind::Record(COMMENT_FIELDS),
        "The last 30 comments that are not deleted, oldest first, each body cut at 4000 characters.",
        "ann (human): Please start with the login page.",
    ),
    number(
        "comments.omitted",
        "How many older comments are not in the comments list.",
        "0",
    ),
    list(
        "links.blocks",
        ItemKind::Record(ISSUE_REF),
        "Issues this issue blocks (archived ones left out).",
        "TRK-14 Deploy (unstarted)",
    ),
    list(
        "links.blocked_by",
        ItemKind::Record(ISSUE_REF),
        "Issues that block this issue, finished or not (archived ones left out).",
        "TRK-11 Schema (completed)",
    ),
    list(
        "links.related",
        ItemKind::Record(ISSUE_REF),
        "Related issues (archived ones left out).",
        "TRK-9 Same page, other bug (started)",
    ),
    list(
        "blockers",
        ItemKind::Record(ISSUE_REF),
        "Only the blockers that are still open: not completed, not canceled, not archived.",
        "TRK-11 Schema (started)",
    ),
    list(
        "documents",
        ItemKind::Record(DOCUMENT_FIELDS),
        "The documents linked to the issue that are not archived.",
        "Login design (3)",
    ),
    text(
        "trigger",
        "Why the run started: assigned, mentioned, schedule or manual.",
        "assigned",
    ),
    flag(
        "trigger.assigned",
        "True when the run started because the issue was assigned to the agent.",
        "true",
    ),
    flag(
        "trigger.mentioned",
        "True when the run started because a comment mentioned the agent.",
        "false",
    ),
    flag(
        "trigger.schedule",
        "True when the run started on a schedule.",
        "false",
    ),
    flag(
        "trigger.manual",
        "True when a person started the run by hand.",
        "false",
    ),
    text(
        "trigger.comment",
        "The text of the comment that mentioned the agent (cut at 4000 characters); empty for other triggers.",
        "@claude-code can you take this?",
    ),
    text(
        "trigger.actor",
        "The name of the person or agent who caused the run; empty when unknown.",
        "ann",
    ),
    text("agent.name", "The agent's name.", "claude-code"),
    text(
        "workdir",
        "The path of the working copy of this run; empty until a working copy is provided.",
        "C:\\work\\tracker",
    ),
    text(
        "run.id",
        "The run's id; empty when the message is not for a run (a preview).",
        "57",
    ),
    text(
        "run.branch",
        "The git branch of the working copy of this run; empty when the run has no working copy of its own.",
        "aurora/TRK-12-run57",
    ),
    text(
        "server.url",
        "The MCP endpoint of this server: absolute when the server knows its public URL, otherwise /mcp.",
        "http://localhost:8787/mcp",
    ),
    text(
        "now",
        "The current time, ISO-8601 UTC.",
        "2026-03-01T09:30:00Z",
    ),
];

/// The variable called `name`, if there is one.
pub fn find(name: &str) -> Option<&'static Variable> {
    VARIABLES.iter().find(|variable| variable.name == name)
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldInfo {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub description: &'static str,
}

/// A catalogue entry as the API and the pages show it.
#[derive(Debug, Clone, Serialize)]
pub struct VariableInfo {
    pub name: &'static str,
    /// `text`, `number`, `boolean` or `list`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub description: &'static str,
    pub example: &'static str,
    /// For a list: `text` (use `{{this}}`) or `record` (use `{{this.field}}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<&'static str>,
    /// The fields of a `record` list's items.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldInfo>,
}

pub fn catalogue() -> Vec<VariableInfo> {
    VARIABLES
        .iter()
        .map(|variable| {
            let (kind, item, fields) = match variable.kind {
                Kind::Scalar(scalar) => (scalar.label(), None, Vec::new()),
                Kind::List(ItemKind::Text) => ("list", Some("text"), Vec::new()),
                Kind::List(ItemKind::Record(list)) => (
                    "list",
                    Some("record"),
                    list.iter()
                        .map(|f| FieldInfo {
                            name: f.name,
                            kind: f.scalar.label(),
                            description: f.description,
                        })
                        .collect(),
                ),
            };
            VariableInfo {
                name: variable.name,
                kind,
                description: variable.description,
                example: variable.example,
                item,
                fields,
            }
        })
        .collect()
}
