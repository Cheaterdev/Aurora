//! Migration 0016 and the transcript columns (TRK-17): what a finished run
//! records, the cancel's late record, the event payload and the
//! `runs.transcripts` invariant.

use rusqlite::Connection;
use serde_json::Value;

use crate::agents::{self, NewAgent};
use crate::events::{self, EventFilter};
use crate::invariants;
use crate::migrations::MIGRATIONS;
use crate::runs::{
    NewRun, Outcome, RunReason, RunStatus, TranscriptEnd, create_run, finish_run, get_run,
    mark_running, record_transcript, transcript_path,
};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::{projects, test_actor};
use crate::{OpenOptions, Store};

#[tokio::test]
async fn populated_version_fifteen_database_gets_the_transcript_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let v15 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..15]).unwrap();
    assert_eq!(v15.schema_version(), 15);
    v15.close().await;
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(
        "INSERT INTO user (id, name, kind, role) VALUES
             (1, 'Owner', 'human', 'owner'), (2, 'bot', 'agent', 'agent');
         INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 2);
         INSERT INTO \"column\" (id, project_id, name, state_type, position)
             VALUES (1, 1, 'Todo', 'unstarted', 0);
         INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Task');
         INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
             VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1);
         INSERT INTO agent (id, user_id, command, created, updated) VALUES (1, 2, 'x', 1, 1);
         INSERT INTO run (id, agent_id, issue_id, project_id, reason, status, boot_id, message,
                          created, finished)
             VALUES (7, 1, 1, 1, 'manual', 'succeeded', 'old', 'hello', 11, 12);",
    )
    .unwrap();
    drop(raw);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 18);
    let run = store.read(|c| get_run(c, 7)).await.unwrap().run;
    assert_eq!((run.transcript_bytes, run.transcript_truncated), (0, false));
    let bad = store
        .write(|tx| {
            tx.execute("UPDATE run SET transcript_truncated = 2 WHERE id = 7", [])?;
            Ok(())
        })
        .await;
    assert!(bad.is_err(), "the flag is constrained");
    let bad = store
        .write(|tx| {
            tx.execute("UPDATE run SET transcript_bytes = -1 WHERE id = 7", [])?;
            Ok(())
        })
        .await;
    assert!(bad.is_err(), "the size is not negative");
    store.close().await;
}

async fn world() -> (tempfile::TempDir, Store, i64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let agent = store
        .write(|tx| {
            let actor = test_actor(tx);
            projects::create_project(tx, 1, "TRK", "Tracker", None)?;
            projects::create_issue_type(tx, 1, "TRK", "Task")?;
            for n in 0..3 {
                issues::create_issue(
                    tx,
                    2,
                    "TRK",
                    NewIssue {
                        title: format!("issue {n}"),
                        ..NewIssue::default()
                    },
                )?;
            }
            let agent = agents::create_agent(
                tx,
                3,
                &actor,
                NewAgent {
                    name: Some("bot".into()),
                    command: "x".into(),
                    projects: vec!["TRK".into()],
                    ..NewAgent::default()
                },
            )?;
            Ok(agent.id)
        })
        .await
        .unwrap();
    (dir, store, agent)
}

fn new_run(agent: i64, number: i64) -> NewRun {
    NewRun {
        agent_id: agent,
        project: "TRK".into(),
        number,
        reason: RunReason::Manual,
        trigger_event_id: None,
        message: Some("hello".into()),
        template_source: Some("inline".into()),
        template_id: None,
        edited: true,
        boot_id: "boot-1".into(),
        failed: None,
        worktree_root: None,
        base_branch: None,
        comment_id: None,
        limits: None,
    }
}

