//! Migration 0017 and the run queue (TRK-18): queueing over the limits, oldest
//! first promotion, checks before a queued run starts, the event log and the
//! `runs.queue` invariant.

use rusqlite::Connection;

use crate::agents::{self, AgentPatch, NewAgent};
use crate::invariants;
use crate::migrations::MIGRATIONS;
use crate::projects::Rejection;
use crate::runs::{
    self, Limits, NewRun, Outcome, RunReason, RunStatus, create_run, finish_run, get_run,
    mark_running, schedule,
};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::{projects, test_actor};
use crate::{OpenOptions, Store, StoreError};

#[tokio::test]
async fn populated_version_sixteen_database_gets_the_queue_columns_and_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let v16 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..16]).unwrap();
    assert_eq!(v16.schema_version(), 16);
    v16.close().await;
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(
        "INSERT INTO user (id, name, kind, role) VALUES
             (1, 'Owner', 'human', 'owner'), (2, 'bot', 'agent', 'agent');
         INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'AA', 'First', 10, 3);
         INSERT INTO \"column\" (id, project_id, name, state_type, position)
             VALUES (1, 1, 'Todo', 'unstarted', 0);
         INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Task');
         INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
             VALUES (1, 1, 1, 1, 1, 'one', 1.0, 1, 1), (2, 1, 2, 1, 1, 'two', 2.0, 1, 1);
         INSERT INTO agent (id, user_id, command, created, updated) VALUES (1, 2, 'x', 1, 1);
         INSERT INTO run (id, agent_id, issue_id, project_id, reason, status, boot_id, message,
                          created, finished)
             VALUES (7, 1, 1, 1, 'manual', 'succeeded', 'old', 'hello', 11, 12);
         INSERT INTO run (id, agent_id, issue_id, project_id, reason, status, boot_id, message,
                          created, started)
             VALUES (8, 1, 2, 1, 'manual', 'running', 'old', 'again', 13, 14);",
    )
    .unwrap();
    drop(raw);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version(), 17);
    let backup = store.open_report().backup.clone().expect("backup taken");
    let old = Connection::open(&backup).unwrap();
    let has_queued_at: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('run') WHERE name = 'queued_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_queued_at, 0, "the backup is the version-16 database");
    drop(old);

    for id in [7, 8] {
        let run = store.read(move |c| get_run(c, id)).await.unwrap().run;
        assert_eq!((run.queued_at, run.queue_position), (None, None));
    }
    assert_eq!(
        store.read(|c| get_run(c, 8)).await.unwrap().run.status,
        RunStatus::Running
    );
    // `queued` is a status now, and it needs its queue time.
    let bad = store
        .write(|tx| {
            tx.execute(
                "INSERT INTO run (agent_id, issue_id, project_id, reason, status, boot_id, message, created)
                 VALUES (1, 1, 1, 'manual', 'queued', 'b', 'm', 20)",
                [],
            )?;
            Ok(())
        })
        .await;
    assert!(bad.is_err(), "queued without queued_at is refused");
    // The unique "one active run per issue" index counts a queued run.
    let dup = store
        .write(|tx| {
            tx.execute(
                "INSERT INTO run (agent_id, issue_id, project_id, reason, status, boot_id, message, created, queued_at)
                 VALUES (1, 2, 1, 'manual', 'queued', 'b', 'm', 20, 20)",
                [],
            )?;
            Ok(())
        })
        .await;
    assert!(dup.is_err(), "issue 2 already has an active run");
    let ok = store
        .write(|tx| {
            tx.execute(
                "INSERT INTO run (agent_id, issue_id, project_id, reason, status, boot_id, message, created, queued_at)
                 VALUES (1, 1, 1, 'manual', 'queued', 'b', 'm', 20, 20)",
                [],
            )?;
            Ok(())
        })
        .await;
    assert!(ok.is_ok(), "issue 1 only has a finished run: {ok:?}");
    store.close().await;
}

