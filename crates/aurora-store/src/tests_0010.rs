//! Migration 0010 (search) applied to a populated version-9 database.

use rusqlite::Connection;

use crate::migrations::MIGRATIONS;
use crate::search::{self, Kind, SearchQuery, search_check};
use crate::{OpenOptions, Store, default_backup_dir};

#[tokio::test]
async fn populated_version_nine_database_becomes_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let v9 = Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..9]).unwrap();
    assert_eq!(v9.schema_version(), 9);
    v9.write(|tx| {
        tx.execute_batch(
            "INSERT INTO user (id, name, kind, role) VALUES (1, 'Owner', 'human', 'owner');
             INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 4);
             INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES
                 (1, 1, 'Todo', 'unstarted', 0);
             INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Bug');
             INSERT INTO issue (id, project_id, number, type_id, column_id, title, description, position, created, updated, archived)
                 VALUES (1, 1, 1, 1, 1, 'Old zebra', 'stripes everywhere', 1.0, 1, 1, 0),
                        (2, 1, 2, 1, 1, 'Retired giraffe', '', 2.0, 1, 1, 1),
                        (3, 1, 3, 1, 1, 'Third', 'with \u{1}marker\u{2} text', 3.0, 1, 1, 0);
             INSERT INTO comment (id, issue_id, author_id, body, created, deleted) VALUES
                 (1, 1, 1, 'a live wombat', 3, NULL),
                 (2, 1, 1, 'a deleted quokka', 3, 4),
                 (3, 2, 1, 'a comment on the archived issue', 3, NULL);
             INSERT INTO document (id, project_id, title, body, position, created, updated, archived) VALUES
                 (1, 1, 'Handbook', 'chapter on otters', 1.0, 1, 1, 0),
                 (2, 1, 'Old handbook', 'chapter on seals', 2.0, 1, 1, 1);",
        )?;
        Ok(())
    })
    .await
    .unwrap();
    v9.close().await;

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), crate::latest_version());
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));

    let find = |text: &'static str, include_archived: bool| {
        store.read(move |c| {
            search::search(
                c,
                &SearchQuery {
                    q: text.into(),
                    include_archived,
                    ..SearchQuery::default()
                },
            )
        })
    };
    let issue = find("zebra", false).await.unwrap();
    assert_eq!(issue.hits.len(), 1);
    assert_eq!(issue.hits[0].kind, Kind::Issue);
    assert_eq!(issue.hits[0].issue.as_ref().unwrap().identifier, "AA-1");
    assert_eq!(
        find("wombat", false).await.unwrap().hits[0].kind,
        Kind::Comment
    );
    assert!(
        find("quokka", true).await.unwrap().hits.is_empty(),
        "deleted comments are not indexed"
    );
    assert!(find("giraffe", false).await.unwrap().hits.is_empty());
    let retired = find("giraffe", true).await.unwrap();
    assert!(retired.hits[0].archived);
    assert!(find("archived issue", true).await.unwrap().hits[0].archived);
    assert!(find("seals", false).await.unwrap().hits.is_empty());
    assert!(find("seals", true).await.unwrap().hits[0].archived);
    assert_eq!(
        find("otters", false).await.unwrap().hits[0].kind,
        Kind::Document
    );
    assert_eq!(
        find("AA-3", false).await.unwrap().hits[0]
            .issue
            .as_ref()
            .unwrap()
            .number,
        3
    );
    let marked = find("marker", false).await.unwrap();
    assert_eq!(marked.hits.len(), 1);
    assert!(
        marked.hits[0]
            .snippet
            .iter()
            .all(|s| !s.text.contains('\u{1}'))
    );

    let check = store.read(search_check).await.unwrap();
    assert!(check.is_consistent(), "{check:?}");
    assert_eq!(check.indexed.issues, 3);
    assert_eq!(check.indexed.comments, 2);
    assert_eq!(check.indexed.documents, 2);
    store.close().await;

    let old = Connection::open(&backup).unwrap();
    let tables: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'search_index%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the backup is the version-9 database");
}

#[tokio::test]
async fn the_bundled_sqlite_has_fts5_with_the_options_the_index_uses() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let options: Vec<String> = store
        .read(|c| {
            let mut stmt = c.prepare("PRAGMA compile_options")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert!(options.iter().any(|o| o == "ENABLE_FTS5"), "{options:?}");
}
