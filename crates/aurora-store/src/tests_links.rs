//! Links between issues: kinds, refusals, cycles, blocked state and events.

use rusqlite::Transaction;

use crate::auth::{Principal, Role, Scope};
use crate::events::{self, EventFilter};
use crate::projects::{Rejection, StateType};
use crate::tests_support::issues::{
    self, IssueDetail, IssueFilter, MoveTarget, NewIssue, archive_issue, create_issue, get_issue,
    list_issues, move_issue,
};
use crate::tests_support::links::{self, LinkKind};
use crate::tests_support::projects::{create_issue_type, create_project};
use crate::{Store, StoreError};

struct Setup {
    _dir: tempfile::TempDir,
    store: Store,
    /// backlog, todo, in progress, done, canceled
    columns: Vec<i64>,
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

/// Project TRK with `count` issues (numbers 1..=count) in Backlog; project OTH with 5 issues.
async fn setup(count: i64) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let columns = store
        .write(move |tx| {
            let trk = create_project(tx, 1, "TRK", "Tracker", None)?;
            create_issue_type(tx, 1, "TRK", "Task")?;
            create_project(tx, 1, "OTH", "Other", None)?;
            create_issue_type(tx, 1, "OTH", "Task")?;
            for n in 1..=count {
                create_issue(tx, 10 + n, "TRK", titled(&format!("issue {n}")))?;
            }
            for n in 1..=5 {
                create_issue(tx, 10 + n, "OTH", titled(&format!("other {n}")))?;
            }
            Ok(trk.columns.iter().map(|c| c.id).collect::<Vec<_>>())
        })
        .await
        .unwrap();
    Setup {
        _dir: dir,
        store,
        columns,
    }
}

fn titled(title: &str) -> NewIssue {
    NewIssue {
        title: title.into(),
        ..NewIssue::default()
    }
}

impl Setup {
    async fn add(&self, from: i64, to: i64, kind: LinkKind) -> Result<links::Link, StoreError> {
        self.store
            .write(move |tx| links::add_link(tx, 100, "TRK", from, to, kind))
            .await
    }

    async fn remove(&self, id: i64) -> Result<(), StoreError> {
        self.store
            .write(move |tx| links::remove_link(tx, 200, "TRK", id))
            .await
    }

    async fn get(&self, number: i64) -> IssueDetail {
        self.store
            .read(move |c| get_issue(c, "TRK", number))
            .await
            .unwrap()
    }

    async fn links(&self, number: i64) -> links::IssueLinks {
        self.store
            .read(move |c| links::list_links(c, "TRK", number))
            .await
            .unwrap()
    }

    async fn mv(&self, number: i64, column: i64) {
        self.store
            .write(move |tx| {
                move_issue(
                    tx,
                    300,
                    "TRK",
                    number,
                    MoveTarget {
                        column_id: column,
                        ..MoveTarget::default()
                    },
                )
            })
            .await
            .unwrap();
    }

    async fn blocked(&self, number: i64) -> (bool, i64) {
        let issue = self.get(number).await.issue;
        (issue.blocked, issue.blocker_count)
    }
}

fn numbers(items: &[links::LinkedIssue]) -> Vec<i64> {
    items.iter().map(|i| i.number).collect()
}

#[tokio::test]
async fn blocks_and_relates_links_show_from_both_ends() {
    let s = setup(4).await;
    let blocks = s.add(1, 2, LinkKind::Blocks).await.unwrap();
    assert_eq!(blocks.kind, LinkKind::Blocks);
    assert_eq!((blocks.from.number, blocks.to.number), (1, 2));
    assert_eq!(blocks.from.identifier, "TRK-1");
    assert_eq!(blocks.created, 100);
    s.add(3, 1, LinkKind::Blocks).await.unwrap();
    // Given as 4 -> 3, stored as 3 -> 4: the lower issue id comes first.
    let relates = s.add(4, 3, LinkKind::Relates).await.unwrap();
    assert_eq!((relates.from.number, relates.to.number), (3, 4));

    let one = s.links(1).await;
    assert_eq!(numbers(&one.blocks), [2]);
    assert_eq!(numbers(&one.blocked_by), [3]);
    assert!(one.related.is_empty());
    let two = s.links(2).await;
    assert_eq!(numbers(&two.blocked_by), [1]);
    assert!(two.blocks.is_empty());
    let three = s.links(3).await;
    assert_eq!(numbers(&three.related), [4]);
    assert_eq!(numbers(&s.links(4).await.related), [3]);

    let item = &one.blocks[0];
    assert_eq!(item.link_id, blocks.id);
    assert_eq!(item.identifier, "TRK-2");
    assert_eq!(item.title, "issue 2");
    assert_eq!(item.column_id, s.columns[0]);
    assert_eq!(item.state_type, StateType::Backlog);
    assert!(!item.archived);

    // The detail carries the same links.
    assert_eq!(s.get(1).await.links, one);
    s.store.close().await;
}

