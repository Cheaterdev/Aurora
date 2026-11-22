//! Migration 0018 and start-up reconciliation (TRK-45): the `reaped` note, the
//! comment and the card move written in the transaction that interrupts a run,
//! and the `runs.reconciled` invariant.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::agents::{self, NewAgent};
use crate::invariants;
use crate::issues::{self as real_issues, MoveTarget};
use crate::migrations::MIGRATIONS;
use crate::projects::StateType;
use crate::runs::{
    self, Limits, NewRun, RunReason, RunStatus, create_run, get_run, mark_running_process,
};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::{projects, test_actor};
use crate::{OpenOptions, Store};

#[tokio::test]
async fn populated_version_seventeen_database_gets_the_containment_columns_and_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let v17 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..17]).unwrap();
    assert_eq!(v17.schema_version(), 17);
    v17.close().await;
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
         INSERT INTO run (id, agent_id, issue_id, project_id, reason, status, pid, boot_id, message,
                          created, started)
             VALUES (8, 1, 1, 1, 'manual', 'running', 4242, 'old', 'again', 13, 14);",
    )
    .unwrap();
    drop(raw);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 18);
    let backup = store.open_report().backup.clone().expect("backup taken");
    let old = Connection::open(&backup).unwrap();
    let has_reaped: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('run') WHERE name IN ('reaped', 'pid_started_at')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_reaped, 0, "the backup is the version-17 database");
    drop(old);

    let run = store.read(|c| get_run(c, 8)).await.unwrap().run;
    assert_eq!((run.pid, run.reaped), (Some(4242), None));
    // The old row is still legal for the invariants (no start time, no note).
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(
        report.get("runs.reconciled").unwrap().ok,
        "{:#?}",
        report.failed()
    );
    store.close().await;
}

/// Project TRK (default columns: Backlog, Todo, In Progress, Done, Canceled)
/// with four issues and an agent `bot`.
async fn world() -> (tempfile::TempDir, Store, i64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let agent = store
        .write(|tx| {
            let actor = test_actor(tx);
            projects::create_project(tx, 1, "TRK", "Tracker", None)?;
            projects::create_issue_type(tx, 1, "TRK", "Task")?;
            for n in 0..4 {
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
                    max_concurrent: Some(4),
                    ..NewAgent::default()
                },
            )?;
            Ok(agent.id)
        })
        .await
        .unwrap();
    (dir, store, agent)
}

async fn column_of(store: &Store, kind: StateType) -> i64 {
    store
        .read(move |c| Ok(projects::columns_by_state_type(c, "TRK", kind)?[0].id))
        .await
        .unwrap()
}

