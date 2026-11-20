//! The event log and consumer cursors: reading, filters, cursors, restart,
//! replay and immutability.

use std::collections::BTreeMap;

use rusqlite::Transaction;
use serde_json::Value;

use crate::auth::{Principal, Role, Scope};
use crate::comments::{self, NewComment};
use crate::events::{self, Event, EventFilter, Start};
use crate::projects::{NewColumn, Rejection, StateType};
use crate::tests_support::initiatives::{self, InitiativePatch, InitiativeStatus, NewInitiative};
use crate::tests_support::issues::{self, IssuePatch, MoveTarget, NewIssue, Priority};
use crate::tests_support::links::{self, LinkKind};
use crate::tests_support::projects;
use crate::{Store, StoreError};

fn is_not_found(err: &StoreError) -> bool {
    matches!(err, StoreError::Rejected(Rejection::NotFound(_)))
}

fn is_invalid(err: &StoreError) -> bool {
    matches!(err, StoreError::Rejected(Rejection::Invalid(_)))
}

async fn run<T, F>(store: &Store, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
{
    store.write(f).await.unwrap()
}

async fn all_events(store: &Store) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let after = all.last().map_or(0, |e: &Event| e.id);
        let page = store
            .read(move |c| events::read_events(c, after, Some(500), &EventFilter::default()))
            .await
            .unwrap();
        if page.is_empty() {
            return all;
        }
        all.extend(page);
    }
}

async fn poll_ids(store: &Store, name: &'static str) -> Vec<i64> {
    store
        .read(move |c| events::poll(c, name, None, &EventFilter::default()))
        .await
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect()
}

/// Project TRK, one issue type and `issues` issues: `2 + issues` events.
async fn seed(store: &Store, issues: usize) {
    run(store, |tx| {
        projects::create_project(tx, 1, "TRK", "Tracker", None)
    })
    .await;
    run(store, |tx| {
        projects::create_issue_type(tx, 1, "TRK", "Task")
    })
    .await;
    for n in 0..issues {
        run(store, move |tx| {
            issues::create_issue(
                tx,
                1000 + n as i64,
                "TRK",
                NewIssue {
                    title: format!("issue {n}"),
                    ..NewIssue::default()
                },
            )
        })
        .await;
    }
}

fn open() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

// ---- reading ----

#[tokio::test]
async fn events_are_ordered_and_carry_their_fields() {
    let (_dir, _path, store) = open();
    seed(&store, 2).await;
    let list = all_events(&store).await;
    let ids: Vec<i64> = list.iter().map(|e| e.id).collect();
    assert_eq!(ids, [1, 2, 3, 4]);
    let kinds: Vec<&str> = list.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "project.created",
            "issue_type.created",
            "issue.created",
            "issue.created"
        ]
    );
    let last = &list[3];
    assert_eq!(last.project.as_deref(), Some("TRK"));
    assert_eq!(last.entity_kind, "issue");
    assert_eq!(last.actor, Some(crate::tests_support::TEST_USER_ID));
    assert_eq!(last.created, 1001);
    assert_eq!(last.payload["title"], "issue 1");
    assert_eq!(last.payload["identifier"], "TRK-2");

    let after_two = store
        .read(|c| events::read_events(c, 2, None, &EventFilter::default()))
        .await
        .unwrap();
    assert_eq!(after_two.iter().map(|e| e.id).collect::<Vec<_>>(), [3, 4]);
    let latest = store.read(events::latest_event_id).await.unwrap();
    assert_eq!(latest, 4);
    store.close().await;
}

#[tokio::test]
async fn latest_event_id_is_zero_for_an_empty_log() {
    let (_dir, _path, store) = open();
    assert_eq!(store.read(events::latest_event_id).await.unwrap(), 0);
    let none = store
        .read(|c| events::read_events(c, 0, None, &EventFilter::default()))
        .await
        .unwrap();
    assert!(none.is_empty());
    store.close().await;
}

#[tokio::test]
async fn limit_is_clamped_and_negative_after_is_refused() {
    let (_dir, _path, store) = open();
    run(&store, |tx| {
        tx.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
             INSERT INTO event (time, entity_kind, entity_id, kind, payload_json)
             SELECT i, 'thing', i, 'thing.made', '{}' FROM n;",
        )?;
        Ok(())
    })
    .await;
    let count = |limit: Option<i64>| {
        let store = store.clone();
        async move {
            store
                .read(move |c| events::read_events(c, 0, limit, &EventFilter::default()))
                .await
                .unwrap()
                .len()
        }
    };
    assert_eq!(count(None).await, 100, "default");
    assert_eq!(count(Some(0)).await, 1, "raised to 1");
    assert_eq!(count(Some(-7)).await, 1, "raised to 1");
    assert_eq!(count(Some(250)).await, 250);
    assert_eq!(count(Some(10_000)).await, 500, "capped at 500");

    let err = store
        .read(|c| events::read_events(c, -1, None, &EventFilter::default()))
        .await
        .unwrap_err();
    assert!(is_invalid(&err), "{err}");
    store.close().await;
}

