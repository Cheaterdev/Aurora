//! Migration 0008 (initiatives) applied to a populated version-7 database.

use rusqlite::Connection;

use crate::auth::{Principal, Role, Scope};
use crate::initiatives::{self, NewInitiative};
use crate::issues::{self, IssuePatch};
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_seven_database_gains_initiatives_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v7 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..7]).unwrap();
    assert_eq!(v7.schema_version(), 7);
    v7.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES (1, 'Owner', 'human', 'owner');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 3);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES
                 (1, 1, 'Todo', 'unstarted', 0), (2, 1, 'Done', 'completed', 1);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1), (2, 1, 2, 1, 2, 'two', 2.0, 1, 1);
             INSERT INTO comment (issue_id, author_id, body, created) VALUES (1, 1, 'hello', 3);
             INSERT INTO event (time, project_id, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 1, 'issue', 1, 'issue.created', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v7.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), crate::latest_version());
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let owner = Principal {
        user_id: 1,
        role: Role::Owner,
        scope: Scope::Write,
    };
    let made = store
        .write(move |tx| {
            let made = initiatives::create_initiative(
                tx,
                20,
                &owner,
                "AA",
                NewInitiative {
                    name: "Launch".into(),
                    ..NewInitiative::default()
                },
            )?;
            for number in [1, 2] {
                issues::update_issue(
                    tx,
                    21,
                    &owner,
                    "AA",
                    number,
                    IssuePatch {
                        initiative: Some(Some(made.initiative.id)),
                        ..IssuePatch::default()
                    },
                )?;
            }
            Ok(made)
        })
        .await
        .unwrap();
    let detail = store
        .read(move |c| initiatives::get_initiative(c, 30, "AA", made.initiative.id, None, None))
        .await
        .unwrap();
    // The existing issues came through unassigned and are now counted: one done of two.
    assert_eq!(
        (detail.view.progress.total, detail.view.progress.done),
        (2, 1)
    );
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM comment"), 1);
    // The old event, initiative.created and two issue.updated.
    assert_eq!(count("SELECT COUNT(*) FROM event"), 4);

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'initiative'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-7 database");
    let columns: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('issue') WHERE name = 'initiative_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(columns, 0);
}
