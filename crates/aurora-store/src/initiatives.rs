//! Initiatives: a named group of issues of one project, with progress and health.
//!
//! Plain functions over a transaction (writes) or a connection (reads), like
//! [`crate::issues`]. An initiative belongs to one project and is addressed by
//! project key and id; an issue joins one through [`crate::issues::IssuePatch`]
//! (`initiative`) and may only join an initiative of its own project. Every
//! write appends its event in the same transaction.
//!
//! Progress counts the non-archived issues of an initiative by the state type
//! of their column: completed and canceled issues are done. Health compares
//! progress with the time elapsed towards the target date, see [`health_of`].
//! Reads take `now` so tests can fix the clock.

use std::collections::HashMap;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::StoreError;
use crate::auth::Principal;
use crate::documents::{LinkedDocument, documents_of_initiative, unlink_initiative};
use crate::issues::{DEFAULT_LIMIT, MAX_LIMIT, present};
use crate::projects::{StateType, emit, get_project, invalid, not_found, refusal};

pub const MAX_NAME_CHARS: usize = 200;
pub const MAX_DESCRIPTION_CHARS: usize = 20_000;
const DAY_MILLIS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InitiativeStatus {
    Planned,
    Active,
    Paused,
    Completed,
    Canceled,
}

impl InitiativeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InitiativeStatus::Planned => "planned",
            InitiativeStatus::Active => "active",
            InitiativeStatus::Paused => "paused",
            InitiativeStatus::Completed => "completed",
            InitiativeStatus::Canceled => "canceled",
        }
    }

    pub fn parse(text: &str) -> Option<InitiativeStatus> {
        Some(match text {
            "planned" => InitiativeStatus::Planned,
            "active" => InitiativeStatus::Active,
            "paused" => InitiativeStatus::Paused,
            "completed" => InitiativeStatus::Completed,
            "canceled" => InitiativeStatus::Canceled,
            _ => return None,
        })
    }
}

impl ToSql for InitiativeStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for InitiativeStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        InitiativeStatus::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("initiative status {text}").into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    OnTrack,
    AtRisk,
    OffTrack,
    /// No signal: see [`health_of`].
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Initiative {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub description: String,
    pub status: InitiativeStatus,
    /// A calendar date, `YYYY-MM-DD`.
    pub target_date: Option<String>,
    pub created: i64,
    pub updated: i64,
}

/// Counts of the non-archived issues of an initiative.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Progress {
    pub total: i64,
    pub completed: i64,
    pub canceled: i64,
    pub started: i64,
    /// Neither started nor done: backlog and unstarted columns.
    pub open: i64,
    /// `completed + canceled`.
    pub done: i64,
    /// `done / total` in 0..=1; 0 when there are no issues.
    pub done_ratio: f64,
    /// `done * 100 / total`, rounded down, so 100 means everything is done.
    pub percent: i64,
}

