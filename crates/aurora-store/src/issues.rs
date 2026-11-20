//! Issues: per-project sequence numbers (KEY-n), parents and ordering inside a
//! column.
//!
//! Plain functions over a transaction (writes) or a connection (reads), like
//! [`crate::projects`]. Every change appends its event in the same
//! transaction. Issues are addressed by project key and issue number; ids in a
//! request body (type, column, label, assignee) must belong to that project.
//!
//! Position scheme: `issue.position` is a REAL ordering key inside a column.
//! New issues go to the end (`max + 1024`). A move computes a position between
//! its two neighbours; when the gap falls below [`MIN_GAP`] the whole column is
//! renumbered 1024 apart first, in the same transaction.

use rusqlite::types::{
    FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, Value as SqlValue, ValueRef,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use crate::StoreError;
use crate::auth::Principal;
use crate::documents::{LinkedDocument, documents_of_issue};
use crate::links::{IssueLinks, links_of};
use crate::projects::{Project, StateType, conflict, emit, get_project, invalid, not_found};

pub const MAX_TITLE_CHARS: usize = 300;
pub const MAX_DESCRIPTION_CHARS: usize = 50_000;
/// The longest chain of parents, counting the top-level issue and the deepest child.
pub const MAX_DEPTH: i64 = 8;
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
const POSITION_STEP: f64 = 1024.0;
/// Below this distance between two neighbours a column is renumbered.
const MIN_GAP: f64 = 1e-4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Urgent,
    High,
    Medium,
    Low,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        }
    }

    pub fn parse(text: &str) -> Option<Priority> {
        Some(match text {
            "urgent" => Priority::Urgent,
            "high" => Priority::High,
            "medium" => Priority::Medium,
            "low" => Priority::Low,
            _ => return None,
        })
    }
}

