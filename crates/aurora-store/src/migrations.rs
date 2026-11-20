use rusqlite::{Connection, params};

use crate::StoreError;

#[derive(Clone, Copy)]
pub(crate) struct Migration {
    pub(crate) version: i64,
    pub(crate) name: &'static str,
    pub(crate) sql: &'static str,
}

/// Append new migrations at the end with the next version number. Applied
/// migrations are never edited: the schema only moves forward.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: include_str!("../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "project_catalog",
        sql: include_str!("../migrations/0002_project_catalog.sql"),
    },
    Migration {
        version: 3,
        name: "issue_priority",
        sql: include_str!("../migrations/0003_issue_priority.sql"),
    },
    Migration {
        version: 4,
        name: "consumer_cursor",
        sql: include_str!("../migrations/0004_consumer_cursor.sql"),
    },
    Migration {
        version: 5,
        name: "auth",
        sql: include_str!("../migrations/0005_auth.sql"),
    },
    Migration {
        version: 6,
        name: "issue_links",
        sql: include_str!("../migrations/0006_issue_links.sql"),
    },
];

/// The schema version this binary produces and understands.
pub fn latest_version() -> i64 {
    latest_of(MIGRATIONS)
}

pub(crate) fn latest_of(migrations: &[Migration]) -> i64 {
    migrations.last().map_or(0, |m| m.version)
}

/// The version recorded in the database (0 when it has none yet). Refuses a
/// database newer than `migrations`. Writes nothing.
pub(crate) fn recorded_version(
    conn: &Connection,
    migrations: &[Migration],
) -> Result<i64, StoreError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
        [],
        |row| row.get(0),
    )?;
    let current: i64 = if has_table {
        conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    let latest = latest_of(migrations);
    if current > latest {
        return Err(StoreError::DatabaseTooNew {
            found: current,
            supported: latest,
        });
    }
    Ok(current)
}

/// Brings the database from `current` (see [`recorded_version`]) up to the
/// last of `migrations` and returns that version. Each migration runs in its
/// own transaction together with its bookkeeping row.
pub(crate) fn migrate(
    conn: &mut Connection,
    migrations: &[Migration],
    current: i64,
) -> Result<i64, StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        )",
    )?;
    let latest = latest_of(migrations);

    for migration in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        #[cfg(test)]
        crate::tests_support::abort_if_requested(migration.version);
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