impl Progress {
    pub fn from_counts(total: i64, completed: i64, canceled: i64, started: i64) -> Progress {
        let done = completed + canceled;
        let (done_ratio, percent) = if total > 0 {
            (done as f64 / total as f64, done * 100 / total)
        } else {
            (0.0, 0)
        };
        Progress {
            total,
            completed,
            canceled,
            started,
            open: total - done - started,
            done,
            done_ratio,
            percent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InitiativeView {
    #[serde(flatten)]
    pub initiative: Initiative,
    pub progress: Progress,
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitiativeIssue {
    pub number: i64,
    pub identifier: String,
    pub title: String,
    pub column_id: i64,
    pub state_type: StateType,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InitiativeDetail {
    #[serde(flatten)]
    pub view: InitiativeView,
    /// The non-archived issues, ordered by column position, then issue position.
    pub issues: Vec<InitiativeIssue>,
    /// The non-archived documents linked to this initiative.
    pub documents: Vec<LinkedDocument>,
    /// All of them, ignoring `limit` and `offset`.
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewInitiative {
    pub name: String,
    pub description: Option<String>,
    /// Defaults to `planned`.
    pub status: Option<InitiativeStatus>,
    /// `YYYY-MM-DD`.
    pub target_date: Option<String>,
}

/// Only the fields that are present change; `null` clears the description
/// (to empty) and the target date.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitiativePatch {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub description: Option<Option<String>>,
    pub status: Option<InitiativeStatus>,
    #[serde(default, deserialize_with = "present")]
    pub target_date: Option<Option<String>>,
}

const SELECT: &str =
    "SELECT id, project_id, name, description, status, target_date, created, updated
     FROM initiative";

fn initiative_row(row: &Row<'_>) -> rusqlite::Result<Initiative> {
    Ok(Initiative {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        status: row.get(4)?,
        target_date: row.get(5)?,
        created: row.get(6)?,
        updated: row.get(7)?,
    })
}

fn find(conn: &Connection, project_id: i64, id: i64) -> Result<Initiative, StoreError> {
    conn.query_row(
        &format!("{SELECT} WHERE id = ?1 AND project_id = ?2"),
        [id, project_id],
        initiative_row,
    )
    .optional()?
    .ok_or_else(|| not_found("initiative"))
}

// ---- dates ----

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// The date as (year, month, day) when it is a real calendar date `YYYY-MM-DD`.
fn parse_date(text: &str) -> Option<(i64, i64, i64)> {
    let bytes = text.as_bytes();
    let shape_ok = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    if !shape_ok {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => return None,
    };
    ((1970..=9999).contains(&year) && (1..=days_in_month).contains(&day))
        .then_some((year, month, day))
}

/// The instant the target day is over (the next midnight, UTC), in Unix
/// milliseconds; `None` when `text` is not a valid `YYYY-MM-DD` date.
pub fn target_end_millis(text: &str) -> Option<i64> {
    let (year, month, day) = parse_date(text)?;
    Some((days_from_civil(year, month, day) + 1) * DAY_MILLIS)
}

fn clean_target(text: &str) -> Result<String, StoreError> {
    let text = text.trim();
    if parse_date(text).is_some() {
        Ok(text.to_string())
    } else {
        Err(invalid(
            "target_date must be a date written YYYY-MM-DD, such as 2026-12-31",
        ))
    }
}

// ---- validation ----

fn clean_name(name: &str) -> Result<String, StoreError> {
    let name = name.trim();
    if (1..=MAX_NAME_CHARS).contains(&name.chars().count()) {
        Ok(name.to_string())
    } else {
        Err(invalid(format!(
            "initiative name must be 1-{MAX_NAME_CHARS} characters"
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

fn duplicate(name: &str) -> impl FnOnce() -> String + '_ {
    move || format!("the project already has an initiative named {name}")
}

// ---- progress and health ----

/// Counts per initiative id, from one grouped query over the project's
/// non-archived issues; `only` narrows it to one initiative.
fn progress_by_initiative(
    conn: &Connection,
    project_id: i64,
    only: Option<i64>,
) -> Result<HashMap<i64, Progress>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT i.initiative_id, c.state_type, COUNT(*)
         FROM issue i JOIN \"column\" c ON c.id = i.column_id
         WHERE i.project_id = ?1 AND i.archived = 0 AND i.initiative_id IS NOT NULL
           AND (?2 IS NULL OR i.initiative_id = ?2)
         GROUP BY i.initiative_id, c.state_type",
    )?;
    let rows = stmt.query_map(params![project_id, only], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, StateType>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    // (total, completed, canceled, started) per initiative.
    let mut counts: HashMap<i64, [i64; 4]> = HashMap::new();
    for row in rows {
        let (id, state, count) = row?;
        let entry = counts.entry(id).or_default();
        entry[0] += count;
        match state {
            StateType::Completed => entry[1] += count,
            StateType::Canceled => entry[2] += count,
            StateType::Started => entry[3] += count,
            StateType::Backlog | StateType::Unstarted => {}
        }
    }
    Ok(counts
        .into_iter()
        .map(|(id, [total, completed, canceled, started])| {
            (
                id,
                Progress::from_counts(total, completed, canceled, started),
            )
        })
        .collect())
}

fn no_progress() -> Progress {
    Progress::from_counts(0, 0, 0, 0)
}

/// How an initiative is doing against its target date.
///
/// - `none`: no (valid) target date; the status is `completed` or `canceled`;
///   or the status is `planned` and no issue has been added yet.
/// - `off_track`: the target day is over (`now` is at or past the next UTC
///   midnight) and not every issue is done.
/// - `at_risk`: `done_ratio + 0.2` is less than the elapsed fraction of the
///   window from creation to the end of the target day.
/// - `on_track`: otherwise, including everything done after the target day.
///
/// `paused` counts like `active`. The comparison is exact integer arithmetic,
/// so equality (`done_ratio + 0.2 == elapsed`) is `on_track`.
pub fn health_of(
    status: InitiativeStatus,
    target_date: Option<&str>,
    created: i64,
    now: i64,
    progress: &Progress,
) -> Health {
    let Some(end) = target_date.and_then(target_end_millis) else {
        return Health::None;
    };
    match status {
        InitiativeStatus::Completed | InitiativeStatus::Canceled => return Health::None,
        InitiativeStatus::Planned if progress.total == 0 => return Health::None,
        _ => {}
    }
    let all_done = progress.total > 0 && progress.done == progress.total;
    if now >= end && !all_done {
        return Health::OffTrack;
    }
    // elapsed / window, with a window that has no length counted as over.
    let window = end - created;
    let (elapsed, window) = if window > 0 {
        (now.saturating_sub(created).clamp(0, window), window)
    } else {
        (1, 1)
    };
    let total = progress.total.max(1);
    let done = if progress.total == 0 {
        0
    } else {
        progress.done
    };
    // done/total + 1/5 < elapsed/window, multiplied out.
    let behind = i128::from(5 * done + total) * i128::from(window)
        < 5 * i128::from(elapsed) * i128::from(total);
    if behind {
        Health::AtRisk
    } else {
        Health::OnTrack
    }
}

fn view(initiative: Initiative, progress: Progress, now: i64) -> InitiativeView {
    let health = health_of(
        initiative.status,
        initiative.target_date.as_deref(),
        initiative.created,
        now,
        &progress,
    );
    InitiativeView {
        initiative,
        progress,
        health,
    }
}

fn view_of(
    conn: &Connection,
    project_id: i64,
    id: i64,
    now: i64,
) -> Result<InitiativeView, StoreError> {
    let initiative = find(conn, project_id, id)?;
    let progress = progress_by_initiative(conn, project_id, Some(id))?
        .remove(&id)
        .unwrap_or_else(no_progress);
    Ok(view(initiative, progress, now))
}

// ---- reads ----

/// Ordered by id. At most two queries however many initiatives there are.
pub fn list_initiatives(
    conn: &Connection,
    now: i64,
    project_key: &str,
    status: Option<InitiativeStatus>,
) -> Result<Vec<InitiativeView>, StoreError> {
    let project = get_project(conn, project_key)?;
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE project_id = ?1 AND (?2 IS NULL OR status = ?2) ORDER BY id"
    ))?;
    let found = stmt
        .query_map(params![project.id, status], initiative_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut progress = progress_by_initiative(conn, project.id, None)?;
    Ok(found
        .into_iter()
        .map(|initiative| {
            let own = progress.remove(&initiative.id).unwrap_or_else(no_progress);
            view(initiative, own, now)
        })
        .collect())
}

/// The initiative with its progress, health and one page of its issues.
pub fn get_initiative(
    conn: &Connection,
    now: i64,
    project_key: &str,
    id: i64,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<InitiativeDetail, StoreError> {
    let project = get_project(conn, project_key)?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = offset.unwrap_or(0);
    if offset < 0 {
        return Err(invalid("offset must not be negative"));
    }
    let view = view_of(conn, project.id, id, now)?;
    let mut stmt = conn.prepare(
        "SELECT i.number, i.title, i.column_id, c.state_type
         FROM issue i JOIN \"column\" c ON c.id = i.column_id
         WHERE i.initiative_id = ?1 AND i.archived = 0
         ORDER BY c.position, i.position, i.id
         LIMIT ?2 OFFSET ?3",
    )?;
    let issues = stmt
        .query_map(params![id, limit, offset], |row| {
            let number: i64 = row.get(0)?;
            Ok(InitiativeIssue {
                number,
                identifier: format!("{}-{number}", project.key),
                title: row.get(1)?,
                column_id: row.get(2)?,
                state_type: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let documents = documents_of_initiative(conn, id)?;
    Ok(InitiativeDetail {
        total: view.progress.total,
        view,
        issues,
        documents,
        limit,
        offset,
    })
}

// ---- writes ----

pub fn create_initiative(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    new: NewInitiative,
) -> Result<InitiativeView, StoreError> {
    let name = clean_name(&new.name)?;
    let description = new.description.unwrap_or_default();
    check_description(&description)?;
    let status = new.status.unwrap_or(InitiativeStatus::Planned);
    let target = new.target_date.as_deref().map(clean_target).transpose()?;
    let project = get_project(tx, project_key)?;
    tx.execute(
        "INSERT INTO initiative (project_id, name, description, status, target_date, created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![project.id, name, description, status, target, now],
    )
    .map_err(|err| refusal(err, duplicate(&name)))?;
    let id = tx.last_insert_rowid();
    emit(
        tx,
        now,
        actor,
        project.id,
        "initiative",
        id,
        "initiative.created",
        &json!({ "name": name, "status": status, "target_date": target }),
    )?;
    view_of(tx, project.id, id, now)
}

fn changed(changes: &mut Map<String, Value>, field: &str, from: Value, to: Value) {
    changes.insert(field.to_string(), json!({ "from": from, "to": to }));
}

/// A patch that changes nothing writes nothing and emits no event.
pub fn update_initiative(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
    patch: InitiativePatch,
) -> Result<InitiativeView, StoreError> {
    let project = get_project(tx, project_key)?;
    let current = find(tx, project.id, id)?;
    let mut next = current.clone();
    let mut changes = Map::new();

    if let Some(name) = &patch.name {
        next.name = clean_name(name)?;
        if next.name != current.name {
            changed(&mut changes, "name", json!(current.name), json!(next.name));
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
    if let Some(status) = patch.status
        && status != current.status
    {
        next.status = status;
        changed(&mut changes, "status", json!(current.status), json!(status));
    }
    if let Some(target) = &patch.target_date {
        next.target_date = target.as_deref().map(clean_target).transpose()?;
        if next.target_date != current.target_date {
            changed(
                &mut changes,
                "target_date",
                json!(current.target_date),
                json!(next.target_date),
            );
        }
    }

    if changes.is_empty() {
        return view_of(tx, project.id, id, now);
    }
    tx.execute(
        "UPDATE initiative SET name = ?1, description = ?2, status = ?3, target_date = ?4,
                               updated = ?5
         WHERE id = ?6",
        params![
            next.name,
            next.description,
            next.status,
            next.target_date,
            now,
            id
        ],
    )
    .map_err(|err| refusal(err, duplicate(&next.name)))?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "initiative",
        id,
        "initiative.updated",
        &json!({ "changes": changes }),
    )?;
    view_of(tx, project.id, id, now)
}

/// Unassigns every issue of the initiative (archived ones too), each with its
/// own `issue.updated` event, then deletes the initiative, in the caller's
/// transaction. The issues themselves are kept.
pub fn delete_initiative(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    project_key: &str,
    id: i64,
) -> Result<(), StoreError> {
    let project = get_project(tx, project_key)?;
    let initiative = find(tx, project.id, id)?;
    let mut stmt = tx.prepare("SELECT id FROM issue WHERE initiative_id = ?1 ORDER BY id")?;
    let assigned = stmt
        .query_map([id], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for issue_id in &assigned {
        tx.execute(
            "UPDATE issue SET initiative_id = NULL, updated = ?1 WHERE id = ?2",
            params![now, issue_id],
        )?;
        emit(
            tx,
            now,
            actor,
            project.id,
            "issue",
            *issue_id,
            "issue.updated",
            &json!({ "changes": { "initiative": { "from": id, "to": null } } }),
        )?;
    }
    unlink_initiative(tx, now, actor, project.id, id)?;
    tx.execute("DELETE FROM initiative WHERE id = ?1", [id])?;
    emit(
        tx,
        now,
        actor,
        project.id,
        "initiative",
        id,
        "initiative.deleted",
        &json!({ "name": initiative.name, "unassigned": assigned.len() }),
    )
}