impl ToSql for Priority {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for Priority {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Priority::parse(text).ok_or_else(|| FromSqlError::Other(format!("priority {text}").into()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub id: i64,
    pub project_id: i64,
    pub number: i64,
    /// `KEY-n`
    pub identifier: String,
    pub type_id: i64,
    pub column_id: i64,
    pub title: String,
    pub description: String,
    pub assignee_id: Option<i64>,
    /// The parent's number in the same project.
    pub parent: Option<i64>,
    /// The initiative of the same project this issue counts towards.
    pub initiative_id: Option<i64>,
    pub priority: Option<Priority>,
    pub position: f64,
    pub created: i64,
    pub updated: i64,
    pub archived: bool,
    /// Label ids, ascending.
    pub labels: Vec<i64>,
    /// At least one blocker is neither archived nor in a completed or canceled column.
    pub blocked: bool,
    /// How many such blockers.
    pub blocker_count: i64,
    #[serde(skip)]
    parent_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildRef {
    pub number: i64,
    pub title: String,
    pub column_id: i64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssueDetail {
    #[serde(flatten)]
    pub issue: Issue,
    /// Ordered by column position, then issue position.
    pub children: Vec<ChildRef>,
    pub links: IssueLinks,
    /// The non-archived documents linked to this issue.
    pub documents: Vec<LinkedDocument>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssueList {
    pub issues: Vec<Issue>,
    /// Matches of the filter, ignoring `limit` and `offset`.
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewIssue {
    pub title: String,
    pub description: Option<String>,
    /// Required unless the project has exactly one issue type.
    pub type_id: Option<i64>,
    /// Defaults to the first backlog column, else the first column.
    pub column_id: Option<i64>,
    pub assignee_id: Option<i64>,
    /// Number of the parent issue.
    pub parent: Option<i64>,
    /// Id of an initiative of the same project.
    pub initiative: Option<i64>,
    pub priority: Option<Priority>,
    #[serde(default)]
    pub labels: Vec<i64>,
}

/// Only the fields that are present change; `Some(None)` clears. The column
/// is not here on purpose: issues change column through [`move_issue`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuePatch {
    pub title: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub description: Option<Option<String>>,
    pub type_id: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub assignee_id: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    pub parent: Option<Option<i64>>,
    /// Id of an initiative of the same project; `null` clears.
    #[serde(default, deserialize_with = "present")]
    pub initiative: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    pub priority: Option<Option<Priority>>,
    /// The full new label set.
    pub labels: Option<Vec<i64>>,
}

/// Tells an absent field (`None`) from an explicit `null` (`Some(None)`).
pub(crate) fn present<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Where to put an issue. At most one of `before`, `after`, `position`;
/// none means the end of the column. `before` and `after` are issue numbers
/// in the target column.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveTarget {
    pub column_id: i64,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub position: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentFilter {
    /// Top-level issues only.
    None,
    Number(i64),
}

#[derive(Debug, Clone, Default)]
pub struct IssueFilter {
    pub column_id: Option<i64>,
    pub state_type: Option<StateType>,
    pub label_id: Option<i64>,
    pub assignee_id: Option<i64>,
    pub parent: Option<ParentFilter>,
    /// Archived issues too.
    pub include_archived: bool,
    /// Only archived issues; wins over `include_archived`.
    pub archived_only: bool,
    /// Default 50, at most 200.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

const ISSUE_FROM: &str = "FROM issue i
     JOIN project p ON p.id = i.project_id
     JOIN \"column\" c ON c.id = i.column_id
     LEFT JOIN issue par ON par.id = i.parent_id";

const ISSUE_COLUMNS: &str = concat!(
    "SELECT i.id, i.project_id, i.number, p.key, i.type_id, i.column_id,
            i.title, i.description, i.assignee_id, par.number, i.priority, i.position,
            i.created, i.updated, i.archived, i.parent_id,
            (SELECT COUNT(*) FROM issue_link l
               JOIN issue b ON b.id = l.from_issue_id
               JOIN \"column\" bc ON bc.id = b.column_id
              WHERE l.kind = 'blocks' AND l.to_issue_id = i.id AND ",
    open_blocker!(),
    "), i.initiative_id"
);

const ISSUE_ORDER: &str = "ORDER BY c.position, i.position, i.id";

fn issue_row(row: &Row<'_>) -> rusqlite::Result<Issue> {
    let key: String = row.get(3)?;
    let number: i64 = row.get(2)?;
    let blocker_count: i64 = row.get(16)?;
    Ok(Issue {
        id: row.get(0)?,
        project_id: row.get(1)?,
        number,
        identifier: format!("{key}-{number}"),
        type_id: row.get(4)?,
        column_id: row.get(5)?,
        title: row.get(6)?,
        description: row.get(7)?,
        assignee_id: row.get(8)?,
        parent: row.get(9)?,
        initiative_id: row.get(17)?,
        priority: row.get(10)?,
        position: row.get(11)?,
        created: row.get(12)?,
        updated: row.get(13)?,
        archived: row.get(14)?,
        labels: Vec::new(),
        blocked: blocker_count > 0,
        blocker_count,
        parent_id: row.get(15)?,
    })
}

fn label_ids(conn: &Connection, issue_id: i64) -> Result<Vec<i64>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT label_id FROM issue_label WHERE issue_id = ?1 ORDER BY label_id")?;
    let rows = stmt.query_map([issue_id], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn find(conn: &Connection, project: &Project, number: i64) -> Result<Issue, StoreError> {
    let mut issue = conn
        .query_row(
            &format!("{ISSUE_COLUMNS} {ISSUE_FROM} WHERE i.project_id = ?1 AND i.number = ?2"),
            [project.id, number],
            issue_row,
        )
        .optional()?
        .ok_or_else(|| not_found("issue"))?;
    issue.labels = label_ids(conn, issue.id)?;
    Ok(issue)
}

fn detail(conn: &Connection, project: &Project, number: i64) -> Result<IssueDetail, StoreError> {
    let issue = find(conn, project, number)?;
    let mut stmt = conn.prepare(
        "SELECT i.number, i.title, i.column_id, i.archived
         FROM issue i JOIN \"column\" c ON c.id = i.column_id
         WHERE i.parent_id = ?1
         ORDER BY c.position, i.position, i.id",
    )?;
    let children = stmt
        .query_map([issue.id], |row| {
            Ok(ChildRef {
                number: row.get(0)?,
                title: row.get(1)?,
                column_id: row.get(2)?,
                archived: row.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let links = links_of(conn, &project.key, issue.id)?;
    let documents = documents_of_issue(conn, issue.id)?;
    Ok(IssueDetail {
        issue,
        children,
        links,
        documents,
    })
}

fn issue_id(conn: &Connection, project_id: i64, number: i64) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM issue WHERE project_id = ?1 AND number = ?2",
            [project_id, number],
            |row| row.get(0),
        )
        .optional()?)
}

fn clean_title(title: &str) -> Result<String, StoreError> {
    let title = title.trim();
    if (1..=MAX_TITLE_CHARS).contains(&title.chars().count()) {
        Ok(title.to_string())
    } else {
        Err(invalid(format!(
            "title must be 1-{MAX_TITLE_CHARS} characters"
        )))
    }
}

fn check_description(text: &str) -> Result<(), StoreError> {
    if text.chars().count() <= MAX_DESCRIPTION_CHARS {
        Ok(())
    } else {
        Err(invalid(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters"
        )))
    }
}

fn exists(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(sql, params, |_| Ok(()))
        .optional()?
        .is_some())
}

fn check_type(conn: &Connection, project: &Project, id: i64) -> Result<(), StoreError> {
    let found = exists(
        conn,
        "SELECT 1 FROM issue_type WHERE id = ?1 AND project_id = ?2",
        [id, project.id],
    )?;
    if found {
        Ok(())
    } else {
        Err(invalid(format!(
            "type_id {id} is not an issue type of project {}",
            project.key
        )))
    }
}

fn check_column(conn: &Connection, project: &Project, id: i64) -> Result<(), StoreError> {
    let found = exists(
        conn,
        "SELECT 1 FROM \"column\" WHERE id = ?1 AND project_id = ?2",
        [id, project.id],
    )?;
    if found {
        Ok(())
    } else {
        Err(invalid(format!(
            "column_id {id} is not a column of project {}",
            project.key
        )))
    }
}

fn check_initiative(conn: &Connection, project: &Project, id: i64) -> Result<(), StoreError> {
    let found = exists(
        conn,
        "SELECT 1 FROM initiative WHERE id = ?1 AND project_id = ?2",
        [id, project.id],
    )?;
    if found {
        Ok(())
    } else {
        Err(invalid(format!(
            "initiative {id} is not an initiative of project {}",
            project.key
        )))
    }
}

fn check_assignee(conn: &Connection, id: i64) -> Result<(), StoreError> {
    if exists(conn, "SELECT 1 FROM user WHERE id = ?1", [id])? {
        Ok(())
    } else {
        Err(invalid(format!("assignee_id {id} is not a user")))
    }
}

/// The ids sorted and without repeats, each checked to be a label of the project.
fn resolve_labels(
    conn: &Connection,
    project: &Project,
    ids: &[i64],
) -> Result<Vec<i64>, StoreError> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    for id in &ids {
        let found = exists(
            conn,
            "SELECT 1 FROM label WHERE id = ?1 AND project_id = ?2",
            [*id, project.id],
        )?;
        if !found {
            return Err(invalid(format!(
                "label {id} is not a label of project {}",
                project.key
            )));
        }
    }
    Ok(ids)
}

fn set_labels(tx: &Transaction<'_>, issue_id: i64, labels: &[i64]) -> Result<(), StoreError> {
    tx.execute("DELETE FROM issue_label WHERE issue_id = ?1", [issue_id])?;
    for label in labels {
        tx.execute(
            "INSERT INTO issue_label (issue_id, label_id) VALUES (?1, ?2)",
            [issue_id, *label],
        )?;
    }
    Ok(())
}

/// How many levels the chain from this issue up to its top-level ancestor has (top-level = 1).
fn level_of(conn: &Connection, id: i64) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "WITH RECURSIVE up(id, parent_id, level) AS (
             SELECT id, parent_id, 1 FROM issue WHERE id = ?1
             UNION ALL
             SELECT i.id, i.parent_id, up.level + 1 FROM issue i JOIN up ON i.id = up.parent_id
         ) SELECT MAX(level) FROM up",
        [id],
        |row| row.get(0),
    )?)
}

/// How many levels the issue and everything below it have (a leaf = 1).
fn height_of(conn: &Connection, id: i64) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "WITH RECURSIVE sub(id, depth) AS (
             SELECT ?1, 1
             UNION ALL
             SELECT i.id, sub.depth + 1 FROM issue i JOIN sub ON i.parent_id = sub.id
         ) SELECT MAX(depth) FROM sub",
        [id],
        |row| row.get(0),
    )?)
}

