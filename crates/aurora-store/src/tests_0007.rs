//! Migration 0007 (comments) applied to a populated version-6 database.

use rusqlite::Connection;

use crate::auth::{Principal, Role, Scope};
use crate::comments::{self, NewComment};
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_six_database_gains_comments_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v6 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..6]).unwrap();
    assert_eq!(v6.schema_version(), 6);
    v6.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES (1, 'Owner', 'human', 'owner'),
                                                            (2, 'robot', 'agent', 'agent');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 3);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES
                 (1, 1, 'Todo', 'unstarted', 0), (2, 1, 'Done', 'completed', 1);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1), (2, 1, 2, 1, 1, 'two', 2.0, 1, 1);
             INSERT INTO issue_link (from_issue_id, to_issue_id, kind, created) VALUES (1, 2, 'blocks', 3);
             INSERT INTO event (time, project_id, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 1, 'issue', 1, 'issue.created', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v6.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), crate::latest_version());
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let none = store
        .read(|c| comments::list_comments(c, "AA", 1))
        .await
        .unwrap();
    assert!(none.is_empty());
    let owner = Principal {
        user_id: 1,
        role: Role::Owner,
        scope: Scope::Write,
    };
    let added = store
        .write(move |tx| {
            comments::add_comment(
                tx,
                20,
                &owner,
                "AA",
                2,
                NewComment {
                    body: "hello @robot".into(),
                    parent: None,
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(added.mentions.len(), 1);
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM issue_link"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comment"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comment_mention"), 1);
    // The old event, plus comment.created and one mention.created.
    assert_eq!(count("SELECT COUNT(*) FROM event"), 3);

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('comment', 'comment_mention')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-6 database");
    let links: i64 = old
        .query_row("SELECT COUNT(*) FROM issue_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 1);
}
