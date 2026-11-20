//! Migration 0006 (issue links) applied to a populated version-5 database.

use rusqlite::Connection;

use crate::auth::{Principal, Role, Scope};
use crate::links::{self, LinkKind};
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_five_database_gains_links_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v5 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..5]).unwrap();
    assert_eq!(v5.schema_version(), 5);
    v5.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES (1, 'Owner', 'human', 'owner');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 3);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES
                 (1, 1, 'Todo', 'unstarted', 0), (2, 1, 'Done', 'completed', 1);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1), (2, 1, 2, 1, 1, 'two', 2.0, 1, 1);
             INSERT INTO event (time, project_id, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 1, 'issue', 1, 'issue.created', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v5.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 6);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    // Existing issues are unblocked, and links work against them.
    let before = store
        .read(|c| crate::issues::get_issue(c, "AA", 2))
        .await
        .unwrap();
    assert!(!before.issue.blocked);
    assert!(before.links.blocked_by.is_empty());
    let owner = Principal {
        user_id: 1,
        role: Role::Owner,
        scope: Scope::Write,
    };
    store
        .write(move |tx| links::add_link(tx, 20, &owner, "AA", 1, 2, LinkKind::Blocks))
        .await
        .unwrap();
    let after = store
        .read(|c| crate::issues::get_issue(c, "AA", 2))
        .await
        .unwrap();
    assert!(after.issue.blocked);
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM issue_link"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM event"), 2);

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'issue_link'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-5 database");
    let issues: i64 = old
        .query_row("SELECT COUNT(*) FROM issue", [], |r| r.get(0))
        .unwrap();
    assert_eq!(issues, 2);
}
