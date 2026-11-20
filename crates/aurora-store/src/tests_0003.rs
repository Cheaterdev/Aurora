//! Migration 0003 (issue priority) applied to a populated version-2 database.

use rusqlite::Connection;

use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_two_database_gains_priority_and_keeps_its_issues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v2 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..2]).unwrap();
    assert_eq!(v2.schema_version(), 2);
    v2.write(|tx| {
        tx.execute_batch(
            "INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 3);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES (1, 1, 'Todo', 'unstarted', 0);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO label (id, project_id, name, color) VALUES (1, 1, 'x', '#000000');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1), (2, 1, 2, 1, 1, 'two', 2.0, 2, 2);
             INSERT INTO issue_label (issue_id, label_id) VALUES (1, 1);
             INSERT INTO event (time, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 'issue', 1, 'issue.created', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v2.close().await;

    let store =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..3]).unwrap();
    assert_eq!(store.schema_version(), 3);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM issue_label"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM event"), 1);
    assert_eq!(
        count("SELECT COUNT(*) FROM issue WHERE priority IS NULL"),
        2,
        "existing issues have no priority"
    );
    let titles: Vec<String> = conn
        .prepare("SELECT title FROM issue ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(titles, ["one", "two"]);

    conn.execute("UPDATE issue SET priority = 'urgent' WHERE id = 1", [])
        .unwrap();
    assert!(
        conn.execute("UPDATE issue SET priority = 'whenever' WHERE id = 2", [])
            .is_err(),
        "the CHECK constraint is in place"
    );
    let violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);

    let old = Connection::open(&backup).unwrap();
    let has_priority: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('issue') WHERE name = 'priority'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_priority, 0, "the backup is the version-2 database");
    let old_issues: i64 = old
        .query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0))
        .unwrap();
    assert_eq!(old_issues, 2);
}
