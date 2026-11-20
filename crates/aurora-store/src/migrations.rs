use rusqlite::{Connection, params};

use crate::StoreError;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// Append new migrations at the end with the next version number. Applied
/// migrations are never edited: the schema only moves forward.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial",
    sql: include_str!("../migrations/0001_initial.sql"),
}];

/// The schema version this binary produces and understands.
pub fn latest_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

/// Brings the database up to `latest_version()` and returns that version.
/// Each migration runs in its own transaction together with its bookkeeping row.
pub(crate) fn migrate(conn: &mut Connection) -> Result<i64, StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        )",
    )?;

    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    let latest = latest_version();
    if current > latest {
        return Err(StoreError::DatabaseTooNew {
            found: current,
            supported: latest,
        });
    }

    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_version (version, name, applied_at)
             VALUES (?1, ?2, CAST(unixepoch('subsec') * 1000 AS INTEGER))",
            params![migration.version, migration.name],
        )?;
        tx.commit()?;
        tracing::info!(
            version = migration.version,
            name = migration.name,
            "applied migration"
        );
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_contiguous_from_one() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version, i as i64 + 1);
        }
    }
}
