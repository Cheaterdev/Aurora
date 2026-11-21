//! Migration 0002 (per-project issue types, labels) applied to a populated
//! version-1 database.

use rusqlite::Connection;

use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir, latest_version};

fn names(conn: &Connection, sql: &str) -> Vec<(i64, i64, String)> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test]
async fn fresh_database_reaches_the_latest_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(latest_version(), 14);
    assert_eq!(store.schema_version(), 14);
    assert!(store.open_report().backup.is_none());
    store.close().await;
}

#[tokio::test]
async fn populated_version_one_database_migrates_without_losing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v1 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..1]).unwrap();
    assert_eq!(v1.schema_version(), 1);
    v1.write(|tx| {
        tx.execute_batch(
            "INSERT INTO project (id, key, name, created, next_issue_number) VALUES
                 (1, 'AA', 'First', 10, 1), (2, 'BB', 'Second', 20, 3);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES
                 (1, 1, 'Todo', 'unstarted', 0), (2, 2, 'Todo', 'unstarted', 0);
             INSERT INTO issue_type (id, name) VALUES (1, 'Bug'), (2, 'Epic'), (3, 'Spare');
             -- Bug is first used in project 2, Epic in project 1, Spare by nobody.
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (1, 2, 1, 1, 2, 'one', 1.0, 1, 1),
                        (2, 1, 1, 1, 1, 'two', 2.0, 2, 2),
                        (3, 1, 2, 2, 1, 'three', 3.0, 3, 3);
             INSERT INTO event (time, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 'issue', 1, 'issue.created', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v1.close().await;

    let store =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..2]).unwrap();
    assert_eq!(store.schema_version(), 2);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let violations: Vec<String> = conn
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(violations.is_empty(), "foreign_key_check: {violations:?}");
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");

    assert_eq!(
        names(
            &conn,
            "SELECT id, project_id, name FROM issue_type ORDER BY id"
        ),
        [
            (1, 2, "Bug".to_string()),
            (2, 1, "Epic".to_string()),
            (3, 1, "Spare".to_string()),
        ]
    );
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM project"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM \"column\""), 2);
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 3);
    assert_eq!(count("SELECT COUNT(*) FROM event"), 1);
    assert_eq!(
        count("SELECT COUNT(*) FROM issue i JOIN issue_type t ON t.id = i.type_id"),
        3,
        "issues still resolve their type"
    );
    assert_eq!(count("SELECT COUNT(*) FROM label"), 0);
    assert_eq!(count("SELECT COUNT(*) FROM issue_label"), 0);

    // The issue -> issue_type reference still points at the rebuilt table and is enforced.
    let parent: String = conn
        .query_row(
            "SELECT \"table\" FROM pragma_foreign_key_list('issue') WHERE \"from\" = 'type_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parent, "issue_type");
    assert!(
        conn.execute("DELETE FROM issue_type WHERE id = 1", [])
            .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO issue_type (project_id, name) VALUES (1, 'Epic')",
            []
        )
        .is_err(),
        "names are unique per project"
    );
    conn.execute(
        "INSERT INTO issue_type (project_id, name) VALUES (2, 'Epic')",
        [],
    )
    .unwrap();

    let old = Connection::open(&backup).unwrap();
    let has_project_id: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('issue_type') WHERE name = 'project_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_project_id, 0, "the backup is the version-1 database");
    let old_issues: i64 = old
        .query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0))
        .unwrap();
    assert_eq!(old_issues, 3);
}