/// Whether `ancestor` is `id` itself or one of its ancestors.
fn is_self_or_ancestor(conn: &Connection, ancestor: i64, id: i64) -> Result<bool, StoreError> {
    exists(
        conn,
        "WITH RECURSIVE up(id, parent_id) AS (
             SELECT id, parent_id FROM issue WHERE id = ?1
             UNION ALL
             SELECT i.id, i.parent_id FROM issue i JOIN up ON i.id = up.parent_id
         ) SELECT 1 FROM up WHERE id = ?2",
        [id, ancestor],
    )
}

/// Resolves the parent number to an id and applies the parent rules. `child`
/// is the issue being re-parented; `None` when the issue is being created.
fn check_parent(
    conn: &Connection,
    project: &Project,
    number: i64,
    child: Option<i64>,
) -> Result<i64, StoreError> {
    let parent_id = issue_id(conn, project.id, number)?.ok_or_else(|| {
        invalid(format!(
            "parent {} not found in project {}",
            number, project.key
        ))
    })?;
    if let Some(child_id) = child
        && is_self_or_ancestor(conn, child_id, parent_id)?
    {
        return Err(conflict(format!(
            "issue {}-{number} cannot be the parent: it is the issue itself or below it",
            project.key
        )));
    }
    let below = match child {
        Some(child_id) => height_of(conn, child_id)?,
        None => 1,
    };
    if level_of(conn, parent_id)? + below > MAX_DEPTH {
        return Err(invalid(format!(
            "issues can be nested at most {MAX_DEPTH} levels deep"
        )));
    }
    Ok(parent_id)
}

