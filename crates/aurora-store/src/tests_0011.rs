//! Migration 0011 (agent registry) applied to a populated version-10 database.

use rusqlite::Connection;

use crate::agents::{self, NewAgent};
use crate::auth::{Principal, Role, Scope};
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_ten_database_gets_an_empty_registry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v10 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..10]).unwrap();
    assert_eq!(v10.schema_version(), 10);
    v10.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES
                 (1, 'Owner', 'human', 'owner'),
                 (2, 'old-bot', 'agent', 'agent'),
                 (3, 'colleague', 'human', 'member');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 1);",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v10.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), crate::latest_version());
    assert_eq!(store.schema_version(), 11);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let owner = Principal {
        user_id: 1,
        role: Role::Owner,
        scope: Scope::Write,
    };
    let (users, registered, agents_listed) = store
        .read(move |c| {
            let users: i64 = c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get(0))?;
            let registered: i64 = c.query_row("SELECT COUNT(*) FROM agent", [], |r| r.get(0))?;
            Ok((users, registered, agents::list_agents(c, &owner)?))
        })
        .await
        .unwrap();
    assert_eq!((users, registered), (3, 0), "existing users are untouched");
    assert!(agents_listed.is_empty());

    // The old agent user is valid and can be registered; a human cannot.
    let registered = store
        .write(move |tx| {
            agents::create_agent(
                tx,
                20,
                &owner,
                NewAgent {
                    user_id: Some(2),
                    command: "claude".into(),
                    projects: vec!["AA".into()],
                    ..NewAgent::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(
        (registered.user_id, registered.name.as_str()),
        (2, "old-bot")
    );
    assert_eq!(registered.projects, ["AA"]);
    let human = store
        .write(move |tx| {
            agents::create_agent(
                tx,
                21,
                &owner,
                NewAgent {
                    user_id: Some(3),
                    command: "claude".into(),
                    ..NewAgent::default()
                },
            )
        })
        .await;
    assert!(human.is_err());
    store.close().await;

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('agent', 'agent_project')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-10 database");
}

#[tokio::test]
async fn the_database_itself_refuses_an_agent_row_for_a_human() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let outcome = store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind, role) VALUES (1, 'person', 'human', 'member')",
                [],
            )?;
            tx.execute(
                "INSERT INTO agent (user_id, command, created, updated) VALUES (1, 'x', 1, 1)",
                [],
            )?;
            Ok(())
        })
        .await;
    assert!(outcome.is_err());
    let rows: i64 = store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM agent", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(rows, 0);
    store.close().await;
}
