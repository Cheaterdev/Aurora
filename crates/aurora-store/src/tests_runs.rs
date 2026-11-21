//! Runs (TRK-15): lifecycle, the one-active-run rule, redelivery, the per-run
//! key, events, replay, invariants and start-up reconciliation.

use rusqlite::Connection;

use crate::agents::{self, NewAgent};
use crate::auth::{self, Principal, Role, Scope};
use crate::events::{self, EventFilter};
use crate::invariants;
use crate::projects::Rejection;
use crate::runs::{
    self, NewRun, Outcome, RunFilter, RunReason, RunStatus, create_run, finish_run, get_run,
    mark_running,
};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::{projects, test_actor};
use crate::{Store, StoreError};

const SECRET: &str = "SENTINEL-secret-message";

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(r) => r,
        other => panic!("expected a rejection, got {other}"),
    }
}

/// Project TRK with issues 1 and 2, an enabled agent `bot` allowed in TRK;
/// returns the store and the agent id.
async fn world() -> (tempfile::TempDir, Store, i64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let agent = store
        .write(|tx| {
            let actor = test_actor(tx);
            projects::create_project(tx, 1, "TRK", "Tracker", None)?;
            projects::create_issue_type(tx, 1, "TRK", "Task")?;
            for n in 0..2 {
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

fn new_run(agent: i64, number: i64, event: Option<i64>) -> NewRun {
    NewRun {
        agent_id: agent,
        project: "TRK".into(),
        number,
        reason: RunReason::Assigned,
        trigger_event_id: event,
        message: Some(format!("hello {SECRET}")),
        template_source: Some("builtin".into()),
        template_id: None,
        edited: false,
        boot_id: "boot-1".into(),
        failed: None,
    }
}

fn key_live(conn: &Connection, secret: &str) -> bool {
    auth::authenticate_key(conn, 100, secret).unwrap().is_some()
}

#[tokio::test]
async fn a_run_goes_through_its_lifecycle_and_its_key_dies_with_it() {
    let (_dir, store, agent) = world().await;
    let created = store
        .write(move |tx| create_run(tx, 10, None, new_run(agent, 1, None)))
        .await
        .unwrap();
    let secret = created.key.clone().unwrap();
    let id = created.run.id;
    assert_eq!(created.run.status, RunStatus::Starting);
    assert_eq!(created.run.issue, "TRK-1");
    let s = secret.clone();
    assert!(store.read(move |c| Ok(key_live(c, &s))).await.unwrap());
    // The key authenticates as the agent's user.
    let s = secret.clone();
    let who = store
        .read(move |c| auth::authenticate_key(c, 100, &s))
        .await
        .unwrap()
        .unwrap();
    let user = store
        .read(move |c| agents::agent_config(c, agent))
        .await
        .unwrap()
        .user_id;
    assert_eq!(who.principal.user_id, user);
    assert_eq!(who.principal.scope, Scope::Write);

    store
        .write(move |tx| mark_running(tx, 11, id, 4242))
        .await
        .unwrap();
    let again = store.write(move |tx| mark_running(tx, 12, id, 1)).await;
    assert!(matches!(
        rejection(again.unwrap_err()),
        Rejection::Conflict(_)
    ));

    let finished = store
        .write(move |tx| {
            let mut outcome = Outcome::new(RunStatus::Succeeded);
            outcome.exit_code = Some(0);
            outcome.output_tail = "out".into();
            finish_run(tx, 20, None, id, outcome)
        })
        .await
        .unwrap();
    assert!(finished);
    let s = secret.clone();
    assert!(!store.read(move |c| Ok(key_live(c, &s))).await.unwrap());
    let again = store
        .write(move |tx| finish_run(tx, 21, None, id, Outcome::new(RunStatus::Failed)))
        .await
        .unwrap();
    assert!(!again, "finishing twice changes nothing");

    let detail = store.read(move |c| get_run(c, id)).await.unwrap();
    assert_eq!(detail.run.status, RunStatus::Succeeded);
    assert_eq!(detail.run.pid, Some(4242));
    assert_eq!(detail.run.exit_code, Some(0));
    assert_eq!(detail.output_tail, "out");
    assert!(detail.message.contains(SECRET));
    let json = serde_json::to_string(&detail.run).unwrap();
    assert!(!json.contains(SECRET), "Run never carries the message");

    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
    store.close().await;
}

#[tokio::test]
async fn one_active_run_per_issue_and_events_are_deduplicated() {
    let (_dir, store, agent) = world().await;
    let first = store
        .write(move |tx| create_run(tx, 10, None, new_run(agent, 1, None)))
        .await
        .unwrap();
    let second = store
        .write(move |tx| create_run(tx, 11, None, new_run(agent, 1, None)))
        .await;
    assert!(matches!(
        rejection(second.unwrap_err()),
        Rejection::Conflict(_)
    ));
    // Another issue is fine.
    store
        .write(move |tx| create_run(tx, 12, None, new_run(agent, 2, None)))
        .await
        .unwrap();
    let id = first.run.id;
    store
        .write(move |tx| finish_run(tx, 13, None, id, Outcome::new(RunStatus::Failed)))
        .await
        .unwrap();
    // After it finished the issue can run again; the same event twice is one run.
    let a = store
        .write(move |tx| create_run(tx, 14, None, new_run(agent, 1, Some(1))))
        .await
        .unwrap();
    assert!(!a.duplicate);
    let a_id = a.run.id;
    store
        .write(move |tx| finish_run(tx, 15, None, a_id, Outcome::new(RunStatus::Failed)))
        .await
        .unwrap();
    let b = store
        .write(move |tx| create_run(tx, 16, None, new_run(agent, 1, Some(1))))
        .await
        .unwrap();
    assert!(b.duplicate && b.key.is_none());
    assert_eq!(b.run.id, a_id);
    let runs = store
        .read(|c| runs::list_runs(c, &RunFilter::default()))
        .await
        .unwrap();
    assert_eq!(runs.len(), 3);
    let filtered = store
        .read(|c| {
            runs::list_runs(
                c,
                &RunFilter {
                    issue: Some(("TRK".into(), 2)),
                    ..RunFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    let none = store
        .read(|c| {
            runs::list_runs(
                c,
                &RunFilter {
                    allowed_projects: Some(vec![]),
                    ..RunFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(none.is_empty());
    store.close().await;
}

#[tokio::test]
async fn invalid_requests_are_refused_and_leave_nothing() {
    let (_dir, store, agent) = world().await;
    let mut bad = new_run(agent, 99, None);
    let err = store
        .write(move |tx| create_run(tx, 10, None, bad.clone()))
        .await;
    assert!(matches!(rejection(err.unwrap_err()), Rejection::Invalid(_)));
    bad = new_run(agent, 1, None);
    bad.project = "NOPE".into();
    let err = store.write(move |tx| create_run(tx, 10, None, bad)).await;
    assert!(matches!(rejection(err.unwrap_err()), Rejection::Invalid(_)));
    let err = store
        .write(move |tx| {
            let actor = test_actor(tx);
            agents::set_enabled(tx, 11, &actor, agent, false)?;
            create_run(tx, 12, None, new_run(agent, 1, None))
        })
        .await;
    assert!(matches!(rejection(err.unwrap_err()), Rejection::Invalid(_)));
    let count: i64 = store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM run", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0, "a rolled back creation leaves no row");
    let keys: i64 = store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM api_key", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(keys, 0, "and no key");
    store.close().await;
}

#[tokio::test]
async fn a_failed_creation_has_no_key_and_events_never_carry_text() {
    let (_dir, store, agent) = world().await;
    let mut failed = new_run(agent, 1, None);
    failed.message = None;
    failed.failed = Some("prompt template invalid: nope".into());
    let created = store
        .write(move |tx| create_run(tx, 10, None, failed))
        .await
        .unwrap();
    assert!(created.key.is_none());
    assert_eq!(created.run.status, RunStatus::Failed);

    let id = store
        .write(move |tx| create_run(tx, 11, None, new_run(agent, 2, None)))
        .await
        .unwrap()
        .run
        .id;
    store
        .write(move |tx| {
            mark_running(tx, 12, id, 7)?;
            let mut outcome = Outcome::new(RunStatus::Failed).error("exit 1");
            outcome.exit_code = Some(1);
            outcome.output_tail = format!("printed {SECRET}");
            finish_run(tx, 13, None, id, outcome)?;
            Ok(())
        })
        .await
        .unwrap();
    let log = store
        .read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
        .await
        .unwrap();
    let kinds: Vec<&str> = log
        .iter()
        .filter(|e| e.entity_kind == "run")
        .map(|e| e.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "run.created",
            "run.finished",
            "run.created",
            "run.started",
            "run.finished"
        ]
    );
    for event in &log {
        assert!(
            !event.payload.to_string().contains(SECRET),
            "{}",
            event.kind
        );
        if event.entity_kind == "run" {
            assert_eq!(event.project.as_deref(), Some("TRK"));
        }
    }
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
    store.close().await;
}

#[tokio::test]
async fn reconcile_interrupts_runs_of_another_boot_only() {
    let (_dir, store, agent) = world().await;
    let old = store
        .write(move |tx| create_run(tx, 10, None, new_run(agent, 1, None)))
        .await
        .unwrap();
    let id = old.run.id;
    store
        .write(move |tx| mark_running(tx, 11, id, 5))
        .await
        .unwrap();
    let mut mine = new_run(agent, 2, None);
    mine.boot_id = "boot-2".into();
    let mine = store
        .write(move |tx| create_run(tx, 12, None, mine))
        .await
        .unwrap();

    let done = store
        .write(|tx| runs::reconcile_startup(tx, 20, "boot-2"))
        .await
        .unwrap();
    assert_eq!(done, [id]);
    let detail = store.read(move |c| get_run(c, id)).await.unwrap();
    assert_eq!(detail.run.status, RunStatus::Interrupted);
    assert_eq!(detail.run.error.as_deref(), Some("server restarted"));
    let secret = old.key.unwrap();
    assert!(!store.read(move |c| Ok(key_live(c, &secret))).await.unwrap());
    let secret = mine.key.unwrap();
    assert!(store.read(move |c| Ok(key_live(c, &secret))).await.unwrap());
    let report = store
        .read(|c| Ok(invariants::check_with_boot(c, "boot-2")))
        .await
        .unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
    let report = store
        .read(|c| Ok(invariants::check_with_boot(c, "boot-3")))
        .await
        .unwrap();
    assert!(
        !report.get("runs.no_orphans").unwrap().ok,
        "a foreign boot owns an active run"
    );

    let swept = store
        .write(|tx| runs::interrupt_boot(tx, 30, "boot-2"))
        .await
        .unwrap();
    assert_eq!(swept, [mine.run.id]);
    store.close().await;
}

#[tokio::test]
async fn deleting_an_agent_with_runs_is_refused() {
    let (_dir, store, agent) = world().await;
    store
        .write(move |tx| create_run(tx, 10, None, new_run(agent, 1, None)))
        .await
        .unwrap();
    let err = store
        .write(move |tx| {
            let actor = Principal {
                user_id: 1,
                role: Role::Owner,
                scope: Scope::Write,
            };
            tx.execute(
                "INSERT OR IGNORE INTO user (id, name, kind, role) VALUES (1, 'o', 'human', 'owner')",
                [],
            )?;
            agents::delete_agent(tx, 11, &actor, agent)
        })
        .await;
    assert!(matches!(
        rejection(err.unwrap_err()),
        Rejection::Conflict(_)
    ));
    store.close().await;
}

async fn corrupted(sql: &'static str) -> invariants::Report {
    let (dir, store, agent) = world().await;
    let created = store
        .write(move |tx| create_run(tx, 10, None, new_run(agent, 1, None)))
        .await
        .unwrap();
    let id = created.run.id;
    store
        .write(move |tx| mark_running(tx, 11, id, 5))
        .await
        .unwrap();
    store
        .write(move |tx| create_run(tx, 12, None, new_run(agent, 2, None)))
        .await
        .unwrap();
    store
        .write(move |tx| {
            finish_run(tx, 13, None, id, Outcome::new(RunStatus::Succeeded))?;
            Ok(())
        })
        .await
        .unwrap();
    store.close().await;
    let conn = Connection::open(dir.path().join("test.db")).unwrap();
    conn.execute_batch(&format!("PRAGMA foreign_keys = OFF; {sql}"))
        .unwrap();
    invariants::check(&conn)
}

fn failed(report: &invariants::Report, name: &str) -> String {
    let check = report.get(name).unwrap();
    assert!(!check.ok, "{name} should fail: {report:#?}");
    check.details.clone()
}

#[tokio::test]
async fn corruption_of_runs_is_noticed() {
    // run 2 is active (starting) with a live key; run 1 is finished.
    let clean = corrupted("SELECT 1;").await;
    assert!(clean.ok(), "{:#?}", clean.failed());

    let details = corrupted(
        "UPDATE api_key SET revoked = 5 WHERE id = (SELECT key_id FROM run WHERE id = 2);",
    )
    .await;
    assert!(failed(&details, "runs.no_orphans").contains("run 2"));
    let details = corrupted("UPDATE run SET key_id = NULL WHERE id = 2;").await;
    assert!(failed(&details, "runs.no_orphans").contains("no live key"));
    let details = corrupted(
        "UPDATE api_key SET revoked = NULL WHERE id = (SELECT key_id FROM run WHERE id = 1);",
    )
    .await;
    assert!(failed(&details, "runs.no_orphans").contains("still live"));
    let details = corrupted("UPDATE run SET status = 'running' WHERE id = 2;").await;
    assert!(failed(&details, "runs.no_orphans").contains("without a pid"));
    let details = corrupted("UPDATE run SET status = 'failed', finished = 9 WHERE id = 2;").await;
    // Replay disagrees with the table: no event says run 2 failed.
    assert!(failed(&details, "events.state_agreement").contains("run"));
    failed(&details, "runs.no_orphans");
    let details = corrupted("DELETE FROM run WHERE id = 1;").await;
    assert!(failed(&details, "events.state_agreement").contains("run"));
}