// ---- reads ----

pub fn get_issue(
    conn: &Connection,
    project_key: &str,
    number: i64,
) -> Result<IssueDetail, StoreError> {
    let project = get_project(conn, project_key)?;
    detail(conn, &project, number)
}

/// Ordered by column position, then issue position.
pub fn list_issues(
    conn: &Connection,
    project_key: &str,
    filter: &IssueFilter,
) -> Result<IssueList, StoreError> {
    let project = get_project(conn, project_key)?;
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = filter.offset.unwrap_or(0);
    if offset < 0 {
        return Err(invalid("offset must not be negative"));
    }

    let mut conditions = vec!["i.project_id = ?".to_string()];
    let mut values = vec![SqlValue::Integer(project.id)];
    if let Some(column_id) = filter.column_id {
        conditions.push("i.column_id = ?".into());
        values.push(SqlValue::Integer(column_id));
    }
    if let Some(state_type) = filter.state_type {
        conditions.push("c.state_type = ?".into());
        values.push(SqlValue::Text(state_type.as_str().to_string()));
    }
    if let Some(label_id) = filter.label_id {
        conditions.push(
            "EXISTS (SELECT 1 FROM issue_label il WHERE il.issue_id = i.id AND il.label_id = ?)"
                .into(),
        );
        values.push(SqlValue::Integer(label_id));
    }
    if let Some(assignee_id) = filter.assignee_id {
        conditions.push("i.assignee_id = ?".into());
        values.push(SqlValue::Integer(assignee_id));
    }
    match filter.parent {
        None => {}
        Some(ParentFilter::None) => conditions.push("i.parent_id IS NULL".into()),
        Some(ParentFilter::Number(number)) => {
            conditions.push("par.number = ?".into());
            values.push(SqlValue::Integer(number));
        }
    }
    if filter.archived_only {
        conditions.push("i.archived = 1".into());
    } else if !filter.include_archived {
        conditions.push("i.archived = 0".into());
    }
    let wanted = conditions.join(" AND ");

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) {ISSUE_FROM} WHERE {wanted}"),
        params_from_iter(values.iter()),
        |row| row.get(0),
    )?;
    values.push(SqlValue::Integer(limit));
    values.push(SqlValue::Integer(offset));
    let mut stmt = conn.prepare(&format!(
        "{ISSUE_COLUMNS} {ISSUE_FROM} WHERE {wanted} {ISSUE_ORDER} LIMIT ? OFFSET ?"
    ))?;
    let mut issues = stmt
        .query_map(params_from_iter(values.iter()), issue_row)?
        .collect::<Result<Vec<_>, _>>()?;
    for issue in &mut issues {
        issue.labels = label_ids(conn, issue.id)?;
    }
    Ok(IssueList {
        issues,
        total,
        limit,
        offset,
    })
}