/// Project TRK with six issues, an agent `bot` that may run one run at a time
/// and `other` that may run three, both allowed in TRK.
async fn world() -> (tempfile::TempDir, Store, i64, i64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let ids = store
        .write(|tx| {
            let actor = test_actor(tx);
            projects::create_project(tx, 1, "TRK", "Tracker", None)?;
            projects::create_issue_type(tx, 1, "TRK", "Task")?;
            for n in 0..6 {
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
            let mut ids = Vec::new();
            for (name, max) in [("bot", 1), ("other", 3)] {
                let agent = agents::create_agent(
                    tx,
                    3,
                    &actor,
                    NewAgent {
                        name: Some(name.into()),
                        command: "x".into(),
                        projects: vec!["TRK".into()],
                        max_concurrent: Some(max),
                        ..NewAgent::default()
                    },
                )?;
                ids.push(agent.id);
            }
            Ok(ids)
        })
        .await
        .unwrap();
    (dir, store, ids[0], ids[1])
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

fn limits(max_runs: i64) -> Limits {
    Limits {
        max_runs,
        ..Limits::default()
    }
}

async fn make(store: &Store, now: i64, agent: i64, number: i64, limits: Limits) -> runs::Run {
    store
        .write(move |tx| {
            let mut new = new_run(agent, number);
            new.limits = Some(limits);
            Ok(create_run(tx, now, None, new)?.run)
        })
        .await
        .unwrap()
}

async fn status(store: &Store, id: i64) -> RunStatus {
    store
        .read(move |c| get_run(c, id))
        .await
        .unwrap()
        .run
        .status
}

async fn pass(store: &Store, now: i64, limits: Limits) -> runs::Scheduled {
    store
        .write(move |tx| schedule(tx, now, "boot-2", None, &limits, true))
        .await
        .unwrap()
}

async fn check(store: &Store) {
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn a_run_over_the_limit_is_queued_with_a_position_and_no_key_or_working_copy() {
    let (_dir, store, bot, other) = world().await;
    let first = make(&store, 10, other, 1, limits(1)).await;
    assert_eq!(first.status, RunStatus::Starting);
    assert_eq!((first.queued_at, first.queue_position), (None, None));
    let second = make(&store, 11, other, 2, limits(1)).await;
    assert_eq!(second.status, RunStatus::Queued);
    assert_eq!(
        (second.queued_at, second.queue_position),
        (Some(11), Some(1))
    );
    let third = make(&store, 12, bot, 3, limits(1)).await;
    assert_eq!(third.queue_position, Some(2));
    assert!(third.pid.is_none() && third.started.is_none());
    let keys: i64 = store
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM run WHERE status = 'queued' AND key_id IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(keys, 0);
    // The position moves up when an older run leaves the queue.
    store
        .write(move |tx| {
            finish_run(tx, 13, None, second.id, Outcome::new(RunStatus::Canceled))?;
            Ok(())
        })
        .await
        .unwrap();
    let third_now = store.read(move |c| get_run(c, third.id)).await.unwrap().run;
    assert_eq!(third_now.queue_position, Some(1));
    check(&store).await;
}

#[tokio::test]
async fn without_limits_a_run_always_starts() {
    let (_dir, store, _, other) = world().await;
    let a = store
        .write(move |tx| Ok(create_run(tx, 10, None, new_run(other, 1))?.run))
        .await
        .unwrap();
    let b = store
        .write(move |tx| Ok(create_run(tx, 11, None, new_run(other, 2))?.run))
        .await
        .unwrap();
    assert_eq!(
        (a.status, b.status),
        (RunStatus::Starting, RunStatus::Starting)
    );
}

#[tokio::test]
async fn promotion_is_oldest_first_and_an_agent_at_its_limit_does_not_block_others() {
    let (_dir, store, bot, other) = world().await;
    let running = make(&store, 10, bot, 1, limits(2)).await;
    assert_eq!(running.status, RunStatus::Starting);
    // bot is at its limit of one; other has room and one slot is free overall.
    let waits = make(&store, 11, bot, 2, limits(2)).await;
    assert_eq!(waits.status, RunStatus::Queued);
    let direct = make(&store, 12, other, 3, limits(2)).await;
    assert_eq!(
        direct.status,
        RunStatus::Starting,
        "not stuck behind bot's run"
    );
    // Now both slots are taken: another run of `other` queues behind `waits`.
    let behind = make(&store, 13, other, 4, limits(2)).await;
    assert_eq!(behind.status, RunStatus::Queued);

    let nothing = pass(&store, 20, limits(2)).await;
    assert!(nothing.promoted.is_empty() && nothing.canceled.is_empty());

    // A slot frees up, but bot is still at its limit: `behind` goes first.
    store
        .write(move |tx| {
            finish_run(tx, 21, None, direct.id, Outcome::new(RunStatus::Succeeded))?;
            Ok(())
        })
        .await
        .unwrap();
    let moved = pass(&store, 22, limits(2)).await;
    let promoted: Vec<i64> = moved.promoted.iter().map(|p| p.created.run.id).collect();
    assert_eq!(promoted, vec![behind.id]);
    assert_eq!(status(&store, waits.id).await, RunStatus::Queued);
    let one = &moved.promoted[0];
    assert!(one.created.key.is_some(), "a key is minted when it starts");
    assert_eq!(one.created.run.status, RunStatus::Starting);
    assert_eq!(one.created.run.queued_at, Some(13), "the queue time stays");

    // Repeating the pass is harmless.
    let again = pass(&store, 23, limits(2)).await;
    assert!(again.promoted.is_empty());

    // bot's own run ends: its queued run starts next.
    store
        .write(move |tx| {
            finish_run(tx, 24, None, running.id, Outcome::new(RunStatus::Failed))?;
            Ok(())
        })
        .await
        .unwrap();
    let last = pass(&store, 25, limits(2)).await;
    assert_eq!(last.promoted.len(), 1);
    assert_eq!(last.promoted[0].created.run.id, waits.id);
    check(&store).await;
}

#[tokio::test]
async fn nothing_is_promoted_while_the_server_stops() {
    let (_dir, store, _, other) = world().await;
    make(&store, 10, other, 1, limits(1)).await;
    let queued = make(&store, 11, other, 2, limits(1)).await;
    store
        .write(|tx| {
            tx.execute(
                "UPDATE run SET status = 'interrupted', finished = 12 WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let stopping = store
        .write(|tx| schedule(tx, 13, "b", None, &limits(1), false))
        .await
        .unwrap();
    assert!(stopping.promoted.is_empty());
    assert_eq!(status(&store, queued.id).await, RunStatus::Queued);
}

#[tokio::test]
async fn a_full_queue_refuses_the_next_run_with_a_queue_full_conflict() {
    let (_dir, store, _, other) = world().await;
    let small = Limits {
        max_runs: 1,
        max_queued_per_agent: 2,
        max_queue_wait_ms: 0,
    };
    for n in 1..=3 {
        make(&store, 10 + n, other, n, small).await;
    }
    let refused = store
        .write(move |tx| {
            let mut new = new_run(other, 4);
            new.limits = Some(small);
            create_run(tx, 20, None, new).map(|c| c.run.id)
        })
        .await
        .unwrap_err();
    match refused {
        StoreError::Rejected(Rejection::Conflict(why)) => {
            assert!(runs::is_queue_full(&why), "{why}");
        }
        other => panic!("expected a conflict, got {other}"),
    }
    let count = store.read(runs::queued_count).await.unwrap();
    assert_eq!(count, 2);
    check(&store).await;
}

#[tokio::test]
async fn a_queued_run_is_checked_again_when_it_would_start() {
    let (_dir, store, bot, other) = world().await;
    let held = make(&store, 10, other, 1, limits(1)).await;
    let disabled = make(&store, 11, bot, 2, limits(1)).await;
    let archived = make(&store, 12, other, 3, limits(1)).await;
    let denied = make(&store, 13, other, 4, limits(1)).await;
    let fine = make(&store, 14, other, 5, limits(1)).await;
    assert!(
        [&disabled, &archived, &denied, &fine]
            .iter()
            .all(|r| r.status == RunStatus::Queued)
    );
    store
        .write(move |tx| {
            let actor = test_actor(tx);
            agents::set_enabled(tx, 15, &actor, bot, false)?;
            issues::archive_issue(tx, 16, "TRK", 3)?;
            Ok(())
        })
        .await
        .unwrap();
    // `denied` loses its access through its agent: an agent with the allow-list
    // emptied (other) would cancel every run of other, so use a second pass
    // after the first has settled.
    let first = pass(&store, 20, limits(1)).await;
    let mut canceled = first.canceled.clone();
    canceled.sort();
    assert_eq!(
        canceled,
        vec![
            (disabled.id, "agent disabled".to_string()),
            (archived.id, "issue archived".to_string()),
        ]
    );
    assert!(first.promoted.is_empty(), "the held run still has the slot");
    for run in [&disabled, &archived] {
        let got = store
            .read({
                let id = run.id;
                move |c| get_run(c, id)
            })
            .await
            .unwrap()
            .run;
        assert_eq!(got.status, RunStatus::Canceled);
        assert!(got.pid.is_none() && got.started.is_none());
        assert!(got.finished.is_some());
        assert!(got.queue_position.is_none());
    }
    // The agent loses the project: what it still has queued ends.
    store
        .write(move |tx| {
            let actor = test_actor(tx);
            agents::update_agent(
                tx,
                21,
                &actor,
                other,
                AgentPatch {
                    projects: Some(vec![]),
                    ..AgentPatch::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let second = pass(&store, 22, limits(1)).await;
    let mut canceled: Vec<(i64, String)> = second.canceled.clone();
    canceled.sort();
    assert_eq!(
        canceled,
        vec![
            (
                denied.id,
                "agent no longer has access to project TRK".to_string()
            ),
            (
                fine.id,
                "agent no longer has access to project TRK".to_string()
            ),
        ]
    );
    assert_eq!(status(&store, held.id).await, RunStatus::Starting);
    check(&store).await;
}

#[tokio::test]
async fn a_queued_run_that_waited_too_long_is_canceled_not_started() {
    let (_dir, store, _, other) = world().await;
    make(&store, 1_000, other, 1, limits(1)).await;
    let old = make(&store, 1_001, other, 2, limits(1)).await;
    let young = make(&store, 1_900, other, 3, limits(1)).await;
    let patient = Limits {
        max_queue_wait_ms: 500,
        ..limits(1)
    };
    let done = pass(&store, 1_600, patient).await;
    assert_eq!(
        done.canceled,
        vec![(old.id, "queue wait exceeded".to_string())]
    );
    assert!(done.promoted.is_empty());
    assert_eq!(status(&store, young.id).await, RunStatus::Queued);
    // 0 means never.
    let never = pass(&store, 9_000_000, limits(1)).await;
    assert!(never.canceled.is_empty());
    check(&store).await;
}

#[tokio::test]
async fn the_log_replays_to_the_tables_through_queue_promote_and_cancel() {
    let (_dir, store, bot, other) = world().await;
    let a = make(&store, 10, other, 1, limits(1)).await;
    let b = make(&store, 11, other, 2, limits(1)).await;
    let c = make(&store, 12, bot, 3, limits(1)).await;
    store
        .write(move |tx| {
            mark_running(tx, 13, a.id, 99)?;
            finish_run(tx, 14, None, a.id, Outcome::new(RunStatus::Succeeded))?;
            Ok(())
        })
        .await
        .unwrap();
    let moved = pass(&store, 15, limits(1)).await;
    assert_eq!(moved.promoted.len(), 1);
    assert_eq!(moved.promoted[0].created.run.id, b.id);
    store
        .write(move |tx| {
            finish_run(
                tx,
                16,
                Some(crate::tests_support::TEST_USER_ID),
                c.id,
                Outcome::new(RunStatus::Canceled),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    check(&store).await;
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.get("events.state_agreement").unwrap().ok);
    assert!(report.get("runs.queue").unwrap().ok);
}

/// Runs 1 (starting) and 2 (queued) exist; `sql` damages a copy.
async fn corrupted(sql: &'static str) -> invariants::Report {
    let (dir, store, _, other) = world().await;
    make(&store, 10, other, 1, limits(1)).await;
    make(&store, 11, other, 2, limits(1)).await;
    store.close().await;
    let conn = Connection::open(dir.path().join("test.db")).unwrap();
    conn.execute_batch(&format!("PRAGMA foreign_keys = OFF; {sql}"))
        .unwrap();
    invariants::check(&conn)
}

fn queue_details(report: &invariants::Report) -> String {
    let check = report.get("runs.queue").unwrap();
    assert!(!check.ok, "runs.queue should fail: {report:#?}");
    check.details.clone()
}

#[tokio::test]
async fn the_queue_invariant_notices_a_queued_run_that_is_not_one() {
    let clean = corrupted("SELECT 1;").await;
    assert!(clean.ok(), "{:#?}", clean.failed());
    assert_eq!(clean.get("runs.queue").unwrap().details, "1 queued");

    let details = queue_details(&corrupted("UPDATE run SET pid = 5 WHERE id = 2;").await);
    assert!(
        details.contains("run 2") && details.contains("process"),
        "{details}"
    );
    let details = queue_details(&corrupted("UPDATE run SET started = 12 WHERE id = 2;").await);
    assert!(
        details.contains("run 2") && details.contains("started"),
        "{details}"
    );
    let details =
        queue_details(&corrupted("UPDATE run SET worktree_state = 'created' WHERE id = 2;").await);
    assert!(details.contains("working copy"), "{details}");
    let details = queue_details(&corrupted("UPDATE run SET output_tail = 'x' WHERE id = 2;").await);
    assert!(details.contains("transcript"), "{details}");
    let details = queue_details(
        &corrupted(
            "DROP TRIGGER event_no_delete;
             DELETE FROM event WHERE entity_kind = 'run' AND entity_id = 2 AND kind = 'run.created';",
        )
        .await,
    );
    assert!(details.contains("no run.created event"), "{details}");

    // A run that was started on paper while the row still says queued.
    let report = corrupted(
        "INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
         VALUES (30, NULL, 1, 'run', 2, 'run.promoted', '{\"branch\":null,\"worktree_state\":\"none\"}');",
    )
    .await;
    let details = queue_details(&report);
    assert!(details.contains("log says it was started"), "{details}");
    assert!(
        !report.get("events.state_agreement").unwrap().ok,
        "replay and table disagree too"
    );

    // The table says queued while the log says it was started and finished.
    let report =
        corrupted("UPDATE run SET status = 'running', started = 12, queued_at = 11 WHERE id = 2;")
            .await;
    assert!(!report.ok(), "a status the log never produced is found");
}