async fn move_to(store: &Store, number: i64, column_id: i64) {
    store
        .write(move |tx| {
            let actor = test_actor(tx);
            real_issues::move_issue(
                tx,
                5,
                &actor,
                "TRK",
                number,
                MoveTarget {
                    column_id,
                    ..MoveTarget::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// A running run of boot-1 on issue `number`, with a recorded process.
async fn running(store: &Store, agent: i64, number: i64, pid: i64, started_at: Option<i64>) -> i64 {
    store
        .write(move |tx| {
            let created = create_run(
                tx,
                10,
                None,
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
                    limits: Some(Limits::default()),
                },
            )?;
            mark_running_process(tx, 11, created.run.id, pid, started_at)?;
            Ok(created.run.id)
        })
        .await
        .unwrap()
}

async fn reconcile(store: &Store, notes: HashMap<i64, String>) -> Vec<i64> {
    store
        .write(move |tx| runs::reconcile_startup_with(tx, 50, "boot-2", &notes))
        .await
        .unwrap()
}

async fn comments_of(store: &Store, number: i64) -> Vec<(String, String)> {
    store
        .read(move |c| {
            let mut stmt = c.prepare(
                "SELECT u.name, c.body FROM comment c
                 JOIN issue i ON i.id = c.issue_id JOIN user u ON u.id = c.author_id
                 WHERE i.number = ?1 ORDER BY c.id",
            )?;
            let rows = stmt.query_map([number], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
}

async fn count_events(store: &Store, kind: &'static str) -> i64 {
    store
        .read(move |c| {
            Ok(
                c.query_row("SELECT COUNT(*) FROM event WHERE kind = ?1", [kind], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap()
}

async fn column_name_of(store: &Store, number: i64) -> String {
    store
        .read(move |c| {
            Ok(c.query_row(
                "SELECT c.name FROM issue i JOIN \"column\" c ON c.id = i.column_id WHERE i.number = ?1",
                [number],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

async fn check(store: &Store) {
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn reconciling_notes_the_process_comments_and_moves_the_card_in_one_go() {
    let (_dir, store, agent) = world().await;
    let started = column_of(&store, StateType::Started).await;
    move_to(&store, 1, started).await;
    let run = running(&store, agent, 1, 777, Some(123_456)).await;
    assert_eq!(column_name_of(&store, 1).await, "In Progress");

    let notes = HashMap::from([(run, "killed leftover process 777".to_string())]);
    assert_eq!(reconcile(&store, notes).await, vec![run]);

    let found = store.read(move |c| get_run(c, run)).await.unwrap().run;
    assert_eq!(found.status, RunStatus::Interrupted);
    assert_eq!(found.error.as_deref(), Some("server restarted"));
    assert_eq!(found.reaped.as_deref(), Some("killed leftover process 777"));
    assert_eq!(column_name_of(&store, 1).await, "Todo");

    let comments = comments_of(&store, 1).await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].0, "bot", "written as the run's agent");
    assert_eq!(
        comments[0].1,
        format!(
            "[Aurora] Run {run} was interrupted because the server restarted. \
             Process: killed leftover process 777. The card was moved back to Todo."
        )
    );
    assert!(
        !comments[0].1.contains(['\\', '/', '@']),
        "no path, no mention: {}",
        comments[0].1
    );
    assert_eq!(count_events(&store, "issue.moved").await, 2, "in, and out");
    assert_eq!(count_events(&store, "comment.created").await, 1);
    // The agent's own comment must not start a run: no mention was made.
    assert_eq!(count_events(&store, "mention.created").await, 0);
    check(&store).await;

    // A second start-up finds nothing active and changes nothing.
    let events_before = count_events(&store, "run.finished").await;
    assert!(reconcile(&store, HashMap::new()).await.is_empty());
    assert_eq!(comments_of(&store, 1).await.len(), 1);
    assert_eq!(count_events(&store, "issue.moved").await, 2);
    assert_eq!(count_events(&store, "run.finished").await, events_before);
    let again = store.read(move |c| get_run(c, run)).await.unwrap().run;
    assert_eq!(again.reaped, found.reaped);
    check(&store).await;
    store.close().await;
}

#[tokio::test]
async fn a_card_a_person_already_moved_stays_and_a_missing_note_says_not_checked() {
    let (_dir, store, agent) = world().await;
    let done = column_of(&store, StateType::Completed).await;
    move_to(&store, 2, done).await;
    let run = running(&store, agent, 2, 5, None).await;
    // Issue 3 is in Backlog, never started.
    let other = running(&store, agent, 3, 6, None).await;

    assert_eq!(reconcile(&store, HashMap::new()).await, vec![run, other]);
    assert_eq!(column_name_of(&store, 2).await, "Done");
    assert_eq!(column_name_of(&store, 3).await, "Backlog");
    assert_eq!(
        count_events(&store, "issue.moved").await,
        1,
        "only the setup move"
    );
    for number in [2, 3] {
        let comments = comments_of(&store, number).await;
        assert_eq!(comments.len(), 1);
        assert!(
            !comments[0].1.contains("card"),
            "no card talk when nothing moved: {}",
            comments[0].1
        );
        assert!(comments[0].1.contains(runs::NOT_CHECKED));
    }
    let found = store.read(move |c| get_run(c, run)).await.unwrap().run;
    assert_eq!(found.reaped.as_deref(), Some(runs::NOT_CHECKED));
    check(&store).await;
    store.close().await;
}

#[tokio::test]
async fn without_an_unstarted_column_the_card_stays_and_the_comment_says_so() {
    let (_dir, store, agent) = world().await;
    let started = column_of(&store, StateType::Started).await;
    move_to(&store, 1, started).await;
    let todo = column_of(&store, StateType::Unstarted).await;
    // Move every card out of Todo's way and delete the column: no unstarted
    // column is left.
    store
        .write(move |tx| {
            tx.execute("DELETE FROM \"column\" WHERE id = ?1", [todo])?;
            Ok(())
        })
        .await
        .unwrap();
    let run = running(&store, agent, 1, 9, Some(1)).await;
    reconcile(
        &store,
        HashMap::from([(run, "process already gone".into())]),
    )
    .await;
    assert_eq!(column_name_of(&store, 1).await, "In Progress");
    let comments = comments_of(&store, 1).await;
    assert!(
        comments[0].1.contains(
            "The card stays where it is: the project has no column of the unstarted kind."
        ),
        "{}",
        comments[0].1
    );
    store.close().await;
}

#[tokio::test]
async fn a_kept_working_copy_is_named_by_branch_only() {
    let (_dir, store, agent) = world().await;
    let run = store
        .write(move |tx| {
            let created = create_run(
                tx,
                10,
                None,
                NewRun {
                    agent_id: agent,
                    project: "TRK".into(),
                    number: 1,
                    reason: RunReason::Manual,
                    trigger_event_id: None,
                    message: Some("hello".into()),
                    template_source: Some("inline".into()),
                    template_id: None,
                    edited: true,
                    boot_id: "boot-1".into(),
                    failed: None,
                    worktree_root: Some("C:\\secret\\data".into()),
                    base_branch: None,
                    comment_id: None,
                    limits: Some(Limits::default()),
                },
            )?;
            // Without a repository nothing is planned; plan one by hand.
            tx.execute(
                "UPDATE run SET worktree_state = 'created', worktree_path = 'C:\\secret\\data\\worktrees\\1',
                                worktree_repo = 'C:\\secret\\repo', branch = 'aurora/TRK-1-run1'
                 WHERE id = ?1",
                [created.run.id],
            )?;
            mark_running_process(tx, 11, created.run.id, 5, Some(2))?;
            Ok(created.run.id)
        })
        .await
        .unwrap();
    reconcile(
        &store,
        HashMap::from([(run, "pid reused, left alone".into())]),
    )
    .await;
    let comment = &comments_of(&store, 1).await[0].1;
    assert!(
        comment.contains("Working copy: kept on branch aurora/TRK-1-run1."),
        "{comment}"
    );
    assert!(!comment.contains("secret"), "{comment}");
    // (No invariant check: the plan was written by hand, past the event log.)
    store.close().await;
}

/// Runs 1 (reconciled, with a process identity) exists; `sql` damages a copy.
async fn corrupted(sql: &'static str) -> invariants::Report {
    let (dir, store, agent) = world().await;
    let run = running(&store, agent, 1, 5, Some(77)).await;
    reconcile(
        &store,
        HashMap::from([(run, "process already gone".into())]),
    )
    .await;
    store.close().await;
    let conn = Connection::open(dir.path().join("test.db")).unwrap();
    conn.execute_batch(&format!("PRAGMA foreign_keys = OFF; {sql}"))
        .unwrap();
    invariants::check(&conn)
}

fn reconciled_details(report: &invariants::Report) -> String {
    let check = report.get("runs.reconciled").unwrap();
    assert!(!check.ok, "runs.reconciled should fail: {report:#?}");
    check.details.clone()
}

#[tokio::test]
async fn the_reconciled_invariant_notices_a_missing_or_misplaced_note() {
    let clean = corrupted("SELECT 1;").await;
    assert!(clean.ok(), "{:#?}", clean.failed());
    assert_eq!(
        clean.get("runs.reconciled").unwrap().details,
        "1 runs with a leftover-process note"
    );

    let details =
        reconciled_details(&corrupted("UPDATE run SET reaped = NULL WHERE id = 1;").await);
    assert!(
        details.contains("run 1") && details.contains("says nothing"),
        "{details}"
    );

    let details = reconciled_details(&corrupted("UPDATE run SET pid = NULL WHERE id = 1;").await);
    assert!(details.contains("without a pid"), "{details}");

    // A note on a run that is not interrupted. (Keeps the rest consistent
    // enough that only this invariant is under test.)
    let report = corrupted("UPDATE run SET status = 'failed' WHERE id = 1;").await;
    let details = reconciled_details(&report);
    assert!(details.contains("leftover-process note"), "{details}");

    // An old row (no start time, no note) is never flagged.
    let old = corrupted("UPDATE run SET reaped = NULL, pid_started_at = NULL WHERE id = 1;").await;
    assert!(old.get("runs.reconciled").unwrap().ok);
}

#[tokio::test]
async fn the_log_replays_to_the_tables_through_a_reconciliation() {
    let (_dir, store, agent) = world().await;
    let started = column_of(&store, StateType::Started).await;
    move_to(&store, 1, started).await;
    let run = running(&store, agent, 1, 5, Some(5)).await;
    reconcile(
        &store,
        HashMap::from([(run, "killed leftover process 5".into())]),
    )
    .await;
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
    assert!(report.get("events.state_agreement").unwrap().ok);
    store.close().await;
}