#[tokio::test]
async fn bad_links_are_refused() {
    let s = setup(4).await;
    let err = |r: Result<links::Link, StoreError>| rejection(r.unwrap_err());

    assert!(matches!(
        err(s.add(1, 1, LinkKind::Blocks).await),
        Rejection::Invalid(_)
    ));
    assert!(matches!(
        err(s.add(2, 2, LinkKind::Relates).await),
        Rejection::Invalid(_)
    ));
    // 5 exists in OTH only: links stay inside one project.
    assert!(matches!(
        err(s.add(1, 5, LinkKind::Blocks).await),
        Rejection::Invalid(_)
    ));
    assert!(matches!(
        err(s.add(99, 1, LinkKind::Blocks).await),
        Rejection::NotFound(_)
    ));

    s.add(1, 2, LinkKind::Blocks).await.unwrap();
    assert!(matches!(
        err(s.add(1, 2, LinkKind::Blocks).await),
        Rejection::Conflict(_)
    ));
    // The opposite direction of a blocks link closes a cycle.
    assert!(matches!(
        err(s.add(2, 1, LinkKind::Blocks).await),
        Rejection::Conflict(_)
    ));

    // A relates link is the same link in either direction.
    s.add(3, 4, LinkKind::Relates).await.unwrap();
    assert!(matches!(
        err(s.add(3, 4, LinkKind::Relates).await),
        Rejection::Conflict(_)
    ));
    assert!(matches!(
        err(s.add(4, 3, LinkKind::Relates).await),
        Rejection::Conflict(_)
    ));
    // A pair can be both related and blocking.
    s.add(3, 4, LinkKind::Blocks).await.unwrap();

    let count: i64 = s
        .store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM issue_link", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 3, "refused links leave nothing behind");
    s.store.close().await;
}

#[tokio::test]
async fn cycles_in_the_blocks_graph_are_refused() {
    let s = setup(5).await;
    s.add(1, 2, LinkKind::Blocks).await.unwrap();
    s.add(2, 3, LinkKind::Blocks).await.unwrap();
    let cycle = rejection(s.add(3, 1, LinkKind::Blocks).await.unwrap_err());
    assert!(
        matches!(&cycle, Rejection::Conflict(reason) if reason.contains("cycle")),
        "{cycle:?}"
    );
    // A longer chain closes too, and a diamond does not count as a cycle.
    s.add(3, 4, LinkKind::Blocks).await.unwrap();
    assert!(matches!(
        rejection(s.add(4, 1, LinkKind::Blocks).await.unwrap_err()),
        Rejection::Conflict(_)
    ));
    s.add(1, 3, LinkKind::Blocks).await.unwrap();
    s.add(1, 4, LinkKind::Blocks).await.unwrap();
    // Related links do not take part in cycles.
    s.add(4, 1, LinkKind::Relates).await.unwrap();
    let blocked_back = s.links(3).await.blocks;
    assert_eq!(numbers(&blocked_back), [4]);
    s.store.close().await;
}

