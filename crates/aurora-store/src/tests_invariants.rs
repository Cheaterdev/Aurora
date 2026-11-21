//! The invariant checker: a pristine database passes, and every check has a
//! test that corrupts a copy of a database and proves the check notices.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use crate::agents::{self, NewAgent};
use crate::comments::{self, NewComment};
use crate::events::{self, Start};
use crate::invariants::{self, INVARIANTS, Report};
use crate::request_log::{self, Reply, RequestKey};
use crate::tests_support::documents::{self, DocumentPatch, NewDocument};
use crate::tests_support::initiatives::{self, NewInitiative};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::links::{self, LinkKind};
use crate::tests_support::{projects, test_actor};
use crate::{Store, StoreError};

/// Two projects with issues, a label, a link, a comment thread with a mention,
/// a document with two revisions, an initiative, an agent, a consumer that
/// acknowledged part of the log and a remembered request id.
async fn seed(store: &Store) {
    store
        .write(|tx| {
            let actor = test_actor(tx);
            projects::create_project(tx, 1, "TRK", "Tracker", None)?;
            projects::create_issue_type(tx, 1, "TRK", "Task")?;
            let label = projects::create_label(tx, 1, "TRK", "bug", "#ff0000")?;
            projects::create_project(tx, 1, "OTH", "Other", None)?;
            projects::create_issue_type(tx, 1, "OTH", "Item")?;
            for n in 0..3_i64 {
                issues::create_issue(
                    tx,
                    10 + n,
                    "TRK",
                    NewIssue {
                        title: format!("issue {n}"),
                        labels: if n == 0 { vec![label.id] } else { vec![] },
                        ..NewIssue::default()
                    },
                )?;
            }
            issues::create_issue(
                tx,
                20,
                "OTH",
                NewIssue {
                    title: "other".into(),
                    ..NewIssue::default()
                },
            )?;
            links::add_link(tx, 30, "TRK", 1, 2, LinkKind::Blocks)?;
            let agent = agents::create_agent(
                tx,
                35,
                &actor,
                NewAgent {
                    name: Some("runner".into()),
                    command: "x".into(),
                    projects: vec!["TRK".into()],
                    ..NewAgent::default()
                },
            )?;
            crate::prompt::save_template(
                tx,
                36,
                &actor,
                agent.id,
                Some("TRK"),
                None,
                "Work on {{issue.key}}",
            )?;
            projects::update_project(
                tx,
                37,
                &actor,
                "TRK",
                projects::ProjectPatch {
                    rules: Some(Some("be careful".into())),
                    ..projects::ProjectPatch::default()
                },
            )?;
            let top = comments::add_comment(
                tx,
                40,
                &actor,
                "TRK",
                1,
                NewComment {
                    body: "hi @runner".into(),
                    parent: None,
                },
            )?;
            comments::add_comment(
                tx,
                41,
                &actor,
                "TRK",
                1,
                NewComment {
                    body: "a reply".into(),
                    parent: Some(top.id),
                },
            )?;
            let doc = documents::create_document(
                tx,
                50,
                "TRK",
                NewDocument {
                    title: "Handbook".into(),
                    body: Some("first".into()),
                    parent: None,
                },
            )?;
            documents::update_document(
                tx,
                51,
                "TRK",
                doc.document.id,
                DocumentPatch {
                    body: Some("second".into()),
                    ..DocumentPatch::default()
                },
            )?;
            initiatives::create_initiative(
                tx,
                60,
                "TRK",
                NewInitiative {
                    name: "Launch".into(),
                    ..NewInitiative::default()
                },
            )?;
            events::register_consumer(tx, 70, "runner", Start::Beginning)?;
            events::ack(tx, 71, "runner", 2)?;
            let key = RequestKey::new(actor.user_id, "request-0001", &[b"POST", b"/x"])?;
            request_log::run(tx, Some(&key), 80, Duration::from_secs(3600), || {
                Ok(Reply::new(201, "{}".into()))
            })?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
}

async fn seeded() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    seed(&store).await;
    store.close().await;
    (dir, path)
}

/// Runs `sql` on a copy of a seeded database (foreign keys off, so that the
/// damage can be done) and checks the result.
async fn corrupted(sql: &str) -> Report {
    let (_dir, path) = seeded().await;
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(&format!("PRAGMA foreign_keys = OFF; {sql}"))
        .unwrap();
    invariants::check(&conn)
}

fn failed(report: &Report, name: &str) -> String {
    let check = report
        .get(name)
        .unwrap_or_else(|| panic!("no check named {name}"));
    assert!(!check.ok, "{name} should have failed:\n{report:#?}");
    check.details.clone()
}

fn passes(report: &Report, name: &str) {
    let check = report
        .get(name)
        .unwrap_or_else(|| panic!("no check named {name}"));
    assert!(check.ok, "{name} should pass: {}", check.details);
}

const DROP_EVENT_TRIGGERS: &str = "DROP TRIGGER event_no_update; DROP TRIGGER event_no_delete;";

#[tokio::test]
async fn a_pristine_database_passes_every_check() {
    let (_dir, path) = seeded().await;
    let report = invariants::check_file(&path).unwrap();
    assert!(report.ok(), "{report:#?}");
    assert_eq!(report.checks.len(), INVARIANTS.len());
    assert_eq!(report.schema_version, crate::latest_version());
    let state = report.get("events.state_agreement").unwrap();
    assert!(state.details.contains("events reproduce"), "{state:?}");
    let names: std::collections::BTreeSet<_> = report.checks.iter().map(|c| c.name).collect();
    assert_eq!(names.len(), report.checks.len(), "check names are unique");
}

#[tokio::test]
async fn an_empty_new_database_passes_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    Store::open(&path).unwrap().close().await;
    let report = invariants::check_file(&path).unwrap();
    assert!(report.ok(), "{report:#?}");
}

