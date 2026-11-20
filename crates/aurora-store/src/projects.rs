//! Projects and what a project owns: columns, issue types and labels.
//!
//! Plain functions over a transaction (writes) or a connection (reads). Every
//! write appends its event in the same transaction, so call them from inside
//! [`crate::Store::write`] and return their error to roll everything back.
//! `project` arguments are project keys; child ids must belong to that project.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::StoreError;

/// Why a request was refused. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    /// No valid credentials.
    #[error("{0}")]
    Unauthorized(String),
    /// Valid credentials that may not do this.
    #[error("{0}")]
    Forbidden(String),
}

pub(crate) fn not_found(what: &str) -> StoreError {
    Rejection::NotFound(what.to_string()).into()
}

pub(crate) fn conflict(reason: impl Into<String>) -> StoreError {
    Rejection::Conflict(reason.into()).into()
}

pub(crate) fn invalid(reason: impl Into<String>) -> StoreError {
    Rejection::Invalid(reason.into()).into()
}

/// A unique or foreign-key violation is a refusal, not a database failure.
pub(crate) fn refusal(err: rusqlite::Error, reason: impl FnOnce() -> String) -> StoreError {
    match &err {
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::ConstraintViolation => {
            conflict(reason())
        }
        _ => err.into(),
    }
}

/// What a column means for the work in it. Fixed when the column is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateType {
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
}

impl StateType {
    pub fn as_str(self) -> &'static str {
        match self {
            StateType::Backlog => "backlog",
            StateType::Unstarted => "unstarted",
            StateType::Started => "started",
            StateType::Completed => "completed",
            StateType::Canceled => "canceled",
        }
    }

    pub fn parse(text: &str) -> Option<StateType> {
        Some(match text {
            "backlog" => StateType::Backlog,
            "unstarted" => StateType::Unstarted,
            "started" => StateType::Started,
            "completed" => StateType::Completed,
            "canceled" => StateType::Canceled,
            _ => return None,
        })
    }
}

impl ToSql for StateType {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for StateType {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        StateType::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("state type {text}").into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub project: Project,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Column {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub state_type: StateType,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueType {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Label {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub color: String,
}

/// A column to create with a project.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewColumn {
    pub name: String,
    pub state_type: StateType,
}

/// The columns a project starts with when the request names none.
pub fn default_columns() -> Vec<NewColumn> {
    [
        ("Backlog", StateType::Backlog),
        ("Todo", StateType::Unstarted),
        ("In Progress", StateType::Started),
        ("Done", StateType::Completed),
        ("Canceled", StateType::Canceled),
    ]
    .into_iter()
    .map(|(name, state_type)| NewColumn {
        name: name.to_string(),
        state_type,
    })
    .collect()
}

fn project_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        key: row.get(1)?,
        name: row.get(2)?,
        created: row.get(3)?,
    })
}

fn column_row(row: &Row<'_>) -> rusqlite::Result<Column> {
    Ok(Column {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        state_type: row.get(3)?,
        position: row.get(4)?,
    })
}

fn issue_type_row(row: &Row<'_>) -> rusqlite::Result<IssueType> {
    Ok(IssueType {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
    })
}

fn label_row(row: &Row<'_>) -> rusqlite::Result<Label> {
    Ok(Label {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        color: row.get(3)?,
    })
}

const COLUMN_SELECT: &str = "SELECT id, project_id, name, state_type, position FROM \"column\"";

fn validate_key(key: &str) -> Result<(), StoreError> {
    let mut chars = key.chars();
    let shape_ok = (2..=10).contains(&key.chars().count())
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if shape_ok {
        Ok(())
    } else {
        Err(invalid(
            "project key must be 2-10 characters: an uppercase letter, then uppercase letters or digits",
        ))
    }
}

fn clean_name(what: &str, name: &str) -> Result<String, StoreError> {
    let name = name.trim();
    if (1..=100).contains(&name.chars().count()) {
        Ok(name.to_string())
    } else {
        Err(invalid(format!("{what} name must be 1-100 characters")))
    }
}

fn clean_color(color: &str) -> Result<String, StoreError> {
    let digits = color.strip_prefix('#').unwrap_or("");
    if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(color.to_ascii_lowercase())
    } else {
        Err(invalid("label color must look like #rrggbb"))
    }
}

pub(crate) fn emit(
    conn: &Connection,
    now: i64,
    project_id: i64,
    entity_kind: &str,
    entity_id: i64,
    kind: &str,
    payload: &Value,
) -> Result<(), StoreError> {
    // The actor stays NULL for project and issue events until the HTTP layer
    // passes the principal down to these functions (a follow-up to TRK-2).
    emit_as(
        conn,
        now,
        None,
        Some(project_id),
        entity_kind,
        entity_id,
        kind,
        payload,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_as(
    conn: &Connection,
    now: i64,
    actor_id: Option<i64>,
    project_id: Option<i64>,
    entity_kind: &str,
    entity_id: i64,
    kind: &str,
    payload: &Value,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            now,
            actor_id,
            project_id,
            entity_kind,
            entity_id,
            kind,
            payload.to_string()
        ],
    )?;
    Ok(())
}