#[tokio::test]
async fn a_long_chain_is_still_checked_for_cycles() {
    let s = setup(1).await;
    // 300 more issues in a chain 1 <- 2 <- ... : each blocks the next.
    s.store
        .write(|tx| {
            for n in 2..=300 {
                create_issue(tx, 500, "TRK", titled(&format!("n{n}")))?;
            }
            for n in 1..300 {
                links::add_link(tx, 500, "TRK", n, n + 1, LinkKind::Blocks)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let err = rejection(s.add(300, 1, LinkKind::Blocks).await.unwrap_err());
    assert!(matches!(err, Rejection::Conflict(_)));
    s.store.close().await;
}

#[tokio::test]
async fn removing_a_link_deletes_it_and_writes_an_event() {
    let s = setup(3).await;
    let link = s.add(1, 2, LinkKind::Blocks).await.unwrap();
    let other = s.add(2, 3, LinkKind::Blocks).await.unwrap();
    s.remove(link.id).await.unwrap();
    assert!(s.links(1).await.blocks.is_empty());
    assert!(s.links(2).await.blocked_by.is_empty());
    assert_eq!(numbers(&s.links(2).await.blocks), [3]);

    assert!(matches!(
        rejection(s.remove(link.id).await.unwrap_err()),
        Rejection::NotFound(_)
    ));
    // A link of another project cannot be removed through this one.
    let theirs = s
        .store
        .write(|tx| links::add_link(tx, 100, "OTH", 1, 2, LinkKind::Blocks))
        .await
        .unwrap();
    assert!(matches!(
        rejection(s.remove(theirs.id).await.unwrap_err()),
        Rejection::NotFound(_)
    ));
    // Removed means it can be added again, also in the other direction.
    s.add(2, 1, LinkKind::Blocks).await.unwrap();

    let log = s
        .store
        .read(|c| {
            events::read_events(
                c,
                0,
                Some(500),
                &EventFilter {
                    kind_prefix: Some("link.".into()),
                    ..EventFilter::default()
                },
            )
        })
        .await
        .unwrap();
    let kinds: Vec<&str> = log.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "link.added",
            "link.added",
            "link.removed",
            "link.added",
            "link.added"
        ]
    );
    let removed = &log[2];
    assert_eq!(removed.entity_kind, "issue");
    assert_eq!(removed.project.as_deref(), Some("TRK"));
    assert_eq!(removed.payload["link_id"], link.id);
    assert_eq!(removed.payload["kind"], "blocks");
    assert_eq!(removed.payload["from"]["identifier"], "TRK-1");
    assert_eq!(removed.payload["to"]["number"], 2);
    assert_eq!(
        log[0].payload, removed.payload,
        "same payload as when added"
    );
    let issue_one = s.get(1).await.issue.id;
    assert_eq!(removed.entity_id, issue_one);
    assert_eq!(other.from.number, 2);
    s.store.close().await;
}

#[tokio::test]
async fn link_events_record_the_actor_and_rolled_back_writes_leave_none() {
    let s = setup(3).await;
    s.store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind) VALUES (9, 'robot', 'agent')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let robot = Principal {
        user_id: 9,
        role: Role::Agent,
        scope: Scope::Write,
    };
    let link = s
        .store
        .write(move |tx| crate::links::add_link(tx, 100, &robot, "TRK", 1, 2, LinkKind::Blocks))
        .await
        .unwrap();
    assert_eq!(link.created_by, Some(9));
    s.store
        .write(move |tx| crate::links::remove_link(tx, 101, &robot, "TRK", link.id))
        .await
        .unwrap();
    let actors: Vec<Option<i64>> = s
        .store
        .read(|c| {
            events::read_events(
                c,
                0,
                Some(500),
                &EventFilter {
                    kind_prefix: Some("link.".into()),
                    ..EventFilter::default()
                },
            )
        })
        .await
        .unwrap()
        .iter()
        .map(|e| e.actor)
        .collect();
    assert_eq!(actors, [Some(9), Some(9)]);

    // A failure after the link and its event were written undoes both.
    let before = event_count(&s.store).await;
    let result: Result<(), StoreError> = s
        .store
        .write(|tx: &Transaction<'_>| {
            links::add_link(tx, 102, "TRK", 2, 3, LinkKind::Blocks)?;
            links::add_link(tx, 102, "TRK", 3, 2, LinkKind::Blocks)?;
            Ok(())
        })
        .await;
    assert!(matches!(
        rejection(result.unwrap_err()),
        Rejection::Conflict(_)
    ));
    assert_eq!(event_count(&s.store).await, before);
    assert!(s.links(2).await.blocks.is_empty());
    s.store.close().await;
}

