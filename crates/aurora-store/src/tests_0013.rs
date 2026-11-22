//! Migration 0013 (prompt templates, project rules) applied to a populated
//! version-12 database.

use rusqlite::Connection;

use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_twelve_database_gets_templates_and_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v12 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..12]).unwrap();
    assert_eq!(v12.schema_version(), 12);
    v12.write(|tx| {
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
    v12.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 18);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let (users, projects, templates, rules, index) = store
        .read(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM project", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM prompt_template", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                c.query_row("SELECT rules IS NULL FROM project WHERE id = 1", [], |r| {
                    r.get::<_, bool>(0)
                })?,
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'prompt_template_scope'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((users, projects, templates, index), (2, 1, 0, 1));
    assert!(rules, "existing projects have no rules");

    // The new table and column work on the migrated database.
    store
        .write(|tx| {
            tx.execute("UPDATE project SET rules = 'be kind' WHERE id = 1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    store.close().await;

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'prompt_template'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-12 database");
    let projects: i64 = old
        .query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))
        .unwrap();
    assert_eq!(projects, 1);
    assert!(
        old.prepare("SELECT rules FROM project").is_err(),
        "the backup has no rules column"
    );
}