#[tokio::test]
async fn filters_by_project_entity_kind_and_kind_prefix() {
    let (_dir, _path, store) = open();
    seed(&store, 3).await;
    run(&store, |tx| {
        projects::create_project(tx, 2, "OTH", "Other", None)
    })
    .await;
    run(&store, |tx| issues::archive_issue(tx, 3, "TRK", 1)).await;

    let read = |filter: EventFilter| {
        let store = store.clone();
        async move {
            store
                .read(move |c| events::read_events(c, 0, None, &filter))
                .await
        }
    };
    let ids = |list: Vec<Event>| list.iter().map(|e| e.id).collect::<Vec<_>>();

    let oth = read(EventFilter {
        project: Some("OTH".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap();
    assert_eq!(ids(oth), [6]);
    let trk = read(EventFilter {
        project: Some("TRK".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap();
    assert_eq!(ids(trk), [1, 2, 3, 4, 5, 7]);
    let missing = read(EventFilter {
        project: Some("NOPE".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap_err();
    assert!(is_not_found(&missing));

    let issue_entities = read(EventFilter {
        entity_kind: Some("issue".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap();
    assert_eq!(ids(issue_entities), [3, 4, 5, 7]);

    let dotted = read(EventFilter {
        kind_prefix: Some("issue.".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap();
    assert_eq!(
        ids(dotted),
        [3, 4, 5, 7],
        "issue_type. does not start with issue."
    );
    let loose = read(EventFilter {
        kind_prefix: Some("issue".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap();
    assert_eq!(ids(loose), [2, 3, 4, 5, 7]);
    let wildcard = read(EventFilter {
        kind_prefix: Some("%".into()),
        ..EventFilter::default()
    })
    .await
    .unwrap();
    assert!(
        wildcard.is_empty(),
        "the prefix is literal, not a LIKE pattern"
    );

    let combined = read(EventFilter {
        project: Some("TRK".into()),
        entity_kind: Some("issue".into()),
        kind_prefix: Some("issue.arch".into()),
    })
    .await
    .unwrap();
    assert_eq!(ids(combined), [7]);
    store.close().await;
}

// ---- consumers ----

#[tokio::test]
async fn consumer_lifecycle() {
    let (_dir, _path, store) = open();
    seed(&store, 3).await; // events 1..=5

    let (cursor, created) = run(&store, |tx| {
        events::register_consumer(tx, 10, "runner", Start::Beginning)
    })
    .await;
    assert!(created);
    assert_eq!((cursor.last_event_id, cursor.updated), (0, 10));
    let (again, created) = run(&store, |tx| {
        events::register_consumer(tx, 11, "runner", Start::Latest)
    })
    .await;
    assert!(!created, "registering twice is idempotent");
    assert_eq!(again, cursor, "an existing consumer keeps its cursor");

    assert_eq!(poll_ids(&store, "runner").await, [1, 2, 3, 4, 5]);
    assert_eq!(
        poll_ids(&store, "runner").await,
        [1, 2, 3, 4, 5],
        "polling does not advance the cursor"
    );
    let limited = store
        .read(|c| events::poll(c, "runner", Some(2), &EventFilter::default()))
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);

    let moved = run(&store, |tx| events::ack(tx, 20, "runner", 3)).await;
    assert_eq!((moved.last_event_id, moved.updated), (3, 20));
    assert_eq!(poll_ids(&store, "runner").await, [4, 5]);

    let older = run(&store, |tx| events::ack(tx, 30, "runner", 2)).await;
    assert_eq!(
        (older.last_event_id, older.updated),
        (3, 20),
        "an older ack is a no-op"
    );
    let same = run(&store, |tx| events::ack(tx, 31, "runner", 3)).await;
    assert_eq!(same.updated, 20);

    let beyond = store
        .write(|tx| events::ack(tx, 40, "runner", 6))
        .await
        .unwrap_err();
    assert!(is_invalid(&beyond), "{beyond}");
    let negative = store
        .write(|tx| events::ack(tx, 40, "runner", -1))
        .await
        .unwrap_err();
    assert!(is_invalid(&negative));
    let cursor = store
        .read(|c| events::get_cursor(c, "runner"))
        .await
        .unwrap();
    assert_eq!(cursor.last_event_id, 3, "refused acks changed nothing");

    run(&store, |tx| events::ack(tx, 50, "runner", 5)).await;
    assert!(poll_ids(&store, "runner").await.is_empty());

    // A consumer that starts at Latest sees only what comes after.
    let (live, _) = run(&store, |tx| {
        events::register_consumer(tx, 60, "live.page-1", Start::Latest)
    })
    .await;
    assert_eq!(live.last_event_id, 5);
    run(&store, |tx| {
        issues::create_issue(
            tx,
            2000,
            "TRK",
            NewIssue {
                title: "late".into(),
                ..NewIssue::default()
            },
        )
    })
    .await;
    assert_eq!(poll_ids(&store, "live.page-1").await, [6]);

    let names: Vec<String> = store
        .read(events::list_consumers)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["live.page-1", "runner"]);

    run(&store, |tx| events::delete_consumer(tx, "runner")).await;
    let gone = store
        .read(|c| events::get_cursor(c, "runner"))
        .await
        .unwrap_err();
    assert!(is_not_found(&gone));
    store.close().await;
}

#[tokio::test]
async fn unknown_consumer_is_not_found_and_names_are_validated() {
    let (_dir, _path, store) = open();
    let poll = store
        .read(|c| events::poll(c, "ghost", None, &EventFilter::default()))
        .await
        .unwrap_err();
    assert!(is_not_found(&poll));
    let ack = store
        .write(|tx| events::ack(tx, 1, "ghost", 0))
        .await
        .unwrap_err();
    assert!(is_not_found(&ack));
    let delete = store
        .write(|tx| events::delete_consumer(tx, "ghost"))
        .await
        .unwrap_err();
    assert!(is_not_found(&delete));

    for bad in ["", "has space", "slash/name", "ünï", &"x".repeat(101)] {
        let bad = bad.to_string();
        let err = store
            .write(move |tx| events::register_consumer(tx, 1, &bad, Start::Beginning))
            .await
            .unwrap_err();
        assert!(is_invalid(&err), "{err}");
    }
    let longest = "x".repeat(100);
    run(&store, move |tx| {
        events::register_consumer(tx, 1, &longest, Start::Beginning)
    })
    .await;
    store.close().await;
}

#[tokio::test]
async fn consumers_keep_independent_cursors_and_filters_apply_to_poll() {
    let (_dir, _path, store) = open();
    seed(&store, 3).await;
    for name in ["webhook", "runner"] {
        run(&store, move |tx| {
            events::register_consumer(tx, 1, name, Start::Beginning)
        })
        .await;
    }
    run(&store, |tx| events::ack(tx, 2, "webhook", 4)).await;
    assert_eq!(poll_ids(&store, "webhook").await, [5]);
    assert_eq!(poll_ids(&store, "runner").await, [1, 2, 3, 4, 5]);

    let only_types = store
        .read(|c| {
            events::poll(
                c,
                "runner",
                None,
                &EventFilter {
                    kind_prefix: Some("issue_type.".into()),
                    ..EventFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(only_types.len(), 1);
    store.close().await;
}

#[tokio::test]
async fn cursor_survives_a_restart_without_gaps_or_repeats() {
    let (_dir, path, store) = open();
    seed(&store, 4).await; // events 1..=6
    run(&store, |tx| {
        events::register_consumer(tx, 1, "runner", Start::Beginning)
    })
    .await;
    run(&store, |tx| events::ack(tx, 2, "runner", 3)).await;
    // Seen but never acknowledged: must be delivered again.
    assert_eq!(poll_ids(&store, "runner").await, [4, 5, 6]);
    store.close().await;

    let store = Store::open(&path).unwrap();
    let cursor = store
        .read(|c| events::get_cursor(c, "runner"))
        .await
        .unwrap();
    assert_eq!(cursor.last_event_id, 3);
    assert_eq!(poll_ids(&store, "runner").await, [4, 5, 6]);

    run(&store, |tx| {
        issues::create_issue(
            tx,
            3000,
            "TRK",
            NewIssue {
                title: "after restart".into(),
                ..NewIssue::default()
            },
        )
    })
    .await;
    assert_eq!(poll_ids(&store, "runner").await, [4, 5, 6, 7]);
    run(&store, |tx| events::ack(tx, 3, "runner", 7)).await;
    store.close().await;

    let store = Store::open(&path).unwrap();
    assert!(poll_ids(&store, "runner").await.is_empty());
    let ids: Vec<i64> = all_events(&store).await.iter().map(|e| e.id).collect();
    assert_eq!(ids, (1..=7).collect::<Vec<_>>());
    store.close().await;
}

// ---- immutability ----

#[tokio::test]
async fn events_cannot_be_edited_or_deleted() {
    let (_dir, _path, store) = open();
    seed(&store, 1).await;
    let before = all_events(&store).await;

    let outcome = run(&store, |tx| {
        let update = tx.execute("UPDATE event SET kind = 'forged' WHERE id = 1", []);
        let payload = tx.execute("UPDATE event SET payload_json = '{}'", []);
        let delete = tx.execute("DELETE FROM event WHERE id = 1", []);
        let delete_all = tx.execute("DELETE FROM event", []);
        let replace = tx.execute(
            "INSERT OR REPLACE INTO event (id, time, entity_kind, entity_id, kind, payload_json)
             VALUES (1, 0, 'x', 0, 'forged', '{}')",
            [],
        );
        Ok([update, payload, delete, delete_all, replace].map(|r| r.is_err()))
    })
    .await;
    assert_eq!(outcome, [true; 5]);
    assert_eq!(all_events(&store).await, before);

    // Ids are never reused, even for the newest event.
    let ddl: String = store
        .read(|c| {
            Ok(c.query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'event'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(ddl.contains("AUTOINCREMENT"));
    store.close().await;
}

#[test]
fn no_store_function_edits_an_event() {
    for (file, source) in [
        ("projects.rs", include_str!("projects.rs")),
        ("issues.rs", include_str!("issues.rs")),
        ("comments.rs", include_str!("comments.rs")),
        ("events.rs", include_str!("events.rs")),
    ] {
        let source = source.to_lowercase();
        for forbidden in [
            "update event",
            "delete from event",
            "replace into event",
            "drop table event",
        ] {
            assert!(!source.contains(forbidden), "{file} contains {forbidden:?}");
        }
        assert!(
            source.matches("insert into event").count() <= 1,
            "{file}: events are appended in one place (projects::emit)"
        );
    }
}

// ---- replay ----

#[derive(Debug, PartialEq, Default)]
struct Snapshot {
    /// project id -> (key, name)
    projects: BTreeMap<i64, (String, String)>,
    /// project id -> column names, sorted
    columns: BTreeMap<i64, Vec<String>>,
    /// id -> (project id, name)
    types: BTreeMap<i64, (i64, String)>,
    /// id -> (project id, name, color)
    labels: BTreeMap<i64, (i64, String, String)>,
    /// issue id -> issue
    issues: BTreeMap<i64, IssueSnap>,
    /// initiative id -> initiative
    initiatives: BTreeMap<i64, InitiativeSnap>,
    /// link id -> (kind, project id, from number, to number); a `relates` link
    /// has the lower number first
    links: BTreeMap<i64, (String, i64, i64, i64)>,
    /// comment id -> comment, for the comments a reader sees
    comments: BTreeMap<i64, CommentSnap>,
}

#[derive(Debug, PartialEq)]
struct CommentSnap {
    issue_id: i64,
    parent: Option<i64>,
    author: i64,
    mentions: Vec<String>,
    edited: bool,
    deleted: bool,
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(text).collect()
}

/// What readers see: a deleted reply is gone, a deleted top-level comment only
/// stays while a live reply hangs off it, and a deleted comment has no mentions.
fn visible(comments: &mut BTreeMap<i64, CommentSnap>) {
    let with_replies: std::collections::BTreeSet<i64> = comments
        .values()
        .filter(|c| !c.deleted)
        .filter_map(|c| c.parent)
        .collect();
    let gone: Vec<i64> = comments
        .iter()
        .filter(|(id, c)| c.deleted && (c.parent.is_some() || !with_replies.contains(id)))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        comments.remove(&id);
    }
    for c in comments.values_mut().filter(|c| c.deleted) {
        c.mentions.clear();
    }
}

fn link_snap(kind: &str, pid: i64, from: i64, to: i64) -> (String, i64, i64, i64) {
    if kind == "relates" {
        (kind.to_string(), pid, from.min(to), from.max(to))
    } else {
        (kind.to_string(), pid, from, to)
    }
}

/// The description is only in the log as a change, never at creation, so it
/// is not modelled.
#[derive(Debug, PartialEq)]
struct InitiativeSnap {
    project_id: i64,
    name: String,
    status: String,
    target_date: Option<String>,
}

#[derive(Debug, PartialEq)]
struct IssueSnap {
    project_id: i64,
    number: i64,
    title: String,
    type_id: i64,
    column_id: i64,
    assignee_id: Option<i64>,
    parent: Option<i64>,
    initiative: Option<i64>,
    priority: Option<String>,
    labels: Vec<i64>,
    archived: bool,
}

fn text(value: &Value) -> String {
    value.as_str().unwrap().to_string()
}

fn ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

/// Rebuilds state from the events alone. Panics on a kind it does not know, so
/// a new event kind cannot be added without teaching the replay about it.
fn replay(log: &[Event]) -> Snapshot {
    let mut s = Snapshot::default();
    for e in log {
        let pid = e.project_id.unwrap();
        let p = &e.payload;
        match e.kind.as_str() {
            "project.created" => {
                s.projects.insert(pid, (text(&p["key"]), text(&p["name"])));
                let names = p["columns"].as_array().unwrap().iter().map(text).collect();
                s.columns.insert(pid, names);
            }
            "project.updated" => s.projects.get_mut(&pid).unwrap().1 = text(&p["name"]),
            "column.created" => s.columns.get_mut(&pid).unwrap().push(text(&p["name"])),
            "column.renamed" => {
                let names = s.columns.get_mut(&pid).unwrap();
                let at = names.iter().position(|n| *n == text(&p["previous_name"]));
                names[at.unwrap()] = text(&p["name"]);
            }
            "column.deleted" => {
                let names = s.columns.get_mut(&pid).unwrap();
                let at = names.iter().position(|n| *n == text(&p["name"]));
                names.remove(at.unwrap());
            }
            "column.reordered" => {}
            "issue_type.created" => {
                s.types.insert(e.entity_id, (pid, text(&p["name"])));
            }
            "issue_type.renamed" => s.types.get_mut(&e.entity_id).unwrap().1 = text(&p["name"]),
            "issue_type.deleted" => {
                s.types.remove(&e.entity_id);
            }
            "label.created" => {
                s.labels
                    .insert(e.entity_id, (pid, text(&p["name"]), text(&p["color"])));
            }
            "label.renamed" => s.labels.get_mut(&e.entity_id).unwrap().1 = text(&p["name"]),
            "label.recolored" => s.labels.get_mut(&e.entity_id).unwrap().2 = text(&p["color"]),
            "label.deleted" => {
                s.labels.remove(&e.entity_id);
                for issue in s.issues.values_mut().filter(|i| i.project_id == pid) {
                    issue.labels.retain(|l| *l != e.entity_id);
                }
            }
            "issue.created" => {
                s.issues.insert(
                    e.entity_id,
                    IssueSnap {
                        project_id: pid,
                        number: p["number"].as_i64().unwrap(),
                        title: text(&p["title"]),
                        type_id: p["type_id"].as_i64().unwrap(),
                        column_id: p["column_id"].as_i64().unwrap(),
                        assignee_id: p["assignee_id"].as_i64(),
                        parent: p["parent"].as_i64(),
                        initiative: p["initiative"].as_i64(),
                        priority: p["priority"].as_str().map(String::from),
                        labels: ids(&p["labels"]),
                        archived: false,
                    },
                );
            }
            "initiative.created" => {
                s.initiatives.insert(
                    e.entity_id,
                    InitiativeSnap {
                        project_id: pid,
                        name: text(&p["name"]),
                        status: text(&p["status"]),
                        target_date: p["target_date"].as_str().map(String::from),
                    },
                );
            }
            "initiative.updated" => {
                let initiative = s.initiatives.get_mut(&e.entity_id).unwrap();
                for (field, change) in p["changes"].as_object().unwrap() {
                    let to = &change["to"];
                    match field.as_str() {
                        "name" => initiative.name = text(to),
                        "status" => initiative.status = text(to),
                        "target_date" => initiative.target_date = to.as_str().map(String::from),
                        "description" => {}
                        other => panic!("unknown initiative field {other}"),
                    }
                }
            }
            "initiative.deleted" => {
                // The deleted initiative's issues are unassigned by their own
                // issue.updated events, which come first.
                assert!(
                    s.issues.values().all(|i| i.initiative != Some(e.entity_id)),
                    "issues still point at the deleted initiative"
                );
                s.initiatives.remove(&e.entity_id);
            }
            "issue.updated" => {
                let issue = s.issues.get_mut(&e.entity_id).unwrap();
                for (field, change) in p["changes"].as_object().unwrap() {
                    let to = &change["to"];
                    match field.as_str() {
                        "title" => issue.title = text(to),
                        "type_id" => issue.type_id = to.as_i64().unwrap(),
                        "assignee_id" => issue.assignee_id = to.as_i64(),
                        "parent" => issue.parent = to.as_i64(),
                        "initiative" => issue.initiative = to.as_i64(),
                        "priority" => issue.priority = to.as_str().map(String::from),
                        "labels" => issue.labels = ids(to),
                        // The description is only in the log as a change, never at creation.
                        "description" => {}
                        other => panic!("unknown issue field {other}"),
                    }
                }
            }
            "issue.moved" => {
                s.issues.get_mut(&e.entity_id).unwrap().column_id =
                    p["to"]["column_id"].as_i64().unwrap();
            }
            "issue.archived" => s.issues.get_mut(&e.entity_id).unwrap().archived = true,
            "issue.unarchived" => s.issues.get_mut(&e.entity_id).unwrap().archived = false,
            "link.added" => {
                let snap = link_snap(
                    &text(&p["kind"]),
                    pid,
                    p["from"]["number"].as_i64().unwrap(),
                    p["to"]["number"].as_i64().unwrap(),
                );
                s.links.insert(p["link_id"].as_i64().unwrap(), snap);
            }
            "link.removed" => {
                s.links.remove(&p["link_id"].as_i64().unwrap());
            }
            "comment.created" => {
                s.comments.insert(
                    p["comment_id"].as_i64().unwrap(),
                    CommentSnap {
                        issue_id: e.entity_id,
                        parent: p["parent"].as_i64(),
                        author: p["author"]["id"].as_i64().unwrap(),
                        mentions: strings(&p["mentions"]),
                        edited: false,
                        deleted: false,
                    },
                );
            }
            "comment.edited" => {
                let c = s
                    .comments
                    .get_mut(&p["comment_id"].as_i64().unwrap())
                    .unwrap();
                c.mentions = strings(&p["mentions"]);
                c.edited = true;
            }
            "comment.deleted" => {
                s.comments
                    .get_mut(&p["comment_id"].as_i64().unwrap())
                    .unwrap()
                    .deleted = true;
            }
            "mention.created" => {
                // No state of its own: it names a user the comment mentions.
                let c = &s.comments[&p["comment_id"].as_i64().unwrap()];
                assert!(c.mentions.contains(&text(&p["user_name"])));
                assert_eq!(c.issue_id, e.entity_id);
            }
            other => panic!("replay does not know the event kind {other}"),
        }
    }
    visible(&mut s.comments);
    for names in s.columns.values_mut() {
        names.sort();
    }
    s
}

async fn read_state(store: &Store) -> Snapshot {
    store
        .read(|c| {
            let mut s = Snapshot::default();
            for project in projects::list_projects(c)? {
                s.projects
                    .insert(project.id, (project.key.clone(), project.name.clone()));
                let mut names: Vec<String> = projects::list_columns(c, &project.key)?
                    .into_iter()
                    .map(|col| col.name)
                    .collect();
                names.sort();
                s.columns.insert(project.id, names);
                for t in projects::list_issue_types(c, &project.key)? {
                    s.types.insert(t.id, (t.project_id, t.name));
                }
                for l in projects::list_labels(c, &project.key)? {
                    s.labels.insert(l.id, (l.project_id, l.name, l.color));
                }
                for view in initiatives::list_initiatives(c, 0, &project.key, None)? {
                    let i = view.initiative;
                    s.initiatives.insert(
                        i.id,
                        InitiativeSnap {
                            project_id: i.project_id,
                            name: i.name,
                            status: i.status.as_str().to_string(),
                            target_date: i.target_date,
                        },
                    );
                }
                let list = issues::list_issues(
                    c,
                    &project.key,
                    &issues::IssueFilter {
                        include_archived: true,
                        limit: Some(issues::MAX_LIMIT),
                        ..issues::IssueFilter::default()
                    },
                )?;
                for i in &list.issues {
                    for top in comments::list_comments(c, &project.key, i.number)? {
                        for comment in std::iter::once(&top).chain(top.replies.iter()) {
                            s.comments.insert(
                                comment.id,
                                CommentSnap {
                                    issue_id: i.id,
                                    parent: comment.parent,
                                    author: comment.author.id,
                                    mentions: comment
                                        .mentions
                                        .iter()
                                        .map(|m| m.name.clone())
                                        .collect(),
                                    edited: comment.edited.is_some(),
                                    deleted: comment.deleted.is_some(),
                                },
                            );
                        }
                    }
                    let found = links::list_links(c, &project.key, i.number)?;
                    for other in &found.blocks {
                        s.links.insert(
                            other.link_id,
                            link_snap("blocks", project.id, i.number, other.number),
                        );
                    }
                    for other in &found.related {
                        s.links.insert(
                            other.link_id,
                            link_snap("relates", project.id, i.number, other.number),
                        );
                    }
                }
                for i in list.issues {
                    s.issues.insert(
                        i.id,
                        IssueSnap {
                            project_id: i.project_id,
                            number: i.number,
                            title: i.title,
                            type_id: i.type_id,
                            column_id: i.column_id,
                            assignee_id: i.assignee_id,
                            parent: i.parent,
                            initiative: i.initiative_id,
                            priority: i.priority.map(|p| p.as_str().to_string()),
                            labels: i.labels,
                            archived: i.archived,
                        },
                    );
                }
            }
            Ok(s)
        })
        .await
        .unwrap()
}

fn patch(f: impl FnOnce(&mut IssuePatch)) -> IssuePatch {
    let mut patch = IssuePatch::default();
    f(&mut patch);
    patch
}

#[tokio::test]
async fn replaying_the_log_rebuilds_the_state() {
    let (_dir, _path, store) = open();
    let s = &store;

    // Projects and columns.
    let trk = run(s, |tx| {
        projects::create_project(tx, 1, "TRK", "Tracker", None)
    })
    .await;
    let col: Vec<i64> = trk.columns.iter().map(|c| c.id).collect();
    let oth = run(s, |tx| {
        let columns = [
            NewColumn {
                name: "Open".into(),
                state_type: StateType::Unstarted,
            },
            NewColumn {
                name: "Closed".into(),
                state_type: StateType::Completed,
            },
        ];
        projects::create_project(tx, 2, "OTH", "Other", Some(&columns))
    })
    .await;
    let review = run(s, |tx| {
        projects::create_column(tx, 3, "TRK", "Review", StateType::Started)
    })
    .await;
    run(s, move |tx| {
        projects::rename_column(tx, 4, "TRK", review.id, "QA")
    })
    .await;
    let tmp = run(s, |tx| {
        projects::create_column(tx, 5, "TRK", "Tmp", StateType::Backlog)
    })
    .await;
    run(s, move |tx| projects::delete_column(tx, 6, "TRK", tmp.id)).await;
    let mut order: Vec<i64> = col.clone();
    order.push(review.id);
    order.reverse();
    run(s, move |tx| projects::reorder_columns(tx, 7, "TRK", &order)).await;

    // Issue types and labels.
    let task = run(s, |tx| projects::create_issue_type(tx, 8, "TRK", "Task")).await;
    let bug = run(s, |tx| projects::create_issue_type(tx, 8, "TRK", "Bug")).await;
    run(s, move |tx| {
        projects::rename_issue_type(tx, 9, "TRK", bug.id, "Defect")
    })
    .await;
    let temp = run(s, |tx| projects::create_issue_type(tx, 9, "TRK", "Temp")).await;
    run(s, move |tx| {
        projects::delete_issue_type(tx, 9, "TRK", temp.id)
    })
    .await;
    let item = run(s, |tx| projects::create_issue_type(tx, 9, "OTH", "Item")).await;
    let a = run(s, |tx| {
        projects::create_label(tx, 10, "TRK", "a", "#AABBCC")
    })
    .await;
    let b = run(s, |tx| {
        projects::create_label(tx, 10, "TRK", "b", "#000000")
    })
    .await;
    let c = run(s, |tx| {
        projects::create_label(tx, 10, "TRK", "c", "#ffffff")
    })
    .await;
    run(s, move |tx| {
        projects::rename_label(tx, 11, "TRK", a.id, "alpha")
    })
    .await;
    run(s, move |tx| {
        projects::recolor_label(tx, 11, "TRK", b.id, "#112233")
    })
    .await;
    run(s, |tx| {
        projects::create_label(tx, 11, "OTH", "x", "#123456")
    })
    .await;
    run(s, |tx| {
        tx.execute(
            "INSERT INTO user (id, name, kind) VALUES (1, 'Ann', 'human')",
            [],
        )?;
        Ok(())
    })
    .await;

    // Issues.
    let (task_id, defect_id, item_id) = (task.id, bug.id, item.id);
    let (a_id, b_id, c_id) = (a.id, b.id, c.id);
    run(s, move |tx| {
        issues::create_issue(
            tx,
            20,
            "TRK",
            NewIssue {
                title: "one".into(),
                type_id: Some(task_id),
                priority: Some(Priority::High),
                labels: vec![a_id, b_id],
                ..NewIssue::default()
            },
        )
    })
    .await;
    let todo = col[1];
    run(s, move |tx| {
        issues::create_issue(
            tx,
            21,
            "TRK",
            NewIssue {
                title: "two".into(),
                type_id: Some(task_id),
                column_id: Some(todo),
                parent: Some(1),
                ..NewIssue::default()
            },
        )
    })
    .await;
    run(s, move |tx| {
        issues::create_issue(
            tx,
            22,
            "TRK",
            NewIssue {
                title: "three".into(),
                type_id: Some(task_id),
                labels: vec![c_id],
                ..NewIssue::default()
            },
        )
    })
    .await;
    let oth_columns: Vec<i64> = oth.columns.iter().map(|c| c.id).collect();
    run(s, move |tx| {
        issues::create_issue(
            tx,
            23,
            "OTH",
            NewIssue {
                title: "other".into(),
                type_id: Some(item_id),
                column_id: Some(oth_columns[1]),
                ..NewIssue::default()
            },
        )
    })
    .await;

    run(s, |tx| {
        issues::update_issue(tx, 30, "TRK", 1, patch(|p| p.title = Some("uno".into())))
    })
    .await;
    run(s, |tx| {
        issues::update_issue(
            tx,
            31,
            "TRK",
            1,
            patch(|p| p.description = Some(Some("details".into()))),
        )
    })
    .await;
    run(s, |tx| {
        issues::update_issue(tx, 32, "TRK", 2, patch(|p| p.assignee_id = Some(Some(1))))
    })
    .await;
    run(s, |tx| {
        issues::update_issue(tx, 33, "TRK", 2, patch(|p| p.assignee_id = Some(None)))
    })
    .await;
    run(s, |tx| {
        issues::update_issue(tx, 34, "TRK", 3, patch(|p| p.parent = Some(Some(1))))
    })
    .await;
    run(s, |tx| {
        issues::update_issue(tx, 35, "TRK", 2, patch(|p| p.parent = Some(None)))
    })
    .await;
    run(s, move |tx| {
        issues::update_issue(
            tx,
            36,
            "TRK",
            1,
            patch(|p| {
                p.labels = Some(vec![b_id]);
                p.priority = Some(None);
                p.type_id = Some(defect_id);
            }),
        )
    })
    .await;
    let (done, qa) = (col[3], review.id);
    run(s, move |tx| {
        issues::move_issue(
            tx,
            40,
            "TRK",
            1,
            MoveTarget {
                column_id: done,
                ..MoveTarget::default()
            },
        )
    })
    .await;
    run(s, move |tx| {
        issues::move_issue(
            tx,
            41,
            "TRK",
            3,
            MoveTarget {
                column_id: todo,
                after: Some(2),
                ..MoveTarget::default()
            },
        )
    })
    .await;
    run(s, move |tx| {
        issues::move_issue(
            tx,
            42,
            "TRK",
            2,
            MoveTarget {
                column_id: qa,
                ..MoveTarget::default()
            },
        )
    })
    .await;
    run(s, |tx| issues::archive_issue(tx, 50, "TRK", 2)).await;
    run(s, |tx| issues::archive_issue(tx, 51, "TRK", 3)).await;
    run(s, |tx| issues::unarchive_issue(tx, 52, "TRK", 3)).await;
    run(s, move |tx| projects::delete_label(tx, 53, "TRK", c_id)).await;
    run(s, |tx| projects::rename_project(tx, 54, "TRK", "Tracker 2")).await;

    // Initiatives: created, edited, issues joining, moving between them and
    // leaving, one deleted while holding issues (one created into it).
    let launch = run(s, |tx| {
        initiatives::create_initiative(
            tx,
            55,
            "TRK",
            NewInitiative {
                name: "Launch".into(),
                target_date: Some("2030-01-01".into()),
                ..NewInitiative::default()
            },
        )
    })
    .await;
    let doomed = run(s, |tx| {
        initiatives::create_initiative(
            tx,
            55,
            "TRK",
            NewInitiative {
                name: "Doomed".into(),
                status: Some(InitiativeStatus::Active),
                ..NewInitiative::default()
            },
        )
    })
    .await;
    run(s, |tx| {
        initiatives::create_initiative(
            tx,
            55,
            "OTH",
            NewInitiative {
                name: "Theirs".into(),
                ..NewInitiative::default()
            },
        )
    })
    .await;
    let (launch_id, doomed_id) = (launch.initiative.id, doomed.initiative.id);
    run(s, move |tx| {
        initiatives::update_initiative(
            tx,
            56,
            "TRK",
            launch_id,
            InitiativePatch {
                name: Some("Launch 1.0".into()),
                description: Some(Some("the plan".into())),
                status: Some(InitiativeStatus::Active),
                target_date: Some(Some("2031-03-04".into())),
            },
        )
    })
    .await;
    run(s, move |tx| {
        initiatives::update_initiative(
            tx,
            57,
            "TRK",
            launch_id,
            InitiativePatch {
                target_date: Some(None),
                ..InitiativePatch::default()
            },
        )
    })
    .await;
    for (number, initiative) in [(1, launch_id), (2, doomed_id), (3, doomed_id)] {
        run(s, move |tx| {
            issues::update_issue(
                tx,
                58,
                "TRK",
                number,
                patch(|p| p.initiative = Some(Some(initiative))),
            )
        })
        .await;
    }
    run(s, move |tx| {
        issues::update_issue(
            tx,
            59,
            "TRK",
            3,
            patch(|p| p.initiative = Some(Some(launch_id))),
        )
    })
    .await;
    run(s, move |tx| {
        issues::create_issue(
            tx,
            59,
            "TRK",
            NewIssue {
                title: "four".into(),
                type_id: Some(task_id),
                initiative: Some(doomed_id),
                ..NewIssue::default()
            },
        )
    })
    .await;
    run(s, move |tx| {
        initiatives::delete_initiative(tx, 59, "TRK", doomed_id)
    })
    .await;
    run(s, move |tx| {
        issues::update_issue(tx, 59, "TRK", 1, patch(|p| p.initiative = Some(None)))
    })
    .await;

    // Links: added, one removed, one re-added the other way, a symmetric one.
    run(s, |tx| {
        links::add_link(tx, 60, "TRK", 1, 2, LinkKind::Blocks)
    })
    .await;
    let gone = run(s, |tx| {
        links::add_link(tx, 61, "TRK", 3, 1, LinkKind::Blocks)
    })
    .await;
    run(s, |tx| {
        links::add_link(tx, 62, "TRK", 3, 2, LinkKind::Relates)
    })
    .await;
    run(s, move |tx| links::remove_link(tx, 63, "TRK", gone.id)).await;
    run(s, |tx| {
        links::add_link(tx, 64, "TRK", 1, 3, LinkKind::Blocks)
    })
    .await;

    // Comments: threads, edits that add and drop mentions, deletes of every kind.
    run(s, |tx| {
        tx.execute(
            "INSERT INTO user (id, name, kind) VALUES (2, 'bot', 'agent')",
            [],
        )?;
        Ok(())
    })
    .await;
    let ann = Principal {
        user_id: 1,
        role: Role::Member,
        scope: Scope::Write,
    };
    let bot = Principal {
        user_id: 2,
        role: Role::Agent,
        scope: Scope::Write,
    };
    let comment = |now: i64,
                   who: Principal,
                   key: &'static str,
                   issue: i64,
                   body: &'static str,
                   parent: Option<i64>| {
        (
            now,
            who,
            key,
            issue,
            NewComment {
                body: body.into(),
                parent,
            },
        )
    };
    let mut ids = Vec::new();
    for (now, who, key, issue, new) in [
        comment(70, ann, "TRK", 1, "hi @bot", None),
        comment(71, bot, "TRK", 1, "thanks @Ann", Some(1)),
        comment(72, ann, "TRK", 2, "plain", None),
        comment(73, ann, "TRK", 1, "gone soon", Some(1)),
        comment(74, ann, "OTH", 1, "@bot @ann @bot", None),
    ] {
        // `Some(1)` stands for the first comment created above.
        let new = NewComment {
            parent: new.parent.map(|_| ids[0]),
            ..new
        };
        let added = run(s, move |tx| {
            comments::add_comment(tx, now, &who, key, issue, new)
        })
        .await;
        ids.push(added.id);
    }
    let [hi, thanks, plain, soon, other] = ids[..] else {
        panic!("five comments")
    };
    run(s, move |tx| {
        comments::edit_comment(tx, 75, &ann, "TRK", hi, "hi @bot and @Ann")
    })
    .await;
    run(s, move |tx| {
        comments::edit_comment(tx, 76, &bot, "TRK", thanks, "no mentions")
    })
    .await;
    run(s, move |tx| {
        comments::edit_comment(tx, 77, &ann, "OTH", other, "@bot only")
    })
    .await;
    run(s, move |tx| {
        comments::delete_comment(tx, 78, &ann, "TRK", soon)
    })
    .await;
    run(s, move |tx| {
        comments::delete_comment(tx, 79, &ann, "TRK", plain)
    })
    .await;
    run(s, move |tx| {
        comments::delete_comment(tx, 80, &ann, "TRK", hi)
    })
    .await;

    let log = all_events(s).await;
    assert!(log.len() > 30, "{} events", log.len());
    let replayed = replay(&log);
    let actual = read_state(s).await;
    assert_eq!(replayed, actual);
    assert_eq!(actual.links.len(), 3);

    // Visible: the placeholder of `hi` (its reply stays), the reply and the OTH
    // comment; the deleted reply and the deleted lone comment are gone.
    assert_eq!(actual.comments.len(), 3);
    assert!(actual.comments[&hi].deleted && actual.comments[&hi].mentions.is_empty());
    assert_eq!(actual.comments[&thanks].parent, Some(hi));
    assert!(actual.comments[&thanks].edited && actual.comments[&thanks].mentions.is_empty());
    assert_eq!(actual.comments[&other].mentions, ["bot"]);
    assert!(!actual.comments.contains_key(&soon) && !actual.comments.contains_key(&plain));
    assert!(log.iter().any(|e| e.kind == "mention.created"));

    // The scenario really exercised the state, not an empty model.
    assert_eq!(actual.issues.len(), 5);
    assert_eq!(actual.projects.len(), 2);
    assert_eq!(actual.initiatives.len(), 2, "Launch and Theirs are left");
    assert_eq!(actual.initiatives[&launch_id].name, "Launch 1.0");
    assert_eq!(actual.initiatives[&launch_id].target_date, None);
    let joined: Vec<_> = actual
        .issues
        .values()
        .filter(|i| i.initiative.is_some())
        .collect();
    assert_eq!(joined.len(), 1, "only issue 3 stayed in an initiative");
    assert_eq!(joined[0].number, 3);
    assert!(log.iter().any(|e| e.kind == "initiative.deleted"));
    assert!(actual.issues.values().any(|i| i.archived));
    assert!(actual.issues.values().any(|i| i.parent.is_some()));
    assert!(actual.issues.values().any(|i| i.labels == [b_id]));

    assert_eq!(replay(&log), replayed, "replay is deterministic");
    store.close().await;
}
