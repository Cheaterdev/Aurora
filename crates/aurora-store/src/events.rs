//! The append-only event log and the cursors of the consumers that read it.
//!
//! Events are written by the other store modules in the same transaction as
//! the change they describe and are never edited or deleted (triggers enforce
//! it). `event.id` is `AUTOINCREMENT`, so ids only grow and are never reused;
//! because all writes go through the single writer, id order is commit order.
//!
//! # Consumers: at-least-once delivery
//!
//! A consumer (webhook sender, runner, extension, live page) has a named
//! cursor stored in the database. It [`poll`]s the events after its cursor,
//! processes them, then [`ack`]s the last id it finished. `poll` never moves
//! the cursor. A crash between processing and `ack` delivers the same events
//! again after the restart, so consumers must be idempotent. In return no
//! event is ever skipped: the cursor only advances past events the consumer
//! acknowledged.

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::Serialize;
use serde_json::Value;

use crate::StoreError;
use crate::projects::{get_project, invalid, not_found};

pub const DEFAULT_LIMIT: i64 = 100;
pub const MAX_LIMIT: i64 = 500;
pub const MAX_NAME_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub id: i64,
    pub project_id: Option<i64>,
    /// The key of `project_id`.
    pub project: Option<String>,
    pub kind: String,
    pub entity_kind: String,
    pub entity_id: i64,
    pub actor: Option<i64>,
    pub payload: Value,
    /// Unix milliseconds.
    pub created: i64,
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// Project key.
    pub project: Option<String>,
    pub entity_kind: Option<String>,
    /// Matches kinds that start with this text, e.g. `issue.`.
    pub kind_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cursor {
    pub name: String,
    pub last_event_id: i64,
    pub updated: i64,
}

/// Where a new consumer starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// It will see every event ever written.
    Beginning,
    /// It will see only events written after it registered.
    Latest,
}

fn event_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let payload: String = row.get(8)?;
    let payload = serde_json::from_str(&payload)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(err)))?;
    Ok(Event {
        id: row.get(0)?,
        project_id: row.get(1)?,
        project: row.get(2)?,
        kind: row.get(3)?,
        entity_kind: row.get(4)?,
        entity_id: row.get(5)?,
        actor: row.get(6)?,
        created: row.get(7)?,
        payload,
    })
}

fn cursor_row(row: &Row<'_>) -> rusqlite::Result<Cursor> {
    Ok(Cursor {
        name: row.get(0)?,
        last_event_id: row.get(1)?,
        updated: row.get(2)?,
    })
}

/// The id of the newest event, 0 when there are none.
pub fn latest_event_id(conn: &Connection) -> Result<i64, StoreError> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM event", [], |row| {
            row.get(0)
        })?,
    )
}

/// Events with `id > after_id`, oldest first. `limit` defaults to 100 and is
/// clamped to 1..=500. An unknown project key in the filter is `NotFound`.
pub fn read_events(
    conn: &Connection,
    after_id: i64,
    limit: Option<i64>,
    filter: &EventFilter,
) -> Result<Vec<Event>, StoreError> {
    if after_id < 0 {
        return Err(invalid("after must not be negative"));
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut conditions = vec!["e.id > ?".to_string()];
    let mut values = vec![rusqlite::types::Value::Integer(after_id)];
    if let Some(key) = &filter.project {
        let project = get_project(conn, key)?;
        conditions.push("e.project_id = ?".into());
        values.push(rusqlite::types::Value::Integer(project.id));
    }
    if let Some(entity_kind) = &filter.entity_kind {
        conditions.push("e.entity_kind = ?".into());
        values.push(rusqlite::types::Value::Text(entity_kind.clone()));
    }
    if let Some(prefix) = &filter.kind_prefix {
        // substr, not LIKE: the prefix may contain % or _.
        conditions.push("substr(e.kind, 1, length(?)) = ?".into());
        values.push(rusqlite::types::Value::Text(prefix.clone()));
        values.push(rusqlite::types::Value::Text(prefix.clone()));
    }
    values.push(rusqlite::types::Value::Integer(limit));
    let mut stmt = conn.prepare(&format!(
        "SELECT e.id, e.project_id, p.key, e.kind, e.entity_kind, e.entity_id, e.actor_id,
                e.time, e.payload_json
         FROM event e LEFT JOIN project p ON p.id = e.project_id
         WHERE {} ORDER BY e.id LIMIT ?",
        conditions.join(" AND ")
    ))?;
    let rows = stmt.query_map(params_from_iter(values.iter()), event_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

// ---- consumers ----

/// 1-100 characters: ASCII letters, digits, `.`, `_` and `-`.
pub fn validate_name(name: &str) -> Result<(), StoreError> {
    let ok = (1..=MAX_NAME_CHARS).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "consumer name must be 1-{MAX_NAME_CHARS} characters: letters, digits, '.', '_' or '-'"
        )))
    }
}

