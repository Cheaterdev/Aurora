//! Migration 0005 (users, sessions, API keys) applied to a populated version-4 database.

use rusqlite::Connection;

use crate::auth::{self, Role, Scope};
use crate::events;
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_four_database_gains_auth_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v4 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..4]).unwrap();
    assert_eq!(v4.schema_version(), 4);
    v4.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, email, kind) VALUES
                 (1, 'Old Human', 'old@example.com', 'human'),
                 (2, 'Old Agent', NULL, 'agent');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 2);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES (1, 1, 'Todo', 'unstarted', 0);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated, assignee_id)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1, 2);
             INSERT INTO event (time, project_id, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 1, 'issue', 1, 'issue.created', '{}');
             INSERT INTO consumer_cursor (name, last_event_id, updated) VALUES ('runner', 1, 6);",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v4.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 5);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    // Existing users keep their rows; agents get the agent role, humans the member role.
    let users = store.read(auth::list_users).await.unwrap();
    let seen: Vec<_> = users
        .iter()
        .map(|u| (u.id, u.name.as_str(), u.role))
        .collect();
    assert_eq!(
        seen,
        [
            (1, "Old Human", Role::Member),
            (2, "Old Agent", Role::Agent)
        ]
    );
    // Nobody from before can sign in (no password), and a first owner can still be set up.
    let err = store
        .write(|tx| auth::sign_in(tx, 1, "Old Human", "whatever password").map(|_| ()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid name or password"));
    let hash = auth::hash_password("a fine password").unwrap();
    let owner = store
        .write(move |tx| auth::create_owner(tx, 7, "Owner", &hash))
        .await
        .unwrap();
    assert_eq!(owner.id, 3);

    // The new tables work against the migrated data.
    let actor = auth::Principal {
        user_id: owner.id,
        role: Role::Owner,
        scope: Scope::Write,
    };
    let key = store
        .write(move |tx| auth::create_key(tx, 8, &actor, 2, "legacy agent", Scope::Read))
        .await
        .unwrap();
    let found = store
        .read(move |c| auth::authenticate_key(c, 9, &key.key))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.principal.user_id, 2);

    // Events from before the migration are still there, and the log kept growing.
    let pending = store
        .read(|c| events::poll(c, "runner", None, &events::EventFilter::default()))
        .await
        .unwrap();
    assert_eq!(
        pending.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["user.created", "key.created"],
        "the runner cursor was at the pre-migration event; later ones follow"
    );
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM issue WHERE assignee_id = 2"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM project"), 1);

    let old = Connection::open(&backup).unwrap();
    let columns: Vec<String> = old
        .prepare("SELECT name FROM pragma_table_info('user')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        !columns.contains(&"role".to_string()),
        "the backup is the version-4 database"
    );
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('session', 'api_key')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
    let legacy_users: i64 = old
        .query_row("SELECT COUNT(*) FROM user", [], |r| r.get(0))
        .unwrap();
    assert_eq!(legacy_users, 2);
}

#[tokio::test]
async fn fresh_database_has_the_auth_schema() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let names: Vec<String> = store
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT name FROM sqlite_master
                 WHERE name IN ('session', 'api_key', 'user_single_owner', 'user_name_unique')
                 ORDER BY name",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(
        names,
        [
            "api_key",
            "session",
            "user_name_unique",
            "user_single_owner"
        ]
    );
    store.close().await;
}