// ---- writes ----

pub fn create_issue(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    new: NewIssue,
) -> Result<IssueDetail, StoreError> {
    let title = clean_title(&new.title)?;
    let description = new.description.unwrap_or_default();
    check_description(&description)?;
    let project = get_project(tx, project_key)?;

    let type_id = match new.type_id {
        Some(id) => {
            check_type(tx, &project, id)?;
            id
        }
        None => {
            let mut stmt =
                tx.prepare("SELECT id FROM issue_type WHERE project_id = ?1 ORDER BY id LIMIT 2")?;
            let types = stmt
                .query_map([project.id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            match types.as_slice() {
                [only] => *only,
                _ => return Err(invalid("type_id is required")),
            }
        }
    };
    let column_id = match new.column_id {
        Some(id) => {
            check_column(tx, &project, id)?;
            id
        }
        None => default_column(tx, &project)?,
    };
    if let Some(id) = new.assignee_id {
        check_assignee(tx, id)?;
    }
    let parent_id = new
        .parent
        .map(|number| check_parent(tx, &project, number, None))
        .transpose()?;
    if let Some(id) = new.initiative {
        check_initiative(tx, &project, id)?;
    }
    let labels = resolve_labels(tx, &project, &new.labels)?;

    let number: i64 = tx.query_row(
        "SELECT next_issue_number FROM project WHERE id = ?1",
        [project.id],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE project SET next_issue_number = ?1 WHERE id = ?2",
        [number + 1, project.id],
    )?;
    let last: f64 = tx.query_row(
        "SELECT COALESCE(MAX(position), 0) FROM issue WHERE column_id = ?1",
        [column_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO issue (project_id, number, type_id, column_id, title, description,
                            assignee_id, parent_id, priority, position, created, updated,
                            initiative_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
        params![
            project.id,
            number,
            type_id,
            column_id,
            title,
            description,
            new.assignee_id,
            parent_id,
            new.priority,
            last + POSITION_STEP,
            now,
            new.initiative
        ],
    )?;
    let id = tx.last_insert_rowid();
    set_labels(tx, id, &labels)?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        id,
        "issue.created",
        &json!({
            "number": number,
            "identifier": format!("{}-{number}", project.key),
            "title": title,
            "type_id": type_id,
            "column_id": column_id,
            "parent": new.parent,
            "assignee_id": new.assignee_id,
            "initiative": new.initiative,
            "priority": new.priority,
            "labels": labels,
        }),
    )?;
    detail(tx, &project, number)
}

fn default_column(conn: &Connection, project: &Project) -> Result<i64, StoreError> {
    let backlog = conn
        .query_row(
            "SELECT id FROM \"column\" WHERE project_id = ?1 AND state_type = ?2
             ORDER BY position, id LIMIT 1",
            params![project.id, StateType::Backlog],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = backlog {
        return Ok(id);
    }
    conn.query_row(
        "SELECT id FROM \"column\" WHERE project_id = ?1 ORDER BY position, id LIMIT 1",
        [project.id],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| invalid(format!("project {} has no columns", project.key)))
}

/// Records one changed field as `{"from": old, "to": new}` for the event.
fn changed(changes: &mut Map<String, Value>, field: &str, from: Value, to: Value) {
    changes.insert(field.to_string(), json!({ "from": from, "to": to }));
}

pub fn update_issue(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    number: i64,
    patch: IssuePatch,
) -> Result<IssueDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, &project, number)?;
    let mut next = current.clone();
    let mut changes = Map::new();

    if let Some(title) = &patch.title {
        next.title = clean_title(title)?;
        if next.title != current.title {
            changed(
                &mut changes,
                "title",
                json!(current.title),
                json!(next.title),
            );
        }
    }
    if let Some(description) = &patch.description {
        next.description = description.clone().unwrap_or_default();
        check_description(&next.description)?;
        if next.description != current.description {
            changed(
                &mut changes,
                "description",
                json!(current.description),
                json!(next.description),
            );
        }
    }
    if let Some(type_id) = patch.type_id {
        check_type(tx, &project, type_id)?;
        if type_id != current.type_id {
            next.type_id = type_id;
            changed(
                &mut changes,
                "type_id",
                json!(current.type_id),
                json!(type_id),
            );
        }
    }
    if let Some(assignee) = patch.assignee_id {
        if let Some(id) = assignee {
            check_assignee(tx, id)?;
        }
        if assignee != current.assignee_id {
            next.assignee_id = assignee;
            changed(
                &mut changes,
                "assignee_id",
                json!(current.assignee_id),
                json!(assignee),
            );
        }
    }
    if let Some(parent) = patch.parent
        && parent != current.parent
    {
        next.parent_id = parent
            .map(|number| check_parent(tx, &project, number, Some(current.id)))
            .transpose()?;
        next.parent = parent;
        changed(&mut changes, "parent", json!(current.parent), json!(parent));
    }
    if let Some(initiative) = patch.initiative {
        if let Some(id) = initiative {
            check_initiative(tx, &project, id)?;
        }
        if initiative != current.initiative_id {
            next.initiative_id = initiative;
            changed(
                &mut changes,
                "initiative",
                json!(current.initiative_id),
                json!(initiative),
            );
        }
    }
    if let Some(priority) = patch.priority
        && priority != current.priority
    {
        next.priority = priority;
        changed(
            &mut changes,
            "priority",
            json!(current.priority),
            json!(priority),
        );
    }
    if let Some(labels) = &patch.labels {
        next.labels = resolve_labels(tx, &project, labels)?;
        if next.labels != current.labels {
            changed(
                &mut changes,
                "labels",
                json!(current.labels),
                json!(next.labels),
            );
        }
    }

    if changes.is_empty() {
        return detail(tx, &project, number);
    }
    tx.execute(
        "UPDATE issue SET title = ?1, description = ?2, type_id = ?3, assignee_id = ?4,
                          parent_id = ?5, priority = ?6, updated = ?7, initiative_id = ?8
         WHERE id = ?9",
        params![
            next.title,
            next.description,
            next.type_id,
            next.assignee_id,
            next.parent_id,
            next.priority,
            now,
            next.initiative_id,
            current.id
        ],
    )?;
    if next.labels != current.labels {
        set_labels(tx, current.id, &next.labels)?;
    }
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        current.id,
        "issue.updated",
        &json!({ "changes": changes }),
    )?;
    detail(tx, &project, number)
}

/// Where in the target column an issue goes, after resolving the request.
#[derive(Clone, Copy)]
enum Place {
    End,
    Before(i64),
    After(i64),
    At(f64),
}

/// The issues of a column except `skip`, as (id, position), in column order.
fn column_order(
    conn: &Connection,
    column_id: i64,
    skip: Option<i64>,
) -> Result<Vec<(i64, f64)>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT id, position FROM issue WHERE column_id = ?1 ORDER BY position, id")?;
    let rows = stmt
        .query_map([column_id], |row| Ok((row.get::<_, i64>(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().filter(|r| Some(r.0) != skip).collect())
}

fn renumber(tx: &Transaction<'_>, order: &mut [(i64, f64)]) -> Result<(), StoreError> {
    for ((id, position), step) in order.iter_mut().zip(1_u32..) {
        let wanted = POSITION_STEP * f64::from(step);
        if *position != wanted {
            tx.execute(
                "UPDATE issue SET position = ?1 WHERE id = ?2",
                params![wanted, *id],
            )?;
            *position = wanted;
        }
    }
    Ok(())
}

/// A position for `place` among `others`; renumbers `others` first when the
/// room between the two neighbours has run out. Returns whether it did.
fn position_for(
    tx: &Transaction<'_>,
    others: &mut [(i64, f64)],
    place: Place,
) -> Result<(f64, bool), StoreError> {
    let mut renumbered = false;
    loop {
        let index_of = |anchor: i64| others.iter().position(|o| o.0 == anchor);
        let (low, high) = match place {
            Place::At(position) => return Ok((position, false)),
            Place::End => (others.last().map(|o| o.1), None),
            Place::Before(anchor) => {
                let at = index_of(anchor);
                (
                    at.and_then(|i| i.checked_sub(1)).map(|i| others[i].1),
                    at.map(|i| others[i].1),
                )
            }
            Place::After(anchor) => {
                let at = index_of(anchor);
                (
                    at.map(|i| others[i].1),
                    at.and_then(|i| others.get(i + 1)).map(|o| o.1),
                )
            }
        };
        return Ok(match (low, high) {
            (Some(low), Some(high)) if high - low < MIN_GAP && !renumbered => {
                renumber(tx, others)?;
                renumbered = true;
                continue;
            }
            (Some(low), Some(high)) => ((low + high) / 2.0, renumbered),
            (None, Some(high)) => (high - POSITION_STEP, renumbered),
            (Some(low), None) => (low + POSITION_STEP, renumbered),
            (None, None) => (POSITION_STEP, renumbered),
        });
    }
}

/// Changes column and/or position. Moving to where the issue already is
/// changes nothing and writes no event.
pub fn move_issue(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    number: i64,
    target: MoveTarget,
) -> Result<IssueDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, &project, number)?;
    check_column(tx, &project, target.column_id)?;
    let given = [
        target.before.is_some(),
        target.after.is_some(),
        target.position.is_some(),
    ];
    if given.into_iter().filter(|g| *g).count() > 1 {
        return Err(invalid("give at most one of before, after and position"));
    }
    if target.position.is_some_and(|p| !p.is_finite()) {
        return Err(invalid("position must be a finite number"));
    }

    let mut others = column_order(tx, target.column_id, Some(current.id))?;
    let anchor = |wanted: i64| -> Result<i64, StoreError> {
        let id = issue_id(tx, project.id, wanted)?
            .filter(|id| others.iter().any(|o| o.0 == *id))
            .ok_or_else(|| invalid(format!("issue {wanted} is not in the target column")))?;
        Ok(id)
    };
    if target.before == Some(number) || target.after == Some(number) {
        return Err(invalid("an issue cannot be placed relative to itself"));
    }
    let place = match (target.before, target.after, target.position) {
        (Some(before), _, _) => Place::Before(anchor(before)?),
        (_, Some(after), _) => Place::After(anchor(after)?),
        (_, _, Some(position)) => Place::At(position),
        _ => Place::End,
    };

    if current.column_id == target.column_id {
        let full = column_order(tx, target.column_id, None)?;
        let at = full.iter().position(|o| o.0 == current.id);
        let already = match place {
            Place::End => at.is_some_and(|i| i + 1 == full.len()),
            Place::Before(id) => at.and_then(|i| full.get(i + 1)).is_some_and(|o| o.0 == id),
            Place::After(id) => at
                .and_then(|i| i.checked_sub(1))
                .is_some_and(|i| full[i].0 == id),
            Place::At(position) => position == current.position,
        };
        if already {
            return detail(tx, &project, number);
        }
    }

    let (position, renumbered) = position_for(tx, &mut others, place)?;
    tx.execute(
        "UPDATE issue SET column_id = ?1, position = ?2, updated = ?3 WHERE id = ?4",
        params![target.column_id, position, now, current.id],
    )?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        current.id,
        "issue.moved",
        &json!({
            "from": { "column_id": current.column_id, "position": current.position },
            "to": { "column_id": target.column_id, "position": position },
            "renumbered": renumbered,
        }),
    )?;
    detail(tx, &project, number)
}

fn set_archived(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    number: i64,
    archived: bool,
) -> Result<IssueDetail, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, &project, number)?;
    if current.archived == archived {
        return detail(tx, &project, number);
    }
    tx.execute(
        "UPDATE issue SET archived = ?1, updated = ?2 WHERE id = ?3",
        params![archived, now, current.id],
    )?;
    let kind = if archived {
        "issue.archived"
    } else {
        "issue.unarchived"
    };
    emit(
        tx,
        now,
        actor,
        project.id,
        "issue",
        current.id,
        kind,
        &json!({ "number": number, "title": current.title }),
    )?;
    detail(tx, &project, number)
}

/// Soft delete: the issue keeps its number, column and children.
pub fn archive_issue(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    number: i64,
) -> Result<IssueDetail, StoreError> {
    set_archived(tx, now, actor, project_key, number, true)
}

pub fn unarchive_issue(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    number: i64,
) -> Result<IssueDetail, StoreError> {
    set_archived(tx, now, actor, project_key, number, false)
}