pub fn get_cursor(conn: &Connection, name: &str) -> Result<Cursor, StoreError> {
    conn.query_row(
        "SELECT name, last_event_id, updated FROM consumer_cursor WHERE name = ?1",
        [name],
        cursor_row,
    )
    .optional()?
    .ok_or_else(|| not_found("consumer"))
}

pub fn list_consumers(conn: &Connection) -> Result<Vec<Cursor>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT name, last_event_id, updated FROM consumer_cursor ORDER BY name")?;
    let rows = stmt.query_map([], cursor_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Creates the consumer if it does not exist; an existing one keeps its cursor
/// and `start` is ignored. Returns the cursor and whether it was created.
pub fn register_consumer(
    tx: &Transaction<'_>,
    now: i64,
    name: &str,
    start: Start,
) -> Result<(Cursor, bool), StoreError> {
    validate_name(name)?;
    let first = match start {
        Start::Beginning => 0,
        Start::Latest => latest_event_id(tx)?,
    };
    let inserted = tx.execute(
        "INSERT INTO consumer_cursor (name, last_event_id, updated) VALUES (?1, ?2, ?3)
         ON CONFLICT (name) DO NOTHING",
        params![name, first, now],
    )?;
    Ok((get_cursor(tx, name)?, inserted == 1))
}

pub fn delete_consumer(tx: &Transaction<'_>, name: &str) -> Result<(), StoreError> {
    let removed = tx.execute("DELETE FROM consumer_cursor WHERE name = ?1", [name])?;
    if removed == 0 {
        return Err(not_found("consumer"));
    }
    Ok(())
}

/// The events after the consumer's stored cursor. Does not advance it.
pub fn poll(
    conn: &Connection,
    name: &str,
    limit: Option<i64>,
    filter: &EventFilter,
) -> Result<Vec<Event>, StoreError> {
    let cursor = get_cursor(conn, name)?;
    read_events(conn, cursor.last_event_id, limit, filter)
}

/// Moves the cursor to `up_to_id`, meaning every event up to and including it
/// is done. Acknowledging an id at or behind the cursor changes nothing; an id
/// beyond the newest event is refused.
///
/// With a filtered poll, ack the id of the last event the consumer handled or
/// skipped, not just the last one it was shown: events its filter hides are
/// passed over together with it.
pub fn ack(
    tx: &Transaction<'_>,
    now: i64,
    name: &str,
    up_to_id: i64,
) -> Result<Cursor, StoreError> {
    let current = get_cursor(tx, name)?;
    if up_to_id < 0 {
        return Err(invalid("up_to must not be negative"));
    }
    if up_to_id > latest_event_id(tx)? {
        return Err(invalid(format!(
            "event {up_to_id} does not exist yet; cannot acknowledge beyond the newest event"
        )));
    }
    if up_to_id <= current.last_event_id {
        return Ok(current);
    }
    tx.execute(
        "UPDATE consumer_cursor SET last_event_id = ?1, updated = ?2 WHERE name = ?3",
        params![up_to_id, now, name],
    )?;
    get_cursor(tx, name)
}
