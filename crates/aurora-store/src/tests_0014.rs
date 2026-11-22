//! Migration 0014 (runs) applied to a populated version-13 database.

use rusqlite::Connection;

use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_thirteen_database_gets_the_run_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let v13 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..13]).unwrap();
    assert_eq!(v13.schema_version(), 13);
    v13.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES (1, 'Owner', 'human', 'owner');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 1);",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v13.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 15);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));
    let (projects, runs, indexes) = store
        .read(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM project", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM run", [], |r| r.get::<_, i64>(0))?,
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name IN
                     ('run_once_per_trigger', 'run_one_active_per_issue')",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((projects, runs, indexes), (1, 0, 2));
    store.close().await;

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'run'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-13 database");
}
