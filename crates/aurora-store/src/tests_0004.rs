//! Migration 0004 (consumer cursors) applied to a populated version-3 database.

use rusqlite::Connection;

use crate::events::{self, Start};
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_three_database_gains_cursors_and_keeps_its_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v3 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..3]).unwrap();
    assert_eq!(v3.schema_version(), 3);
    v3.write(|tx| {
        tx.execute_batch(
            "INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 2);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES (1, 1, 'Todo', 'unstarted', 0);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated, priority)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1, 'high');
             INSERT INTO event (time, project_id, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 1, 'issue', 1, 'issue.created', '{}'), (6, 1, 'issue', 1, 'issue.moved', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v3.close().await;

    let store =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..4]).unwrap();
    assert_eq!(store.schema_version(), 4);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let (cursor, created) = store
        .write(|tx| events::register_consumer(tx, 7, "runner", Start::Beginning))
        .await
        .unwrap();
    assert!(created);
    assert_eq!(cursor.last_event_id, 0);
    let pending = store
        .read(|c| events::poll(c, "runner", None, &events::EventFilter::default()))
        .await
        .unwrap();
    assert_eq!(
        pending.iter().map(|e| e.id).collect::<Vec<_>>(),
        [1, 2],
        "events from before the migration are readable"
    );
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM event"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 1);
    assert!(
        conn.execute(
            "INSERT INTO consumer_cursor (name, updated) VALUES ('', 1)",
            []
        )
        .is_err(),
        "the name CHECK is in place"
    );

    let old = Connection::open(&backup).unwrap();
    let has_table: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'consumer_cursor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_table, 0, "the backup is the version-3 database");
}
