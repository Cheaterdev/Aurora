//! Migration 0009 (documents) applied to a populated version-8 database.

use rusqlite::Connection;

use crate::auth::{Principal, Role, Scope};
use crate::documents::{self, DocumentTarget, NewDocument};
use crate::migrations::MIGRATIONS;
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_eight_database_gains_documents_and_keeps_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v8 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..8]).unwrap();
    assert_eq!(v8.schema_version(), 8);
    v8.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES (1, 'Owner', 'human', 'owner');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 2);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES
                 (1, 1, 'Todo', 'unstarted', 0);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1);
             INSERT INTO initiative (id, project_id, name, created, updated) VALUES (1, 1, 'Launch', 2, 2);
             INSERT INTO comment (issue_id, author_id, body, created) VALUES (1, 1, 'hello', 3);
             INSERT INTO event (time, project_id, entity_kind, entity_id, kind, payload_json)
                 VALUES (5, 1, 'issue', 1, 'issue.created', '{}');",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v8.close().await;

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
            let made = documents::create_document(
                tx,
                20,
                &owner,
                "AA",
                NewDocument {
                    title: "Spec".into(),
                    body: Some("# Spec".into()),
                    parent: None,
                },
            )?;
            documents::add_link(
                tx,
                21,
                &owner,
                "AA",
                made.document.id,
                DocumentTarget::Issue(1),
            )?;
            documents::add_link(
                tx,
                22,
                &owner,
                "AA",
                made.document.id,
                DocumentTarget::Initiative(1),
            )?;
            Ok(made)
        })
        .await
        .unwrap();
    let id = made.document.id;
    let (issue, initiative) = store
        .read(move |c| {
            let issue = crate::issues::get_issue(c, "AA", 1)?;
            let initiative = crate::initiatives::get_initiative(c, 30, "AA", 1, None, None)?;
            Ok((issue, initiative))
        })
        .await
        .unwrap();
    // The old issue and initiative gained their document without changing otherwise.
    assert_eq!(issue.documents.len(), 1);
    assert_eq!(issue.documents[0].id, id);
    assert_eq!(initiative.documents.len(), 1);
    store.close().await;

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("SELECT COUNT(*) FROM issue"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comment"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM document_revision"), 1);
    // The old event, document.created and two document.link_added.
    assert_eq!(count("SELECT COUNT(*) FROM event"), 4);

    // The table rules hold below the store too.
    let both = conn.execute(
        "INSERT INTO document_link (document_id, issue_id, initiative_id, created)
         VALUES (?1, 1, 1, 1)",
        [id],
    );
    assert!(both.is_err(), "a link has exactly one target");
    let neither = conn.execute(
        "INSERT INTO document_link (document_id, created) VALUES (?1, 1)",
        [id],
    );
    assert!(neither.is_err(), "a link has exactly one target");
    let twice = conn.execute(
        "INSERT INTO document_revision (document_id, number, title, body, created)
         VALUES (?1, 1, 't', 'b', 1)",
        [id],
    );
    assert!(twice.is_err(), "revision numbers are unique per document");

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'document%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-8 database");
}