async fn event_count(store: &Store) -> i64 {
    store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn an_issue_is_blocked_until_its_blockers_are_done_canceled_or_archived() {
    let s = setup(5).await;
    let (done, canceled, started) = (s.columns[3], s.columns[4], s.columns[2]);
    assert_eq!(s.blocked(2).await, (false, 0));

    s.add(1, 2, LinkKind::Blocks).await.unwrap();
    s.add(3, 2, LinkKind::Blocks).await.unwrap();
    // Related issues never block.
    s.add(4, 2, LinkKind::Relates).await.unwrap();
    assert_eq!(s.blocked(2).await, (true, 2));
    assert_eq!(s.blocked(1).await, (false, 0), "the blocker itself is free");

    s.mv(1, started).await;
    assert_eq!(s.blocked(2).await, (true, 2));
    s.mv(1, done).await;
    assert_eq!(s.blocked(2).await, (true, 1));
    s.mv(3, canceled).await;
    assert_eq!(s.blocked(2).await, (false, 0));

    // Reopening a blocker blocks again.
    s.mv(1, started).await;
    assert_eq!(s.blocked(2).await, (true, 1));
    // A new blocker blocks an issue that was free.
    s.mv(1, done).await;
    assert_eq!(s.blocked(2).await, (false, 0));
    s.add(5, 2, LinkKind::Blocks).await.unwrap();
    assert_eq!(s.blocked(2).await, (true, 1));

    // An archived blocker does not block, and keeps its link.
    s.store
        .write(|tx| archive_issue(tx, 400, "TRK", 5))
        .await
        .unwrap();
    assert_eq!(s.blocked(2).await, (false, 0));
    let kept = s.links(2).await.blocked_by;
    assert_eq!(numbers(&kept), [1, 3, 5]);
    assert!(kept[2].archived);

    // The per-project query names only open blockers of blocked issues.
    s.mv(3, started).await;
    let open = s
        .store
        .read(|c| links::open_blockers(c, "TRK"))
        .await
        .unwrap();
    let two = s.get(2).await.issue.id;
    assert_eq!(open.len(), 1);
    let names: Vec<&str> = open[&two].iter().map(|b| b.identifier.as_str()).collect();
    assert_eq!(names, ["TRK-3"]);
    s.store.close().await;
}

#[tokio::test]
async fn listing_many_issues_marks_the_blocked_ones() {
    let s = setup(150).await;
    // Every even issue is blocked by the issue before it; only some blockers are done.
    s.store
        .write(|tx| {
            for n in (2..=150).step_by(2) {
                links::add_link(tx, 100, "TRK", n - 1, n, LinkKind::Blocks)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let done = s.columns[3];
    for n in (1..=149).step_by(4) {
        s.mv(n, done).await;
    }
    let list = s
        .store
        .read(|c| {
            list_issues(
                c,
                "TRK",
                &IssueFilter {
                    limit: Some(issues::MAX_LIMIT),
                    ..IssueFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(list.issues.len(), 150);
    for issue in &list.issues {
        let expected = issue.number % 2 == 0 && (issue.number - 1) % 4 != 1;
        assert_eq!(
            issue.blocked, expected,
            "issue {} blocked={}",
            issue.number, issue.blocked
        );
        assert_eq!(issue.blocker_count, i64::from(expected));
    }
    assert_eq!(list.issues.iter().filter(|i| i.blocked).count(), 37);
    s.store.close().await;
}

#[test]
fn links_are_a_table_with_cascade_and_guards() {
    // The database itself refuses what the functions refuse.
    let dir = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("raw.db")).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
    for migration in crate::migrations::MIGRATIONS {
        conn.execute_batch(migration.sql).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO project (id, key, name, created) VALUES (1, 'P', 'P', 1);
         INSERT INTO \"column\" (id, project_id, name, state_type, position) VALUES (1, 1, 'c', 'backlog', 0);
         INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 't');
         INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
             VALUES (1, 1, 1, 1, 1, 'a', 1, 1, 1), (2, 1, 2, 1, 1, 'b', 2, 1, 1);",
    )
    .unwrap();
    let insert = |from: i64, to: i64, kind: &str| {
        conn.execute(
            "INSERT INTO issue_link (from_issue_id, to_issue_id, kind, created) VALUES (?1, ?2, ?3, 1)",
            rusqlite::params![from, to, kind],
        )
    };
    assert!(insert(1, 1, "blocks").is_err(), "self link");
    assert!(insert(1, 2, "other").is_err(), "unknown kind");
    assert!(
        insert(2, 1, "relates").is_err(),
        "relates stored lower id first"
    );
    insert(1, 2, "relates").unwrap();
    assert!(insert(1, 2, "relates").is_err(), "duplicate");
    insert(2, 1, "blocks").unwrap();
    // Deleting an issue (the product does not) takes its links with it.
    conn.execute("DELETE FROM issue WHERE id = 2", []).unwrap();
    let left: i64 = conn
        .query_row("SELECT COUNT(*) FROM issue_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}
