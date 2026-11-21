//! Migration 0012 (request log) applied to a populated version-11 database.

use rusqlite::Connection;

use crate::migrations::MIGRATIONS;
use crate::request_log::{self, Reply, RequestKey, Run};
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_eleven_database_gets_an_empty_request_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v11 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..11]).unwrap();
    assert_eq!(v11.schema_version(), 11);
    v11.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES
                 (1, 'Owner', 'human', 'owner'),
                 (2, 'old-bot', 'agent', 'agent');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 1);",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v11.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 12);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let (users, projects, log, index) = store
        .read(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM project", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM request_log", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'request_log_created'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((users, projects, log, index), (2, 1, 0, 1));

    // The table works on the migrated database.
    let key = RequestKey::new(1, "request-0001", &[b"x"]).unwrap();
    let first = store
        .write(move |tx| {
            request_log::run(tx, Some(&key), 100, request_log::DEFAULT_RETENTION, || {
                Ok(Reply::new(201, "{}".into()))
            })
        })
        .await
        .unwrap();
    assert!(matches!(first, Run::Fresh(_)));
    store.close().await;

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'request_log'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-11 database");
    let projects: i64 = old
        .query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))
        .unwrap();
    assert_eq!(projects, 1);
}