async fn finished_events(store: &Store) -> Vec<Value> {
    store
        .read(|c| {
            let all = events::read_events(
                c,
                0,
                Some(500),
                &EventFilter {
                    kind_prefix: Some("run.finished".into()),
                    ..EventFilter::default()
                },
            )?;
            Ok(all.into_iter().map(|e| e.payload).collect())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn finishing_a_run_records_the_transcript_and_the_event_carries_only_the_size() {
    let (_dir, store, agent) = world().await;
    let id = store
        .write(move |tx| {
            let id = create_run(tx, 10, None, new_run(agent, 1))?.run.id;
            mark_running(tx, 11, id, 5)?;
            let mut outcome = Outcome::new(RunStatus::Succeeded);
            outcome.transcript = Some(TranscriptEnd {
                bytes: 1234,
                truncated: true,
            });
            finish_run(tx, 12, None, id, outcome)?;
            Ok(id)
        })
        .await
        .unwrap();
    let run = store.read(move |c| get_run(c, id)).await.unwrap().run;
    assert_eq!(
        (run.transcript_bytes, run.transcript_truncated),
        (1234, true)
    );
    let events = finished_events(&store).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["transcript_bytes"], 1234);
    assert!(
        events[0].get("transcript_truncated").is_none(),
        "only the size is in the event: {}",
        events[0]
    );
    let json = serde_json::to_value(&run).unwrap();
    assert_eq!(json["transcript_bytes"], 1234);
    assert_eq!(json["transcript_truncated"], true);
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    // The file does not exist: a note, not a failure.
    assert!(report.ok(), "{:#?}", report.failed());
    assert!(
        report
            .get("runs.transcripts")
            .unwrap()
            .details
            .contains("has no transcript file"),
        "{:?}",
        report.get("runs.transcripts")
    );
}

#[tokio::test]
async fn a_cancel_records_the_transcript_later_and_only_once() {
    let (_dir, store, agent) = world().await;
    let id = store
        .write(move |tx| {
            let id = create_run(tx, 10, None, new_run(agent, 1))?.run.id;
            mark_running(tx, 11, id, 5)?;
            Ok(id)
        })
        .await
        .unwrap();
    let end = TranscriptEnd {
        bytes: 77,
        truncated: false,
    };
    let early = store
        .write(move |tx| record_transcript(tx, id, end))
        .await
        .unwrap();
    assert!(!early, "an active run records nothing");
    store
        .write(move |tx| {
            finish_run(
                tx,
                12,
                Some(crate::tests_support::TEST_USER_ID),
                id,
                Outcome::new(RunStatus::Canceled),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let events = finished_events(&store).await;
    assert!(
        events[0].get("transcript_bytes").is_none(),
        "unknown at the time of the cancel: {}",
        events[0]
    );
    assert!(
        store
            .write(move |tx| record_transcript(tx, id, end))
            .await
            .unwrap()
    );
    let again = store
        .write(move |tx| {
            record_transcript(
                tx,
                id,
                TranscriptEnd {
                    bytes: 5,
                    truncated: true,
                },
            )
        })
        .await
        .unwrap();
    assert!(!again, "a recorded transcript is never overwritten");
    let nothing = store
        .write(move |tx| {
            record_transcript(
                tx,
                id,
                TranscriptEnd {
                    bytes: 0,
                    truncated: false,
                },
            )
        })
        .await
        .unwrap();
    assert!(!nothing);
    let run = store.read(move |c| get_run(c, id)).await.unwrap().run;
    assert_eq!(
        (run.transcript_bytes, run.transcript_truncated),
        (77, false)
    );
}

/// A finished run 1 with a recorded transcript whose file exists with the
/// right size, and an active run 2; `sql` then damages a copy.
async fn corrupted(sql: &'static str, file_len: Option<usize>) -> invariants::Report {
    let (dir, store, agent) = world().await;
    store
        .write(move |tx| {
            let a = create_run(tx, 10, None, new_run(agent, 1))?.run.id;
            mark_running(tx, 11, a, 5)?;
            let mut outcome = Outcome::new(RunStatus::Succeeded);
            outcome.transcript = Some(TranscriptEnd {
                bytes: 100,
                truncated: false,
            });
            finish_run(tx, 12, None, a, outcome)?;
            let b = create_run(tx, 13, None, new_run(agent, 2))?.run.id;
            mark_running(tx, 14, b, 6)?;
            Ok(())
        })
        .await
        .unwrap();
    store.close().await;
    if let Some(len) = file_len {
        let path = transcript_path(dir.path(), 1);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; len]).unwrap();
    }
    let conn = Connection::open(dir.path().join("test.db")).unwrap();
    conn.execute_batch(&format!("PRAGMA foreign_keys = OFF; {sql}"))
        .unwrap();
    invariants::check(&conn)
}

fn failed(report: &invariants::Report) -> String {
    let check = report.get("runs.transcripts").unwrap();
    assert!(!check.ok, "runs.transcripts should fail: {report:#?}");
    check.details.clone()
}

#[tokio::test]
async fn the_transcript_invariant_notices_corruption_and_only_notes_a_missing_file() {
    let clean = corrupted("SELECT 1;", Some(100)).await;
    assert!(clean.ok(), "{:#?}", clean.failed());
    assert_eq!(
        clean.get("runs.transcripts").unwrap().details,
        "1 recorded transcripts"
    );

    let missing = corrupted("SELECT 1;", None).await;
    assert!(missing.ok(), "{:#?}", missing.failed());
    assert!(
        missing
            .get("runs.transcripts")
            .unwrap()
            .details
            .contains("note: run 1 has no transcript file")
    );

    let details = failed(&corrupted("SELECT 1;", Some(99)).await);
    assert!(
        details.contains("run 1") && details.contains("100") && details.contains("99"),
        "{details}"
    );
    let details = failed(&corrupted("SELECT 1;", Some(101)).await);
    assert!(details.contains("run 1"), "{details}");
    let details = failed(
        &corrupted(
            "UPDATE run SET transcript_bytes = 5 WHERE id = 2;",
            Some(100),
        )
        .await,
    );
    assert!(
        details.contains("run 2") && details.contains("running"),
        "{details}"
    );
    let details = failed(
        &corrupted(
            "UPDATE run SET transcript_truncated = 1 WHERE id = 2;",
            Some(100),
        )
        .await,
    );
    assert!(details.contains("run 2"), "{details}");

    // 0 means "not recorded" (a crash): the file is never compared then.
    let crashed = corrupted("UPDATE run SET transcript_bytes = 0 WHERE id = 1;", Some(5)).await;
    assert!(crashed.ok(), "{:#?}", crashed.failed());
}