// ---- projects ----

pub fn get_project(conn: &Connection, key: &str) -> Result<Project, StoreError> {
    conn.query_row(
        "SELECT id, key, name, created FROM project WHERE key = ?1",
        [key],
        project_row,
    )
    .optional()?
    .ok_or_else(|| not_found("project"))
}

pub fn list_projects(conn: &Connection) -> Result<Vec<Project>, StoreError> {
    let mut stmt = conn.prepare("SELECT id, key, name, created FROM project ORDER BY id")?;
    let rows = stmt.query_map([], project_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Creates the project with `columns`, or the default columns when `None`.
pub fn create_project(
    tx: &Transaction<'_>,
    now: i64,
    key: &str,
    name: &str,
    columns: Option<&[NewColumn]>,
) -> Result<ProjectDetail, StoreError> {
    validate_key(key)?;
    let name = clean_name("project", name)?;
    let defaults;
    let columns = match columns {
        Some([]) => return Err(invalid("a project needs at least one column")),
        Some(columns) => columns,
        None => {
            defaults = default_columns();
            &defaults
        }
    };
    tx.execute(
        "INSERT INTO project (key, name, created) VALUES (?1, ?2, ?3)",
        params![key, name, now],
    )
    .map_err(|err| refusal(err, || format!("project key {key} is already taken")))?;
    let project = get_project(tx, key)?;

    let mut names = Vec::with_capacity(columns.len());
    for (position, column) in (0_i64..).zip(columns) {
        let column_name = clean_name("column", &column.name)?;
        tx.execute(
            "INSERT INTO \"column\" (project_id, name, state_type, position) VALUES (?1, ?2, ?3, ?4)",
            params![project.id, column_name, column.state_type, position],
        )
        .map_err(|err| refusal(err, || format!("duplicate column name {column_name}")))?;
        names.push(column_name);
    }
    emit(
        tx,
        now,
        project.id,
        "project",
        project.id,
        "project.created",
        &json!({ "key": project.key, "name": project.name, "columns": names }),
    )?;
    let columns = list_columns(tx, key)?;
    Ok(ProjectDetail { project, columns })
}

/// The key never changes; only the name can.
pub fn rename_project(
    tx: &Transaction<'_>,
    now: i64,
    key: &str,
    name: &str,
) -> Result<Project, StoreError> {
    let name = clean_name("project", name)?;
    let mut project = get_project(tx, key)?;
    if project.name != name {
        tx.execute(
            "UPDATE project SET name = ?1 WHERE id = ?2",
            params![name, project.id],
        )?;
        emit(
            tx,
            now,
            project.id,
            "project",
            project.id,
            "project.updated",
            &json!({ "name": name, "previous_name": project.name }),
        )?;
        project.name = name;
    }
    Ok(project)
}

// ---- columns ----

pub fn list_columns(conn: &Connection, project: &str) -> Result<Vec<Column>, StoreError> {
    let project = get_project(conn, project)?;
    let mut stmt = conn.prepare(&format!(
        "{COLUMN_SELECT} WHERE project_id = ?1 ORDER BY position, id"
    ))?;
    let rows = stmt.query_map([project.id], column_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The project's columns of one state type, in board order.
pub fn columns_by_state_type(
    conn: &Connection,
    project: &str,
    state_type: StateType,
) -> Result<Vec<Column>, StoreError> {
    let project = get_project(conn, project)?;
    let mut stmt = conn.prepare(&format!(
        "{COLUMN_SELECT} WHERE project_id = ?1 AND state_type = ?2 ORDER BY position, id"
    ))?;
    let rows = stmt.query_map(params![project.id, state_type], column_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn get_column(conn: &Connection, project_id: i64, id: i64) -> Result<Column, StoreError> {
    conn.query_row(
        &format!("{COLUMN_SELECT} WHERE id = ?1 AND project_id = ?2"),
        [id, project_id],
        column_row,
    )
    .optional()?
    .ok_or_else(|| not_found("column"))
}

/// Appended after the project's last column.
pub fn create_column(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    name: &str,
    state_type: StateType,
) -> Result<Column, StoreError> {
    let name = clean_name("column", name)?;
    let project = get_project(tx, project)?;
    let position: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position), -1) + 1 FROM \"column\" WHERE project_id = ?1",
        [project.id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO \"column\" (project_id, name, state_type, position) VALUES (?1, ?2, ?3, ?4)",
        params![project.id, name, state_type, position],
    )
    .map_err(|err| {
        refusal(err, || {
            format!("the project already has a column named {name}")
        })
    })?;
    let column = get_column(tx, project.id, tx.last_insert_rowid())?;
    emit(
        tx,
        now,
        project.id,
        "column",
        column.id,
        "column.created",
        &json!({ "name": column.name, "state_type": state_type, "position": position }),
    )?;
    Ok(column)
}

pub fn rename_column(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
    name: &str,
) -> Result<Column, StoreError> {
    let name = clean_name("column", name)?;
    let project = get_project(tx, project)?;
    let mut column = get_column(tx, project.id, id)?;
    if column.name != name {
        tx.execute(
            "UPDATE \"column\" SET name = ?1 WHERE id = ?2",
            params![name, id],
        )
        .map_err(|err| {
            refusal(err, || {
                format!("the project already has a column named {name}")
            })
        })?;
        emit(
            tx,
            now,
            project.id,
            "column",
            id,
            "column.renamed",
            &json!({ "name": name, "previous_name": column.name }),
        )?;
        column.name = name;
    }
    Ok(column)
}

/// `ids` must be exactly the project's current columns, in the new order.
pub fn reorder_columns(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    ids: &[i64],
) -> Result<Vec<Column>, StoreError> {
    let project = get_project(tx, project)?;
    let current = list_columns(tx, &project.key)?;
    let mut wanted = ids.to_vec();
    wanted.sort_unstable();
    let mut existing: Vec<i64> = current.iter().map(|c| c.id).collect();
    existing.sort_unstable();
    if wanted != existing {
        return Err(invalid(
            "ids must list every column of the project exactly once",
        ));
    }
    if current.iter().map(|c| c.id).eq(ids.iter().copied()) {
        return Ok(current);
    }
    for (position, id) in (0_i64..).zip(ids) {
        tx.execute(
            "UPDATE \"column\" SET position = ?1 WHERE id = ?2",
            params![position, id],
        )?;
    }
    emit(
        tx,
        now,
        project.id,
        "project",
        project.id,
        "column.reordered",
        &json!({ "ids": ids }),
    )?;
    list_columns(tx, &project.key)
}

/// Refused while any issue, archived ones included, is in the column.
pub fn delete_column(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project)?;
    let column = get_column(tx, project.id, id)?;
    let issues: i64 = tx.query_row(
        "SELECT COUNT(*) FROM issue WHERE column_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    if issues > 0 {
        return Err(conflict(format!(
            "column {} still has {issues} issue(s); move them first",
            column.name
        )));
    }
    tx.execute("DELETE FROM \"column\" WHERE id = ?1", [id])
        .map_err(|err| refusal(err, || format!("column {} is still in use", column.name)))?;
    emit(
        tx,
        now,
        project.id,
        "column",
        id,
        "column.deleted",
        &json!({ "name": column.name, "state_type": column.state_type }),
    )
}

// ---- issue types ----

pub fn list_issue_types(conn: &Connection, project: &str) -> Result<Vec<IssueType>, StoreError> {
    let project = get_project(conn, project)?;
    let mut stmt = conn
        .prepare("SELECT id, project_id, name FROM issue_type WHERE project_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map([project.id], issue_type_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn get_issue_type(conn: &Connection, project_id: i64, id: i64) -> Result<IssueType, StoreError> {
    conn.query_row(
        "SELECT id, project_id, name FROM issue_type WHERE id = ?1 AND project_id = ?2",
        [id, project_id],
        issue_type_row,
    )
    .optional()?
    .ok_or_else(|| not_found("issue type"))
}

pub fn create_issue_type(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    name: &str,
) -> Result<IssueType, StoreError> {
    let name = clean_name("issue type", name)?;
    let project = get_project(tx, project)?;
    tx.execute(
        "INSERT INTO issue_type (project_id, name) VALUES (?1, ?2)",
        params![project.id, name],
    )
    .map_err(|err| {
        refusal(err, || {
            format!("the project already has an issue type named {name}")
        })
    })?;
    let issue_type = get_issue_type(tx, project.id, tx.last_insert_rowid())?;
    emit(
        tx,
        now,
        project.id,
        "issue_type",
        issue_type.id,
        "issue_type.created",
        &json!({ "name": issue_type.name }),
    )?;
    Ok(issue_type)
}

pub fn rename_issue_type(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
    name: &str,
) -> Result<IssueType, StoreError> {
    let name = clean_name("issue type", name)?;
    let project = get_project(tx, project)?;
    let mut issue_type = get_issue_type(tx, project.id, id)?;
    if issue_type.name != name {
        tx.execute(
            "UPDATE issue_type SET name = ?1 WHERE id = ?2",
            params![name, id],
        )
        .map_err(|err| {
            refusal(err, || {
                format!("the project already has an issue type named {name}")
            })
        })?;
        emit(
            tx,
            now,
            project.id,
            "issue_type",
            id,
            "issue_type.renamed",
            &json!({ "name": name, "previous_name": issue_type.name }),
        )?;
        issue_type.name = name;
    }
    Ok(issue_type)
}

/// Refused while any issue uses the type.
pub fn delete_issue_type(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project)?;
    let issue_type = get_issue_type(tx, project.id, id)?;
    let issues: i64 = tx.query_row(
        "SELECT COUNT(*) FROM issue WHERE type_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    if issues > 0 {
        return Err(conflict(format!(
            "issue type {} is used by {issues} issue(s)",
            issue_type.name
        )));
    }
    tx.execute("DELETE FROM issue_type WHERE id = ?1", [id])
        .map_err(|err| {
            refusal(err, || {
                format!("issue type {} is still in use", issue_type.name)
            })
        })?;
    emit(
        tx,
        now,
        project.id,
        "issue_type",
        id,
        "issue_type.deleted",
        &json!({ "name": issue_type.name }),
    )
}

// ---- labels ----

pub fn list_labels(conn: &Connection, project: &str) -> Result<Vec<Label>, StoreError> {
    let project = get_project(conn, project)?;
    let mut stmt = conn.prepare(
        "SELECT id, project_id, name, color FROM label WHERE project_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([project.id], label_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn get_label(conn: &Connection, project_id: i64, id: i64) -> Result<Label, StoreError> {
    conn.query_row(
        "SELECT id, project_id, name, color FROM label WHERE id = ?1 AND project_id = ?2",
        [id, project_id],
        label_row,
    )
    .optional()?
    .ok_or_else(|| not_found("label"))
}

pub fn create_label(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    name: &str,
    color: &str,
) -> Result<Label, StoreError> {
    let name = clean_name("label", name)?;
    let color = clean_color(color)?;
    let project = get_project(tx, project)?;
    tx.execute(
        "INSERT INTO label (project_id, name, color) VALUES (?1, ?2, ?3)",
        params![project.id, name, color],
    )
    .map_err(|err| {
        refusal(err, || {
            format!("the project already has a label named {name}")
        })
    })?;
    let label = get_label(tx, project.id, tx.last_insert_rowid())?;
    emit(
        tx,
        now,
        project.id,
        "label",
        label.id,
        "label.created",
        &json!({ "name": label.name, "color": label.color }),
    )?;
    Ok(label)
}

pub fn rename_label(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
    name: &str,
) -> Result<Label, StoreError> {
    let name = clean_name("label", name)?;
    let project = get_project(tx, project)?;
    let mut label = get_label(tx, project.id, id)?;
    if label.name != name {
        tx.execute(
            "UPDATE label SET name = ?1 WHERE id = ?2",
            params![name, id],
        )
        .map_err(|err| {
            refusal(err, || {
                format!("the project already has a label named {name}")
            })
        })?;
        emit(
            tx,
            now,
            project.id,
            "label",
            id,
            "label.renamed",
            &json!({ "name": name, "previous_name": label.name }),
        )?;
        label.name = name;
    }
    Ok(label)
}

pub fn recolor_label(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
    color: &str,
) -> Result<Label, StoreError> {
    let color = clean_color(color)?;
    let project = get_project(tx, project)?;
    let mut label = get_label(tx, project.id, id)?;
    if label.color != color {
        tx.execute(
            "UPDATE label SET color = ?1 WHERE id = ?2",
            params![color, id],
        )?;
        emit(
            tx,
            now,
            project.id,
            "label",
            id,
            "label.recolored",
            &json!({ "color": color, "previous_color": label.color }),
        )?;
        label.color = color;
    }
    Ok(label)
}

/// Also removes the label from every issue that carries it.
pub fn delete_label(
    tx: &Transaction<'_>,
    now: i64,
    project: &str,
    id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project)?;
    let label = get_label(tx, project.id, id)?;
    let issues: i64 = tx.query_row(
        "SELECT COUNT(*) FROM issue_label WHERE label_id = ?1",
        [id],
        |row| row.get(0),
    )?;
    tx.execute("DELETE FROM label WHERE id = ?1", [id])?;
    emit(
        tx,
        now,
        project.id,
        "label",
        id,
        "label.deleted",
        &json!({ "name": label.name, "issues": issues }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    fn rejection(err: StoreError) -> Rejection {
        match err {
            StoreError::Rejected(rejection) => rejection,
            other => panic!("expected a rejection, got {other}"),
        }
    }

    /// (kind, entity_kind, entity_id, project_id, payload) of every event, oldest first.
    async fn events(store: &Store) -> Vec<(String, String, i64, Option<i64>, Value)> {
        store
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT kind, entity_kind, entity_id, project_id, payload_json, actor_id
                     FROM event ORDER BY id",
                )?;
                let rows = stmt.query_map([], |r| {
                    let payload: String = r.get(4)?;
                    assert_eq!(r.get::<_, Option<i64>>(5)?, None, "actor is NULL for now");
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        serde_json::from_str(&payload).unwrap(),
                    ))
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap()
    }

    async fn event_kinds(store: &Store) -> Vec<String> {
        events(store).await.into_iter().map(|e| e.0).collect()
    }

    async fn new_project(store: &Store, key: &'static str) -> ProjectDetail {
        store
            .write(move |tx| create_project(tx, 1000, key, "Name", None))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn project_gets_default_columns_in_order_and_one_event() {
        let (_dir, store) = temp_store();
        let created = new_project(&store, "TRK").await;
        assert_eq!(created.project.key, "TRK");
        assert_eq!(created.project.created, 1000);
        let seen: Vec<_> = created
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.state_type, c.position))
            .collect();
        assert_eq!(
            seen,
            [
                ("Backlog", StateType::Backlog, 0),
                ("Todo", StateType::Unstarted, 1),
                ("In Progress", StateType::Started, 2),
                ("Done", StateType::Completed, 3),
                ("Canceled", StateType::Canceled, 4),
            ]
        );
        let events = events(&store).await;
        assert_eq!(events.len(), 1);
        let (kind, entity_kind, entity_id, project_id, payload) = &events[0];
        assert_eq!(kind, "project.created");
        assert_eq!(entity_kind, "project");
        assert_eq!(*entity_id, created.project.id);
        assert_eq!(*project_id, Some(created.project.id));
        assert_eq!(payload["key"], "TRK");
        assert_eq!(payload["columns"].as_array().unwrap().len(), 5);
        store.close().await;
    }

    #[tokio::test]
    async fn project_with_its_own_columns_and_refusals() {
        let (_dir, store) = temp_store();
        let own = [
            NewColumn {
                name: " Open ".into(),
                state_type: StateType::Unstarted,
            },
            NewColumn {
                name: "Shipped".into(),
                state_type: StateType::Completed,
            },
        ];
        let created = store
            .write(move |tx| create_project(tx, 1, "OWN", "Own", Some(&own)))
            .await
            .unwrap();
        let names: Vec<_> = created.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Open", "Shipped"]);

        let empty = store
            .write(|tx| create_project(tx, 1, "EMP", "Empty", Some(&[])))
            .await
            .unwrap_err();
        assert!(matches!(rejection(empty), Rejection::Invalid(_)));

        let dup = [
            NewColumn {
                name: "A".into(),
                state_type: StateType::Backlog,
            },
            NewColumn {
                name: "A".into(),
                state_type: StateType::Started,
            },
        ];
        let err = store
            .write(move |tx| create_project(tx, 1, "DUP", "Dup", Some(&dup)))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Conflict(_)));
        let projects = store.read(list_projects).await.unwrap();
        assert_eq!(projects.len(), 1, "the failed project left nothing behind");
        assert_eq!(event_kinds(&store).await, ["project.created"]);
        store.close().await;
    }

    #[tokio::test]
    async fn bad_and_duplicate_keys_and_names_are_refused() {
        let (_dir, store) = temp_store();
        new_project(&store, "TRK").await;
        for bad in ["", "A", "trk", "1AB", "AB-C", "ABCDEFGHIJK", "AB CD", "ÄB"] {
            let err = store
                .write(move |tx| create_project(tx, 1, bad, "N", None))
                .await
                .unwrap_err();
            assert!(
                matches!(rejection(err), Rejection::Invalid(_)),
                "key {bad:?}"
            );
        }
        for ok in ["AB", "A1", "ABCDEFGHI1"] {
            store
                .write(move |tx| create_project(tx, 1, ok, "N", None))
                .await
                .unwrap();
        }
        let dup = store
            .write(|tx| create_project(tx, 1, "TRK", "Again", None))
            .await
            .unwrap_err();
        assert!(matches!(rejection(dup), Rejection::Conflict(_)));
        for bad_name in ["", "   ", &"x".repeat(101)] {
            let bad_name = bad_name.to_string();
            let err = store
                .write(move |tx| create_project(tx, 1, "NEW", &bad_name, None))
                .await
                .unwrap_err();
            assert!(matches!(rejection(err), Rejection::Invalid(_)));
        }
        store.close().await;
    }

    #[tokio::test]
    async fn rename_project_keeps_the_key_and_writes_one_event() {
        let (_dir, store) = temp_store();
        let created = new_project(&store, "TRK").await;
        let renamed = store
            .write(|tx| rename_project(tx, 2000, "TRK", "  Tracker "))
            .await
            .unwrap();
        assert_eq!(
            (renamed.key.as_str(), renamed.name.as_str()),
            ("TRK", "Tracker")
        );
        let events = events(&store).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].0, "project.updated");
        assert_eq!(events[1].4["name"], "Tracker");
        assert_eq!(events[1].4["previous_name"], "Name");
        assert_eq!(events[1].2, created.project.id);

        store
            .write(|tx| rename_project(tx, 3000, "TRK", "Tracker"))
            .await
            .unwrap();
        assert_eq!(events_len(&store).await, 2, "no change, no event");

        let missing = store
            .write(|tx| rename_project(tx, 1, "NOPE", "x"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(missing), Rejection::NotFound(_)));
        let missing = store.read(|c| get_project(c, "NOPE")).await.unwrap_err();
        assert!(matches!(rejection(missing), Rejection::NotFound(_)));
        store.close().await;
    }

    async fn events_len(store: &Store) -> usize {
        events(store).await.len()
    }

    #[tokio::test]
    async fn column_create_rename_delete_and_refusals() {
        let (_dir, store) = temp_store();
        let project = new_project(&store, "TRK").await;
        let before = events_len(&store).await;

        let review = store
            .write(|tx| create_column(tx, 5, "TRK", "Review", StateType::Started))
            .await
            .unwrap();
        assert_eq!(review.position, 5);
        let ev = events(&store).await;
        assert_eq!(ev.len(), before + 1);
        let last = ev.last().unwrap();
        assert_eq!(last.0, "column.created");
        assert_eq!((last.1.as_str(), last.2), ("column", review.id));
        assert_eq!(last.3, Some(project.project.id));
        assert_eq!(last.4["state_type"], "started");

        let dup = store
            .write(|tx| create_column(tx, 5, "TRK", "Review", StateType::Started))
            .await
            .unwrap_err();
        assert!(matches!(rejection(dup), Rejection::Conflict(_)));
        let bad = store
            .write(|tx| create_column(tx, 5, "TRK", " ", StateType::Started))
            .await
            .unwrap_err();
        assert!(matches!(rejection(bad), Rejection::Invalid(_)));
        assert_eq!(
            events_len(&store).await,
            before + 1,
            "refusals write nothing"
        );

        let id = review.id;
        let renamed = store
            .write(move |tx| rename_column(tx, 6, "TRK", id, "QA"))
            .await
            .unwrap();
        assert_eq!(renamed.name, "QA");
        assert_eq!(renamed.state_type, StateType::Started);
        let ev = events(&store).await;
        assert_eq!(ev.last().unwrap().0, "column.renamed");
        assert_eq!(ev.last().unwrap().4["previous_name"], "Review");

        let clash = store
            .write(move |tx| rename_column(tx, 6, "TRK", id, "Done"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(clash), Rejection::Conflict(_)));

        store
            .write(move |tx| delete_column(tx, 7, "TRK", id))
            .await
            .unwrap();
        assert_eq!(event_kinds(&store).await.last().unwrap(), "column.deleted");
        let gone = store
            .write(move |tx| delete_column(tx, 7, "TRK", id))
            .await
            .unwrap_err();
        assert!(matches!(rejection(gone), Rejection::NotFound(_)));
        store.close().await;
    }

    #[tokio::test]
    async fn column_of_another_project_is_not_found() {
        let (_dir, store) = temp_store();
        let a = new_project(&store, "AA").await;
        new_project(&store, "BB").await;
        let id = a.columns[0].id;
        let err = store
            .write(move |tx| rename_column(tx, 1, "BB", id, "X"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::NotFound(_)));
        let err = store
            .write(move |tx| delete_column(tx, 1, "BB", id))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::NotFound(_)));
        store.close().await;
    }

    #[tokio::test]
    async fn columns_are_selected_by_state_type() {
        let (_dir, store) = temp_store();
        new_project(&store, "TRK").await;
        store
            .write(|tx| create_column(tx, 1, "TRK", "Review", StateType::Started))
            .await
            .unwrap();
        let started = store
            .read(|c| columns_by_state_type(c, "TRK", StateType::Started))
            .await
            .unwrap();
        let names: Vec<_> = started.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["In Progress", "Review"]);
        let completed = store
            .read(|c| columns_by_state_type(c, "TRK", StateType::Completed))
            .await
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].name, "Done");
        let err = store
            .read(|c| columns_by_state_type(c, "NOPE", StateType::Started))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::NotFound(_)));
        store.close().await;
    }

    #[tokio::test]
    async fn reorder_rewrites_positions_and_rejects_a_wrong_id_set() {
        let (_dir, store) = temp_store();
        let project = new_project(&store, "TRK").await;
        let ids: Vec<i64> = project.columns.iter().map(|c| c.id).collect();
        let mut reversed = ids.clone();
        reversed.reverse();

        let want = reversed.clone();
        let result = store
            .write(move |tx| reorder_columns(tx, 9, "TRK", &want))
            .await
            .unwrap();
        assert_eq!(result.iter().map(|c| c.id).collect::<Vec<_>>(), reversed);
        assert_eq!(
            result.iter().map(|c| c.position).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        let ev = events(&store).await;
        assert_eq!(ev.last().unwrap().0, "column.reordered");
        assert_eq!(ev.last().unwrap().4["ids"], json!(reversed));
        let count = ev.len();

        let same = reversed.clone();
        store
            .write(move |tx| reorder_columns(tx, 9, "TRK", &same))
            .await
            .unwrap();
        assert_eq!(events_len(&store).await, count, "same order, no event");

        let mut missing = reversed.clone();
        missing.pop();
        let mut extra = reversed.clone();
        extra.push(9999);
        let mut duplicate = reversed.clone();
        duplicate[0] = duplicate[1];
        for bad in [missing, extra, duplicate, Vec::new()] {
            let err = store
                .write(move |tx| reorder_columns(tx, 9, "TRK", &bad))
                .await
                .unwrap_err();
            assert!(matches!(rejection(err), Rejection::Invalid(_)));
        }
        let after = store.read(|c| list_columns(c, "TRK")).await.unwrap();
        assert_eq!(after.iter().map(|c| c.id).collect::<Vec<_>>(), reversed);
        assert_eq!(events_len(&store).await, count);
        store.close().await;
    }

    async fn insert_issue(
        store: &Store,
        project: i64,
        type_id: i64,
        column_id: i64,
        archived: i64,
    ) {
        store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO issue (project_id, number, type_id, column_id, title, position,
                                        created, updated, archived)
                     VALUES (?1, (SELECT COALESCE(MAX(number), 0) + 1 FROM issue WHERE project_id = ?1),
                             ?2, ?3, 'T', 1.0, 1, 1, ?4)",
                    params![project, type_id, column_id, archived],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deleting_a_column_or_issue_type_in_use_is_refused() {
        let (_dir, store) = temp_store();
        let project = new_project(&store, "TRK").await;
        let bug = store
            .write(|tx| create_issue_type(tx, 1, "TRK", "Bug"))
            .await
            .unwrap();
        let spare = store
            .write(|tx| create_column(tx, 1, "TRK", "Spare", StateType::Started))
            .await
            .unwrap();
        let todo = project.columns[1].id;
        insert_issue(&store, project.project.id, bug.id, todo, 0).await;
        // An archived issue still counts.
        insert_issue(&store, project.project.id, bug.id, spare.id, 1).await;
        let count = events_len(&store).await;

        for id in [todo, spare.id] {
            let err = store
                .write(move |tx| delete_column(tx, 2, "TRK", id))
                .await
                .unwrap_err();
            assert!(
                matches!(rejection(err), Rejection::Conflict(_)),
                "column {id}"
            );
        }
        let bug_id = bug.id;
        let err = store
            .write(move |tx| delete_issue_type(tx, 2, "TRK", bug_id))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Conflict(_)));
        assert_eq!(events_len(&store).await, count, "refusals write no event");

        let columns = store.read(|c| list_columns(c, "TRK")).await.unwrap();
        assert_eq!(columns.len(), 6);

        let free = project.columns[0].id;
        store
            .write(move |tx| delete_column(tx, 2, "TRK", free))
            .await
            .unwrap();
        store.close().await;
    }

    #[tokio::test]
    async fn issue_type_lifecycle() {
        let (_dir, store) = temp_store();
        let project = new_project(&store, "TRK").await;
        new_project(&store, "OTH").await;
        let created = store
            .write(|tx| create_issue_type(tx, 3, "TRK", " Bug "))
            .await
            .unwrap();
        assert_eq!(created.name, "Bug");
        assert_eq!(created.project_id, project.project.id);
        // The same name is fine in another project, not in the same one.
        store
            .write(|tx| create_issue_type(tx, 3, "OTH", "Bug"))
            .await
            .unwrap();
        let dup = store
            .write(|tx| create_issue_type(tx, 3, "TRK", "Bug"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(dup), Rejection::Conflict(_)));
        let bad = store
            .write(|tx| create_issue_type(tx, 3, "TRK", ""))
            .await
            .unwrap_err();
        assert!(matches!(rejection(bad), Rejection::Invalid(_)));

        let id = created.id;
        let renamed = store
            .write(move |tx| rename_issue_type(tx, 4, "TRK", id, "Defect"))
            .await
            .unwrap();
        assert_eq!(renamed.name, "Defect");
        let wrong_project = store
            .write(move |tx| rename_issue_type(tx, 4, "OTH", id, "X"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(wrong_project), Rejection::NotFound(_)));
        let listed = store.read(|c| list_issue_types(c, "TRK")).await.unwrap();
        assert_eq!(listed, [renamed]);

        store
            .write(move |tx| delete_issue_type(tx, 5, "TRK", id))
            .await
            .unwrap();
        assert_eq!(
            event_kinds(&store).await[2..],
            [
                "issue_type.created",
                "issue_type.created",
                "issue_type.renamed",
                "issue_type.deleted"
            ]
        );
        assert!(
            store
                .read(|c| list_issue_types(c, "TRK"))
                .await
                .unwrap()
                .is_empty()
        );
        store.close().await;
    }

    #[tokio::test]
    async fn label_lifecycle_and_cascade() {
        let (_dir, store) = temp_store();
        let project = new_project(&store, "TRK").await;
        let created = store
            .write(|tx| create_label(tx, 3, "TRK", "urgent", "#FF0000"))
            .await
            .unwrap();
        assert_eq!(created.color, "#ff0000", "colors are stored lowercase");

        for bad in ["red", "#ff00", "#gg0000", "ff0000", "#ff00000", ""] {
            let err = store
                .write(move |tx| create_label(tx, 3, "TRK", "other", bad))
                .await
                .unwrap_err();
            assert!(matches!(rejection(err), Rejection::Invalid(_)), "{bad:?}");
        }
        let dup = store
            .write(|tx| create_label(tx, 3, "TRK", "urgent", "#000000"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(dup), Rejection::Conflict(_)));

        let id = created.id;
        let renamed = store
            .write(move |tx| rename_label(tx, 4, "TRK", id, "p0"))
            .await
            .unwrap();
        assert_eq!(renamed.name, "p0");
        let recolored = store
            .write(move |tx| recolor_label(tx, 5, "TRK", id, "#00ff00"))
            .await
            .unwrap();
        assert_eq!(recolored.color, "#00ff00");
        let bad = store
            .write(move |tx| recolor_label(tx, 5, "TRK", id, "green"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(bad), Rejection::Invalid(_)));
        let ev = events(&store).await;
        let kinds: Vec<_> = ev.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            kinds[1..],
            ["label.created", "label.renamed", "label.recolored"]
        );
        assert_eq!(ev[3].4["previous_color"], "#ff0000");

        // Attach it to an issue, delete the label, the link goes, the issue stays.
        let bug = store
            .write(|tx| create_issue_type(tx, 1, "TRK", "Bug"))
            .await
            .unwrap();
        insert_issue(&store, project.project.id, bug.id, project.columns[0].id, 0).await;
        store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO issue_label (issue_id, label_id) SELECT id, ?1 FROM issue",
                    [id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .write(move |tx| delete_label(tx, 6, "TRK", id))
            .await
            .unwrap();
        let (links, issues): (i64, i64) = store
            .read(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM issue_label", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((links, issues), (0, 1));
        let ev = events(&store).await;
        assert_eq!(ev.last().unwrap().0, "label.deleted");
        assert_eq!(ev.last().unwrap().4["issues"], 1);
        store.close().await;
    }

    #[tokio::test]
    async fn a_rolled_back_change_leaves_no_event() {
        let (_dir, store) = temp_store();
        new_project(&store, "TRK").await;
        let before = events_len(&store).await;
        let outcome = store
            .write(|tx| {
                create_column(tx, 1, "TRK", "Review", StateType::Started)?;
                create_label(tx, 1, "TRK", "x", "#123456")?;
                Err::<(), _>(StoreError::Closed)
            })
            .await;
        assert!(matches!(outcome, Err(StoreError::Closed)));
        assert_eq!(events_len(&store).await, before);
        let columns = store.read(|c| list_columns(c, "TRK")).await.unwrap();
        assert_eq!(columns.len(), 5);
        assert!(
            store
                .read(|c| list_labels(c, "TRK"))
                .await
                .unwrap()
                .is_empty()
        );
        store.close().await;
    }

    #[tokio::test]
    async fn every_change_writes_exactly_one_event_of_the_right_kind() {
        let (_dir, store) = temp_store();
        new_project(&store, "TRK").await;
        let column = store
            .write(|tx| create_column(tx, 1, "TRK", "Review", StateType::Started))
            .await
            .unwrap();
        let ids: Vec<i64> = store
            .read(|c| list_columns(c, "TRK"))
            .await
            .unwrap()
            .iter()
            .map(|c| c.id)
            .rev()
            .collect();
        store
            .write(move |tx| reorder_columns(tx, 1, "TRK", &ids))
            .await
            .unwrap();
        let cid = column.id;
        store
            .write(move |tx| rename_column(tx, 1, "TRK", cid, "QA"))
            .await
            .unwrap();
        store
            .write(move |tx| delete_column(tx, 1, "TRK", cid))
            .await
            .unwrap();
        assert_eq!(
            event_kinds(&store).await,
            [
                "project.created",
                "column.created",
                "column.reordered",
                "column.renamed",
                "column.deleted"
            ]
        );
        store.close().await;
    }
}