#[tokio::test]
async fn damaged_pages_fail_the_sqlite_integrity_check() {
    let (_dir, path) = seeded().await;
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(
        bytes.len() > 4096 * 8,
        "the database is too small to damage"
    );
    bytes[4096 * 2..4096 * 6].fill(0xAB);
    std::fs::write(&path, bytes).unwrap();

    let conn = Connection::open(&path).unwrap();
    let report = invariants::check(&conn);
    assert!(!report.ok());
    failed(&report, "sqlite.integrity_check");
}

#[tokio::test]
async fn a_dangling_reference_fails_the_foreign_key_check() {
    let report = corrupted("INSERT INTO issue_label (issue_id, label_id) VALUES (9999, 1);").await;
    let details = failed(&report, "sqlite.foreign_key_check");
    assert!(details.contains("issue_label"), "{details}");
}

#[tokio::test]
async fn a_database_behind_this_build_fails_and_skips_the_rest() {
    let report = corrupted("DELETE FROM schema_version WHERE version = 14;").await;
    let details = failed(&report, "schema.version");
    assert!(details.contains("start aurora to migrate"), "{details}");
    let skipped = failed(&report, "events.state_agreement");
    assert!(skipped.starts_with("not run"), "{skipped}");
    passes(&report, "sqlite.integrity_check");
}

#[tokio::test]
async fn a_hole_in_the_migrations_fails_the_schema_check() {
    let report = corrupted("DELETE FROM schema_version WHERE version = 5;").await;
    let details = failed(&report, "schema.version");
    assert!(details.contains("migration 5 is missing"), "{details}");
}

#[tokio::test]
async fn a_deleted_event_fails_the_id_check_and_the_agreement() {
    let report = corrupted(&format!(
        "{DROP_EVENT_TRIGGERS} DELETE FROM event WHERE id = 3;"
    ))
    .await;
    let details = failed(&report, "events.ids");
    assert!(details.contains("event 3 is missing"), "{details}");
    failed(&report, "events.state_agreement");
    failed(&report, "events.immutable_triggers");
}

#[tokio::test]
async fn an_id_counter_set_back_fails_the_id_check() {
    let report = corrupted("UPDATE sqlite_sequence SET seq = 1 WHERE name = 'event';").await;
    let details = failed(&report, "events.ids");
    assert!(details.contains("counter"), "{details}");
    passes(&report, "events.immutable_triggers");
}

#[tokio::test]
async fn a_removed_trigger_fails_the_immutability_check_alone() {
    let report = corrupted("DROP TRIGGER event_no_replace;").await;
    let details = failed(&report, "events.immutable_triggers");
    assert!(details.contains("event_no_replace"), "{details}");
    passes(&report, "events.state_agreement");
    passes(&report, "events.ids");
}

#[tokio::test]
async fn a_change_without_its_event_fails_the_agreement() {
    let report = corrupted("UPDATE issue SET title = 'forged' WHERE id = 1;").await;
    let details = failed(&report, "events.state_agreement");
    assert!(details.contains("issue 1"), "{details}");

    let report =
        corrupted("INSERT INTO label (project_id, name, color) VALUES (1, 'ghost', '#000000');")
            .await;
    let details = failed(&report, "events.state_agreement");
    assert!(
        details.contains("the tables have it, the log does not"),
        "{details}"
    );
}

#[tokio::test]
async fn an_event_without_its_change_fails_the_agreement() {
    let report = corrupted("DELETE FROM issue_link;").await;
    let details = failed(&report, "events.state_agreement");
    assert!(
        details.contains("the log has it, the tables do not"),
        "{details}"
    );

    let report = corrupted(
        "INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
         VALUES (99, NULL, 1, 'issue', 1, 'issue.archived', '{}');",
    )
    .await;
    failed(&report, "events.state_agreement");
}

#[tokio::test]
async fn an_event_kind_the_replay_does_not_know_fails_the_agreement() {
    let report = corrupted(
        "INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
         VALUES (99, NULL, 1, 'thing', 1, 'mystery.happened', '{}');",
    )
    .await;
    let details = failed(&report, "events.state_agreement");
    assert!(details.contains("mystery.happened"), "{details}");
}

#[tokio::test]
async fn a_counter_that_fell_behind_fails_the_issue_numbering() {
    let report = corrupted("UPDATE project SET next_issue_number = 2 WHERE key = 'TRK';").await;
    let details = failed(&report, "projects.issue_numbering");
    assert!(details.contains("project TRK"), "{details}");
}

#[tokio::test]
async fn an_orphaned_or_missing_revision_fails_the_revision_chain() {
    let report = corrupted("DELETE FROM document_revision WHERE number = 1;").await;
    let details = failed(&report, "documents.revision_chain");
    assert!(details.contains("document 1"), "{details}");

    let report = corrupted("UPDATE document SET revision = 9;").await;
    failed(&report, "documents.revision_chain");

    let report = corrupted("UPDATE document SET body = 'tampered';").await;
    let details = failed(&report, "documents.revision_chain");
    assert!(details.contains("differs from its revision"), "{details}");

    let report = corrupted("DELETE FROM document_revision;").await;
    failed(&report, "documents.revision_chain");
}

#[tokio::test]
async fn a_reference_across_projects_fails_the_same_project_check() {
    let report = corrupted(
        "UPDATE issue SET parent_id = (SELECT id FROM issue WHERE project_id =
             (SELECT id FROM project WHERE key = 'OTH'))
         WHERE id = 1;",
    )
    .await;
    let details = failed(&report, "references.same_project");
    assert!(details.contains("issue 1 has a parent"), "{details}");
}

#[tokio::test]
async fn a_reply_to_a_reply_fails_the_thread_check() {
    let report = corrupted("UPDATE comment SET parent_id = 2 WHERE id = 1;").await;
    let details = failed(&report, "comments.threads");
    assert!(details.contains("comment 1"), "{details}");
}

#[tokio::test]
async fn a_cursor_beyond_the_log_fails_the_cursor_check() {
    let report = corrupted("UPDATE consumer_cursor SET last_event_id = 9999;").await;
    let details = failed(&report, "consumers.cursors");
    assert!(details.contains("consumer runner"), "{details}");
}

#[tokio::test]
async fn a_bad_remembered_request_fails_the_request_log_check() {
    let report = corrupted(
        "INSERT INTO request_log (principal_user_id, request_id, fingerprint, status, created)
         VALUES (777, 'ghost-user-1', 'f', 201, 1), (4242, 'failed-one-1', 'f', 500, 1);",
    )
    .await;
    let details = failed(&report, "request_log.rows");
    assert!(details.contains("missing user 777"), "{details}");
    assert!(details.contains("status 500"), "{details}");
}

#[tokio::test]
async fn a_stale_search_index_fails_the_search_check() {
    let report = corrupted("DELETE FROM search_index WHERE rowid = 4;").await;
    failed(&report, "search.index");
}

#[tokio::test]
async fn the_runs_check_is_real_and_counts_runs() {
    let (_dir, path) = seeded().await;
    let report = invariants::check_file(&path).unwrap();
    let runs = report.get("runs.no_orphans").unwrap();
    assert!(runs.ok);
    assert!(runs.details.starts_with("0 runs"), "{}", runs.details);
}

#[tokio::test]
async fn a_missing_or_foreign_file_cannot_be_checked() {
    let dir = tempfile::tempdir().unwrap();
    let missing = invariants::check_file(&dir.path().join("nope.db")).err();
    assert!(missing.is_some_and(|e| e.to_string().contains("not a file")));

    let text = dir.path().join("text.db");
    std::fs::write(&text, "this is not a database, just text ".repeat(200)).unwrap();
    assert!(invariants::check_file(&text).is_err());
}

#[tokio::test]
async fn checking_writes_nothing_and_works_beside_a_running_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    seed(&store).await;

    let before = file_listing(dir.path());
    let report = invariants::check_file(&path).unwrap();
    assert!(report.ok(), "{report:#?}");
    // The store is still writing afterwards.
    store
        .write(|tx| {
            issues::create_issue(
                tx,
                100,
                "TRK",
                NewIssue {
                    title: "after the check".into(),
                    ..NewIssue::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(invariants::check_file(&path).unwrap().ok());
    let after = file_listing(dir.path());
    assert!(
        after
            .iter()
            .all(|name| before.contains(name) || name.ends_with("-shm")),
        "{before:?} -> {after:?}"
    );
    store.close().await;
}

fn file_listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_template_that_no_longer_parses_fails_the_template_check() {
    let report = corrupted("UPDATE prompt_template SET body = '{{#if issue.key}} open';").await;
    let details = failed(&report, "prompt_templates.valid");
    assert!(details.contains("does not parse"), "{details}");
    // The text length no longer matches the log either.
    failed(&report, "events.state_agreement");
}

#[tokio::test]
async fn a_template_with_an_unknown_variable_fails_the_template_check() {
    let report = corrupted("UPDATE prompt_template SET body = '{{no.such}}';").await;
    let details = failed(&report, "prompt_templates.valid");
    assert!(
        details.contains("{{no.such}}") || details.contains("no.such"),
        "{details}"
    );
}

#[tokio::test]
async fn a_template_for_an_issue_type_of_another_project_fails_the_template_check() {
    let report = corrupted(
        "DROP TRIGGER prompt_template_type_in_project_insert;
         DROP TRIGGER prompt_template_type_in_project_update;
         UPDATE prompt_template SET issue_type_id = (SELECT id FROM issue_type WHERE name = 'Item');",
    )
    .await;
    let details = failed(&report, "prompt_templates.valid");
    assert!(details.contains("another project"), "{details}");
}

#[tokio::test]
async fn a_template_or_rules_text_changed_without_an_event_fails_the_agreement() {
    let report =
        corrupted("UPDATE prompt_template SET body = 'a different text of another length';").await;
    let details = failed(&report, "events.state_agreement");
    assert!(details.contains("prompt template"), "{details}");

    let report = corrupted("UPDATE project SET rules = 'x' WHERE key = 'TRK';").await;
    let details = failed(&report, "events.state_agreement");
    assert!(details.contains("rules"), "{details}");
    passes(&report, "prompt_templates.valid");
}
