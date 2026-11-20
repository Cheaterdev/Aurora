//! Issues: numbering, references, parents, ordering, filters and events.

use serde_json::{Value, json};

use crate::issues::*;
use crate::projects::{
    ProjectDetail, Rejection, StateType, create_issue_type, create_label, create_project,
};
use crate::{Store, StoreError};

struct Setup {
    _dir: tempfile::TempDir,
    store: Store,
    columns: Vec<i64>,
    type_id: i64,
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

fn titled(title: &str) -> NewIssue {
    NewIssue {
        title: title.into(),
        ..NewIssue::default()
    }
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let project: ProjectDetail = store
        .write(|tx| create_project(tx, 1, "TRK", "Tracker", None))
        .await
        .unwrap();
    let task = store
        .write(|tx| create_issue_type(tx, 1, "TRK", "Task"))
        .await
        .unwrap();
    Setup {
        _dir: dir,
        store,
        columns: project.columns.iter().map(|c| c.id).collect(),
        type_id: task.id,
    }
}

impl Setup {
    async fn create(&self, new: NewIssue) -> IssueDetail {
        self.store
            .write(move |tx| create_issue(tx, 1000, "TRK", new))
            .await
            .unwrap()
    }

    async fn create_err(&self, new: NewIssue) -> Rejection {
        rejection(
            self.store
                .write(move |tx| create_issue(tx, 1000, "TRK", new))
                .await
                .unwrap_err(),
        )
    }

    async fn get(&self, number: i64) -> IssueDetail {
        self.store
            .read(move |c| get_issue(c, "TRK", number))
            .await
            .unwrap()
    }

    async fn update(&self, number: i64, patch: IssuePatch) -> Result<IssueDetail, StoreError> {
        self.store
            .write(move |tx| update_issue(tx, 2000, "TRK", number, patch))
            .await
    }

    async fn mv(&self, number: i64, target: MoveTarget) -> Result<IssueDetail, StoreError> {
        self.store
            .write(move |tx| move_issue(tx, 3000, "TRK", number, target))
            .await
    }

    async fn list(&self, filter: IssueFilter) -> IssueList {
        self.store
            .read(move |c| list_issues(c, "TRK", &filter))
            .await
            .unwrap()
    }

    /// Issue numbers of a column in board order, archived ones included.
    async fn order(&self, column_id: i64) -> Vec<i64> {
        self.list(IssueFilter {
            column_id: Some(column_id),
            include_archived: true,
            ..IssueFilter::default()
        })
        .await
        .issues
        .iter()
        .map(|i| i.number)
        .collect()
    }

    async fn label(&self, name: &'static str) -> i64 {
        self.store
            .write(move |tx| create_label(tx, 1, "TRK", name, "#112233"))
            .await
            .unwrap()
            .id
    }

    async fn user(&self, id: i64) {
        self.store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO user (id, name, kind) VALUES (?1, 'U', 'human')",
                    [id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// (kind, entity_kind, entity_id, project_id, payload) of every event, oldest first.
    async fn events(&self) -> Vec<(String, String, i64, Option<i64>, Value)> {
        self.store
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT kind, entity_kind, entity_id, project_id, payload_json, actor_id
                     FROM event ORDER BY id",
                )?;
                let rows = stmt.query_map([], |r| {
                    let payload: String = r.get(4)?;
                    assert_eq!(r.get::<_, Option<i64>>(5)?, None, "actor is NULL for now");
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        serde_json::from_str(&payload).unwrap(),
                    ))
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap()
    }

    async fn event_count(&self) -> usize {
        self.events().await.len()
    }
}

fn to(column_id: i64) -> MoveTarget {
    MoveTarget {
        column_id,
        ..MoveTarget::default()
    }
}

#[tokio::test]
async fn create_with_defaults() {
    let s = setup().await;
    let issue = s.create(titled("  First  ")).await;
    assert_eq!(issue.issue.number, 1);
    assert_eq!(issue.issue.identifier, "TRK-1");
    assert_eq!(issue.issue.title, "First");
    assert_eq!(issue.issue.description, "");
    assert_eq!(
        issue.issue.type_id, s.type_id,
        "the only type is the default"
    );
    assert_eq!(issue.issue.column_id, s.columns[0], "first backlog column");
    assert_eq!(issue.issue.position, 1024.0);
    assert_eq!((issue.issue.created, issue.issue.updated), (1000, 1000));
    assert!(!issue.issue.archived);
    assert_eq!(issue.issue.parent, None);
    assert_eq!(issue.issue.assignee_id, None);
    assert_eq!(issue.issue.priority, None);
    assert!(issue.issue.labels.is_empty() && issue.children.is_empty());
    let second = s.create(titled("Second")).await;
    assert_eq!(second.issue.position, 2048.0, "new issues go to the end");
    s.store.close().await;
}

#[tokio::test]
async fn create_with_every_field() {
    let s = setup().await;
    s.user(7).await;
    let bug = s
        .store
        .write(|tx| create_issue_type(tx, 1, "TRK", "Bug"))
        .await
        .unwrap();
    let (a, b) = (s.label("a").await, s.label("b").await);
    let parent = s
        .create(NewIssue {
            type_id: Some(s.type_id),
            ..titled("Parent")
        })
        .await;
    let issue = s
        .create(NewIssue {
            title: "Child".into(),
            description: Some("# markdown\n\ntext".into()),
            type_id: Some(bug.id),
            column_id: Some(s.columns[2]),
            assignee_id: Some(7),
            parent: Some(parent.issue.number),
            priority: Some(Priority::High),
            labels: vec![b, a, b],
        })
        .await;
    let i = &issue.issue;
    assert_eq!(i.description, "# markdown\n\ntext");
    assert_eq!((i.type_id, i.column_id), (bug.id, s.columns[2]));
    assert_eq!((i.assignee_id, i.parent), (Some(7), Some(1)));
    assert_eq!(i.priority, Some(Priority::High));
    assert_eq!(i.labels, [a, b], "sorted, without repeats");
    assert_eq!(s.get(2).await, issue);

    let json = serde_json::to_value(&issue).unwrap();
    assert_eq!(json["identifier"], "TRK-2");
    assert_eq!(json["priority"], "high");
    assert!(json.get("parent_id").is_none());
    s.store.close().await;
}

#[tokio::test]
async fn type_is_required_when_the_project_has_several_or_none() {
    let s = setup().await;
    s.store
        .write(|tx| create_issue_type(tx, 1, "TRK", "Bug"))
        .await
        .unwrap();
    assert!(matches!(
        s.create_err(titled("x")).await,
        Rejection::Invalid(_)
    ));
    s.store
        .write(|tx| create_project(tx, 1, "NOT", "No types", None))
        .await
        .unwrap();
    let err = s
        .store
        .write(|tx| create_issue(tx, 1, "NOT", titled("x")))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    s.store.close().await;
}

#[tokio::test]
async fn default_column_is_first_backlog_else_first() {
    let s = setup().await;
    s.store
        .write(|tx| {
            let own = [
                crate::projects::NewColumn {
                    name: "Open".into(),
                    state_type: StateType::Unstarted,
                },
                crate::projects::NewColumn {
                    name: "Later".into(),
                    state_type: StateType::Backlog,
                },
                crate::projects::NewColumn {
                    name: "Even later".into(),
                    state_type: StateType::Backlog,
                },
            ];
            create_project(tx, 1, "OWN", "Own", Some(&own))?;
            create_issue_type(tx, 1, "OWN", "Task")?;
            create_project(
                tx,
                1,
                "NOB",
                "No backlog",
                Some(&[crate::projects::NewColumn {
                    name: "Open".into(),
                    state_type: StateType::Started,
                }]),
            )?;
            create_issue_type(tx, 1, "NOB", "Task")?;
            Ok(())
        })
        .await
        .unwrap();
    let (own, nob) = s
        .store
        .write(|tx| {
            Ok((
                create_issue(tx, 1, "OWN", titled("x"))?,
                create_issue(tx, 1, "NOB", titled("x"))?,
            ))
        })
        .await
        .unwrap();
    let (own_id, nob_id) = (own.issue.column_id, nob.issue.column_id);
    let names: Vec<String> = s
        .store
        .read(move |c| {
            [own_id, nob_id]
                .into_iter()
                .map(|id| {
                    Ok(
                        c.query_row("SELECT name FROM \"column\" WHERE id = ?1", [id], |r| {
                            r.get(0)
                        })?,
                    )
                })
                .collect()
        })
        .await
        .unwrap();
    assert_eq!(names, ["Later", "Open"]);
    s.store.close().await;
}

#[tokio::test]
async fn title_and_description_limits() {
    let s = setup().await;
    for bad in ["", "   ", &"x".repeat(MAX_TITLE_CHARS + 1)] {
        assert!(matches!(
            s.create_err(titled(bad)).await,
            Rejection::Invalid(_)
        ));
    }
    s.create(titled(&"x".repeat(MAX_TITLE_CHARS))).await;
    let long = NewIssue {
        description: Some("d".repeat(MAX_DESCRIPTION_CHARS + 1)),
        ..titled("t")
    };
    assert!(matches!(s.create_err(long).await, Rejection::Invalid(_)));
    let max = NewIssue {
        description: Some("d".repeat(MAX_DESCRIPTION_CHARS)),
        ..titled("t")
    };
    s.create(max).await;
    let too_long = IssuePatch {
        description: Some(Some("d".repeat(MAX_DESCRIPTION_CHARS + 1))),
        ..IssuePatch::default()
    };
    assert!(matches!(
        rejection(s.update(1, too_long).await.unwrap_err()),
        Rejection::Invalid(_)
    ));
    s.store.close().await;
}

#[tokio::test]
async fn numbers_are_never_reused_and_a_rollback_does_not_consume_one() {
    let s = setup().await;
    for expected in 1..=3 {
        assert_eq!(s.create(titled("n")).await.issue.number, expected);
    }
    s.store
        .write(|tx| archive_issue(tx, 5, "TRK", 2))
        .await
        .unwrap();
    assert_eq!(s.create(titled("n")).await.issue.number, 4);

    let before = s.event_count().await;
    let outcome = s
        .store
        .write(|tx| {
            let made = create_issue(tx, 9, "TRK", titled("rolled back"))?;
            assert_eq!(made.issue.number, 5);
            Err::<(), _>(StoreError::Closed)
        })
        .await;
    assert!(matches!(outcome, Err(StoreError::Closed)));
    assert_eq!(
        s.event_count().await,
        before,
        "a rolled-back create left no event"
    );
    assert_eq!(s.create(titled("n")).await.issue.number, 5);

    // A refused create does not consume a number either.
    s.create_err(titled("")).await;
    assert_eq!(s.create(titled("n")).await.issue.number, 6);
    s.store.close().await;
}

#[tokio::test]
async fn numbers_are_per_project() {
    let s = setup().await;
    s.store
        .write(|tx| {
            create_project(tx, 1, "OTH", "Other", None)?;
            create_issue_type(tx, 1, "OTH", "Task")?;
            Ok(())
        })
        .await
        .unwrap();
    s.create(titled("a")).await;
    let other = s
        .store
        .write(|tx| create_issue(tx, 1, "OTH", titled("b")))
        .await
        .unwrap();
    assert_eq!(other.issue.identifier, "OTH-1");
    s.store.close().await;
}

#[tokio::test]
async fn references_must_belong_to_the_project() {
    let s = setup().await;
    let other = s
        .store
        .write(|tx| {
            let project = create_project(tx, 1, "OTH", "Other", None)?;
            let kind = create_issue_type(tx, 1, "OTH", "Task")?;
            let label = create_label(tx, 1, "OTH", "x", "#000000")?;
            let issue = create_issue(tx, 1, "OTH", titled("theirs"))?;
            Ok((project.columns[1].id, kind.id, label.id, issue.issue.number))
        })
        .await
        .unwrap();
    let (column, kind, label, number) = other;
    s.create(titled("mine")).await;
    s.create(titled("mine too")).await;

    let cases = [
        NewIssue {
            type_id: Some(kind),
            ..titled("x")
        },
        NewIssue {
            column_id: Some(column),
            ..titled("x")
        },
        NewIssue {
            labels: vec![label],
            ..titled("x")
        },
        NewIssue {
            parent: Some(number + 5),
            ..titled("x")
        },
        NewIssue {
            assignee_id: Some(999),
            ..titled("x")
        },
        NewIssue {
            type_id: Some(99_999),
            ..titled("x")
        },
    ];
    for case in cases {
        assert!(matches!(s.create_err(case).await, Rejection::Invalid(_)));
    }
    // OTH-1 exists, but it is not in TRK: TRK-1 is the issue called "mine".
    let child = s
        .create(NewIssue {
            parent: Some(1),
            ..titled("c")
        })
        .await;
    assert_eq!(child.issue.parent, Some(1));

    let patches = [
        IssuePatch {
            type_id: Some(kind),
            ..IssuePatch::default()
        },
        IssuePatch {
            labels: Some(vec![label]),
            ..IssuePatch::default()
        },
        IssuePatch {
            parent: Some(Some(99)),
            ..IssuePatch::default()
        },
        IssuePatch {
            assignee_id: Some(Some(999)),
            ..IssuePatch::default()
        },
    ];
    for patch in patches {
        assert!(matches!(
            rejection(s.update(1, patch).await.unwrap_err()),
            Rejection::Invalid(_)
        ));
    }
    let err = s.mv(1, to(column)).await.unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));

    let missing = s.store.read(|c| get_issue(c, "TRK", 99)).await.unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    let missing = s.store.read(|c| get_issue(c, "NOPE", 1)).await.unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    s.store.close().await;
}

#[tokio::test]
async fn parent_cycles_are_refused() {
    let s = setup().await;
    // 1 <- 2 <- 3
    s.create(titled("one")).await;
    s.create(NewIssue {
        parent: Some(1),
        ..titled("two")
    })
    .await;
    s.create(NewIssue {
        parent: Some(2),
        ..titled("three")
    })
    .await;
    for (issue, parent) in [(1, 1), (1, 2), (1, 3), (2, 3), (3, 3)] {
        let patch = IssuePatch {
            parent: Some(Some(parent)),
            ..IssuePatch::default()
        };
        let err = s.update(issue, patch).await.unwrap_err();
        assert!(
            matches!(rejection(err), Rejection::Conflict(_)),
            "{issue} under {parent}"
        );
    }
    assert_eq!(s.get(1).await.issue.parent, None);
    // Moving a subtree elsewhere and clearing the parent are fine.
    s.create(titled("four")).await;
    s.update(
        2,
        IssuePatch {
            parent: Some(Some(4)),
            ..IssuePatch::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.get(3).await.issue.parent, Some(2));
    s.update(
        2,
        IssuePatch {
            parent: Some(None),
            ..IssuePatch::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.get(2).await.issue.parent, None);
    s.store.close().await;
}

#[tokio::test]
async fn nesting_is_limited_to_eight_levels() {
    let s = setup().await;
    s.create(titled("level 1")).await;
    for level in 2..=8 {
        let issue = s
            .create(NewIssue {
                parent: Some(level - 1),
                ..titled("level")
            })
            .await;
        assert_eq!(issue.issue.number, level);
    }
    let ninth = s
        .create_err(NewIssue {
            parent: Some(8),
            ..titled("level 9")
        })
        .await;
    assert!(matches!(ninth, Rejection::Invalid(_)), "{ninth:?}");

    // Re-parenting counts the whole subtree: a top-level chain of 2 cannot go
    // under level 7 (7 + 2 = 9), but fits under level 6.
    s.create(titled("top")).await; // 9
    s.create(NewIssue {
        parent: Some(9),
        ..titled("top child")
    })
    .await; // 10
    let under = |parent: i64| IssuePatch {
        parent: Some(Some(parent)),
        ..IssuePatch::default()
    };
    let err = s.update(9, under(7)).await.unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    s.update(9, under(6)).await.unwrap();
    s.store.close().await;
}

#[tokio::test]
async fn children_are_listed_under_the_parent_in_board_order() {
    let s = setup().await;
    s.create(titled("parent")).await;
    for title in ["a", "b", "c"] {
        s.create(NewIssue {
            parent: Some(1),
            ..titled(title)
        })
        .await;
    }
    // Move "a" (2) after "c" (4); put "b" (3) in another column.
    s.mv(
        2,
        MoveTarget {
            after: Some(4),
            ..to(s.columns[0])
        },
    )
    .await
    .unwrap();
    s.mv(3, to(s.columns[1])).await.unwrap();
    let parent = s.get(1).await;
    let seen: Vec<_> = parent
        .children
        .iter()
        .map(|c| (c.number, c.title.as_str(), c.column_id))
        .collect();
    assert_eq!(
        seen,
        [
            (4, "c", s.columns[0]),
            (2, "a", s.columns[0]),
            (3, "b", s.columns[1])
        ]
    );
    // Archiving the parent keeps its children, unarchived.
    s.store
        .write(|tx| archive_issue(tx, 1, "TRK", 1))
        .await
        .unwrap();
    let parent = s.get(1).await;
    assert_eq!(parent.children.len(), 3);
    assert!(parent.children.iter().all(|c| !c.archived));
    assert_eq!(s.get(2).await.issue.parent, Some(1));
    s.store.close().await;
}

#[tokio::test]
async fn move_within_and_across_columns() {
    let s = setup().await;
    let (backlog, todo) = (s.columns[0], s.columns[1]);
    for title in ["A", "B", "C", "D"] {
        s.create(titled(title)).await;
    }
    assert_eq!(s.order(backlog).await, [1, 2, 3, 4]);

    let before = |column_id, number| MoveTarget {
        before: Some(number),
        ..to(column_id)
    };
    let after = |column_id, number| MoveTarget {
        after: Some(number),
        ..to(column_id)
    };

    s.mv(4, before(backlog, 1)).await.unwrap();
    assert_eq!(s.order(backlog).await, [4, 1, 2, 3]);
    s.mv(1, after(backlog, 3)).await.unwrap();
    assert_eq!(s.order(backlog).await, [4, 2, 3, 1]);
    s.mv(2, to(backlog)).await.unwrap();
    assert_eq!(
        s.order(backlog).await,
        [4, 3, 1, 2],
        "no target means the end"
    );
    let moved = s
        .mv(
            3,
            MoveTarget {
                position: Some(1e6),
                ..to(backlog)
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.issue.position, 1e6);
    assert_eq!(s.order(backlog).await, [4, 1, 2, 3]);
    assert_eq!(moved.issue.updated, 3000);

    // Across columns: only the column and position change.
    let across = s.mv(1, to(todo)).await.unwrap();
    assert_eq!(across.issue.column_id, todo);
    assert_eq!(
        across.issue.position, 1024.0,
        "an empty column starts at 1024"
    );
    assert_eq!(s.order(backlog).await, [4, 2, 3]);
    s.mv(2, to(todo)).await.unwrap();
    s.mv(3, before(todo, 1)).await.unwrap();
    assert_eq!(s.order(todo).await, [3, 1, 2]);
    s.mv(4, after(todo, 1)).await.unwrap();
    assert_eq!(s.order(todo).await, [3, 1, 4, 2]);
    assert!(s.order(backlog).await.is_empty());

    // The list is ordered by column position, then issue position.
    let all = s.list(IssueFilter::default()).await;
    let numbers: Vec<_> = all.issues.iter().map(|i| i.number).collect();
    assert_eq!(numbers, [3, 1, 4, 2]);

    // Refusals change nothing.
    let events = s.event_count().await;
    for bad in [
        MoveTarget {
            before: Some(1),
            after: Some(2),
            ..to(todo)
        },
        MoveTarget {
            before: Some(1),
            position: Some(5.0),
            ..to(todo)
        },
        MoveTarget {
            position: Some(f64::NAN),
            ..to(todo)
        },
        before(backlog, 1),
        before(todo, 99),
        before(todo, 3),
    ] {
        let err = s.mv(3, bad).await.unwrap_err();
        assert!(matches!(rejection(err), Rejection::Invalid(_)));
    }
    assert_eq!(s.event_count().await, events);
    s.store.close().await;
}

#[tokio::test]
async fn moving_to_where_the_issue_already_is_changes_nothing() {
    let s = setup().await;
    let backlog = s.columns[0];
    for title in ["A", "B", "C"] {
        s.create(titled(title)).await;
    }
    let events = s.event_count().await;
    s.mv(3, to(backlog)).await.unwrap();
    s.mv(
        2,
        MoveTarget {
            before: Some(3),
            ..to(backlog)
        },
    )
    .await
    .unwrap();
    s.mv(
        2,
        MoveTarget {
            after: Some(1),
            ..to(backlog)
        },
    )
    .await
    .unwrap();
    let same = s.get(2).await.issue.position;
    s.mv(
        2,
        MoveTarget {
            position: Some(same),
            ..to(backlog)
        },
    )
    .await
    .unwrap();
    assert_eq!(s.event_count().await, events);
    assert_eq!(s.get(2).await.issue.updated, 1000);
    s.store.close().await;
}

#[tokio::test]
async fn inserting_between_the_same_neighbours_renumbers_the_column() {
    let s = setup().await;
    let backlog = s.columns[0];
    s.create(titled("first")).await; // 1
    s.create(titled("last")).await; // 2
    let mut inserted = Vec::new();
    for round in 0..40 {
        let issue = s.create(titled("between")).await;
        let number = issue.issue.number;
        s.mv(
            number,
            MoveTarget {
                after: Some(1),
                ..to(backlog)
            },
        )
        .await
        .unwrap();
        inserted.push(number);
        let mut expected = vec![1];
        expected.extend(inserted.iter().rev());
        expected.push(2);
        assert_eq!(s.order(backlog).await, expected, "after round {round}");
    }
    let renumbered = s
        .events()
        .await
        .into_iter()
        .filter(|e| e.0 == "issue.moved" && e.4["renumbered"] == true)
        .count();
    assert!(renumbered >= 1, "the renumbering path ran");

    let positions: Vec<f64> = s
        .list(IssueFilter {
            limit: Some(200),
            ..IssueFilter::default()
        })
        .await
        .issues
        .iter()
        .map(|i| i.position)
        .collect();
    assert!(
        positions.windows(2).all(|w| w[1] - w[0] >= 1e-4),
        "neighbours keep room: {positions:?}"
    );
    s.store.close().await;
}

#[tokio::test]
async fn update_has_patch_semantics() {
    let s = setup().await;
    s.user(3).await;
    let bug = s
        .store
        .write(|tx| create_issue_type(tx, 1, "TRK", "Bug"))
        .await
        .unwrap();
    let (a, b) = (s.label("a").await, s.label("b").await);
    s.create(NewIssue {
        type_id: Some(s.type_id),
        ..titled("parent")
    })
    .await;
    s.create(NewIssue {
        description: Some("text".into()),
        assignee_id: Some(3),
        parent: Some(1),
        priority: Some(Priority::Low),
        labels: vec![a],
        type_id: Some(s.type_id),
        ..titled("child")
    })
    .await;

    // Absent keeps everything.
    let same = s.update(2, IssuePatch::default()).await.unwrap();
    assert_eq!(same.issue.updated, 1000);

    let changed = s
        .update(
            2,
            IssuePatch {
                title: Some(" New title ".into()),
                type_id: Some(bug.id),
                priority: Some(Some(Priority::Urgent)),
                labels: Some(vec![b]),
                ..IssuePatch::default()
            },
        )
        .await
        .unwrap();
    let i = &changed.issue;
    assert_eq!(i.title, "New title");
    assert_eq!(i.type_id, bug.id);
    assert_eq!(i.priority, Some(Priority::Urgent));
    assert_eq!(i.labels, [b], "the label set is replaced");
    assert_eq!(i.updated, 2000);
    assert_eq!(
        (i.description.as_str(), i.assignee_id, i.parent),
        ("text", Some(3), Some(1)),
        "fields that were absent kept their value"
    );

    // null clears.
    let cleared = s
        .update(
            2,
            IssuePatch {
                description: Some(None),
                assignee_id: Some(None),
                parent: Some(None),
                priority: Some(None),
                labels: Some(vec![]),
                ..IssuePatch::default()
            },
        )
        .await
        .unwrap();
    let i = &cleared.issue;
    assert_eq!(
        (i.description.as_str(), i.assignee_id, i.parent, i.priority),
        ("", None, None, None)
    );
    assert!(i.labels.is_empty());
    assert!(s.get(1).await.children.is_empty());

    // An empty title is refused and nothing changes.
    let err = s
        .update(
            2,
            IssuePatch {
                title: Some(" ".into()),
                priority: Some(Some(Priority::High)),
                ..IssuePatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    assert_eq!(s.get(2).await.issue.priority, None);

    // JSON: absent, null and a value are told apart; the column is not patchable.
    let patch: IssuePatch = serde_json::from_value(json!({"assignee_id": null})).unwrap();
    assert_eq!(patch.assignee_id, Some(None));
    assert_eq!(patch.parent, None);
    let patch: IssuePatch = serde_json::from_value(json!({"parent": 4})).unwrap();
    assert_eq!(patch.parent, Some(Some(4)));
    assert!(serde_json::from_value::<IssuePatch>(json!({"column_id": 1})).is_err());
    assert!(serde_json::from_value::<IssuePatch>(json!({"priority": "asap"})).is_err());
    s.store.close().await;
}

#[tokio::test]
async fn archive_and_unarchive() {
    let s = setup().await;
    for title in ["a", "b", "c"] {
        s.create(titled(title)).await;
    }
    let archived = s
        .store
        .write(|tx| archive_issue(tx, 4000, "TRK", 2))
        .await
        .unwrap();
    assert!(archived.issue.archived);
    assert_eq!(archived.issue.updated, 4000);
    assert_eq!(
        archived.issue.column_id, s.columns[0],
        "it keeps its column"
    );
    assert_eq!(archived.issue.number, 2);
    // Still readable, hidden from the default list.
    assert!(s.get(2).await.issue.archived);
    let default = s.list(IssueFilter::default()).await;
    assert_eq!(default.total, 2);
    assert_eq!(
        default.issues.iter().map(|i| i.number).collect::<Vec<_>>(),
        [1, 3]
    );
    let events = s.event_count().await;
    s.store
        .write(|tx| archive_issue(tx, 4500, "TRK", 2))
        .await
        .unwrap();
    assert_eq!(
        s.event_count().await,
        events,
        "archiving twice writes nothing"
    );

    let back = s
        .store
        .write(|tx| unarchive_issue(tx, 5000, "TRK", 2))
        .await
        .unwrap();
    assert!(!back.issue.archived);
    assert_eq!(
        s.order(s.columns[0]).await,
        [1, 2, 3],
        "same place as before"
    );
    let events = s.event_count().await;
    s.store
        .write(|tx| unarchive_issue(tx, 5500, "TRK", 2))
        .await
        .unwrap();
    assert_eq!(s.event_count().await, events);

    let missing = s
        .store
        .write(|tx| archive_issue(tx, 1, "TRK", 99))
        .await
        .unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    s.store.close().await;
}

#[tokio::test]
async fn list_filters_and_pagination() {
    let s = setup().await;
    s.user(1).await;
    s.user(2).await;
    let (urgent, other) = (s.label("urgent").await, s.label("other").await);
    let (backlog, todo, started) = (s.columns[0], s.columns[1], s.columns[2]);

    // 1 top-level, backlog, user 1, urgent
    s.create(NewIssue {
        assignee_id: Some(1),
        labels: vec![urgent],
        ..titled("one")
    })
    .await;
    // 2 child of 1, todo, user 2, other
    s.create(NewIssue {
        parent: Some(1),
        column_id: Some(todo),
        assignee_id: Some(2),
        labels: vec![other],
        ..titled("two")
    })
    .await;
    // 3 top-level, started, urgent + other
    s.create(NewIssue {
        column_id: Some(started),
        labels: vec![urgent, other],
        ..titled("three")
    })
    .await;
    // 4 child of 1, todo, archived
    s.create(NewIssue {
        parent: Some(1),
        column_id: Some(todo),
        ..titled("four")
    })
    .await;
    s.store
        .write(|tx| archive_issue(tx, 1, "TRK", 4))
        .await
        .unwrap();

    let numbers = |list: &IssueList| list.issues.iter().map(|i| i.number).collect::<Vec<_>>();
    let all = s.list(IssueFilter::default()).await;
    assert_eq!((numbers(&all), all.total), (vec![1, 2, 3], 3));
    assert_eq!((all.limit, all.offset), (50, 0));

    let setup = &s;
    let f = |filter: IssueFilter| async move { numbers(&setup.list(filter).await) };
    assert_eq!(
        f(IssueFilter {
            column_id: Some(todo),
            ..IssueFilter::default()
        })
        .await,
        [2]
    );
    assert_eq!(
        f(IssueFilter {
            state_type: Some(StateType::Started),
            ..IssueFilter::default()
        })
        .await,
        [3]
    );
    assert_eq!(
        f(IssueFilter {
            state_type: Some(StateType::Unstarted),
            include_archived: true,
            ..IssueFilter::default()
        })
        .await,
        [2, 4]
    );
    assert_eq!(
        f(IssueFilter {
            label_id: Some(urgent),
            ..IssueFilter::default()
        })
        .await,
        [1, 3]
    );
    assert_eq!(
        f(IssueFilter {
            assignee_id: Some(2),
            ..IssueFilter::default()
        })
        .await,
        [2]
    );
    assert_eq!(
        f(IssueFilter {
            parent: Some(ParentFilter::None),
            ..IssueFilter::default()
        })
        .await,
        [1, 3]
    );
    assert_eq!(
        f(IssueFilter {
            parent: Some(ParentFilter::Number(1)),
            ..IssueFilter::default()
        })
        .await,
        [2]
    );
    assert_eq!(
        f(IssueFilter {
            parent: Some(ParentFilter::Number(1)),
            include_archived: true,
            ..IssueFilter::default()
        })
        .await,
        [2, 4]
    );
    assert_eq!(
        f(IssueFilter {
            archived_only: true,
            ..IssueFilter::default()
        })
        .await,
        [4]
    );
    assert_eq!(
        f(IssueFilter {
            archived_only: true,
            include_archived: true,
            ..IssueFilter::default()
        })
        .await,
        [4],
        "archived_only wins"
    );
    assert_eq!(
        f(IssueFilter {
            include_archived: true,
            ..IssueFilter::default()
        })
        .await,
        [1, 2, 4, 3]
    );
    assert_eq!(
        f(IssueFilter {
            label_id: Some(urgent),
            state_type: Some(StateType::Started),
            ..IssueFilter::default()
        })
        .await,
        [3],
        "filters combine"
    );

    // Pagination: the total ignores limit and offset.
    let page = s
        .list(IssueFilter {
            include_archived: true,
            limit: Some(2),
            offset: Some(1),
            ..IssueFilter::default()
        })
        .await;
    assert_eq!((numbers(&page), page.total), (vec![2, 4], 4));
    assert_eq!((page.limit, page.offset), (2, 1));
    let past = s
        .list(IssueFilter {
            offset: Some(10),
            ..IssueFilter::default()
        })
        .await;
    assert!(past.issues.is_empty() && past.total == 3);
    let clamped = s
        .list(IssueFilter {
            limit: Some(10_000),
            ..IssueFilter::default()
        })
        .await;
    assert_eq!(clamped.limit, MAX_LIMIT);
    let bad = s
        .store
        .read(|c| {
            list_issues(
                c,
                "TRK",
                &IssueFilter {
                    offset: Some(-1),
                    ..IssueFilter::default()
                },
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(bad), Rejection::Invalid(_)));
    assert_eq!(all.issues[0].labels, [urgent]);
    let _ = backlog;
    s.store.close().await;
}

#[tokio::test]
async fn every_change_writes_exactly_one_event_with_its_payload() {
    let s = setup().await;
    s.user(5).await;
    let label = s.label("x").await;
    let base = s.event_count().await;
    let project_id: i64 = s
        .store
        .read(|c| Ok(c.query_row("SELECT id FROM project", [], |r| r.get(0))?))
        .await
        .unwrap();

    s.create(titled("parent")).await;
    let child = s
        .create(NewIssue {
            parent: Some(1),
            assignee_id: Some(5),
            priority: Some(Priority::Medium),
            labels: vec![label],
            ..titled("child")
        })
        .await;
    s.update(
        2,
        IssuePatch {
            title: Some("renamed".into()),
            priority: Some(None),
            labels: Some(vec![]),
            ..IssuePatch::default()
        },
    )
    .await
    .unwrap();
    s.mv(2, to(s.columns[1])).await.unwrap();
    s.store
        .write(|tx| archive_issue(tx, 1, "TRK", 2))
        .await
        .unwrap();
    s.store
        .write(|tx| unarchive_issue(tx, 1, "TRK", 2))
        .await
        .unwrap();

    let events = s.events().await;
    let events = &events[base..];
    let kinds: Vec<_> = events.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "issue.created",
            "issue.created",
            "issue.updated",
            "issue.moved",
            "issue.archived",
            "issue.unarchived"
        ]
    );
    for e in events {
        assert_eq!(e.1, "issue");
        assert_eq!(e.3, Some(project_id));
    }
    assert_eq!(events[1].2, child.issue.id);

    let created = &events[1].4;
    assert_eq!(created["title"], "child");
    assert_eq!(created["number"], 2);
    assert_eq!(created["identifier"], "TRK-2");
    assert_eq!(created["type_id"], s.type_id);
    assert_eq!(created["column_id"], s.columns[0]);
    assert_eq!(created["parent"], 1);
    assert_eq!(created["assignee_id"], 5);
    assert_eq!(created["priority"], "medium");
    assert_eq!(created["labels"], json!([label]));

    let updated = &events[2].4["changes"];
    assert_eq!(updated["title"], json!({"from": "child", "to": "renamed"}));
    assert_eq!(updated["priority"], json!({"from": "medium", "to": null}));
    assert_eq!(updated["labels"], json!({"from": [label], "to": []}));
    assert_eq!(updated.as_object().unwrap().len(), 3, "only changed fields");

    let moved = &events[3].4;
    assert_eq!(moved["from"]["column_id"], s.columns[0]);
    assert_eq!(moved["to"]["column_id"], s.columns[1]);
    assert_eq!(moved["from"]["position"], 2048.0);
    assert_eq!(moved["to"]["position"], 1024.0);
    assert_eq!(moved["renumbered"], false);

    assert_eq!(events[4].4["number"], 2);
    s.store.close().await;
}

#[tokio::test]
async fn refused_and_rolled_back_changes_leave_no_event() {
    let s = setup().await;
    s.create(titled("a")).await;
    s.create(titled("b")).await;
    let before = s.event_count().await;

    // Refusals.
    s.create_err(titled("")).await;
    s.update(
        1,
        IssuePatch {
            title: Some("fine".into()),
            parent: Some(Some(1)),
            ..IssuePatch::default()
        },
    )
    .await
    .unwrap_err();
    s.mv(1, to(99_999)).await.unwrap_err();
    assert_eq!(s.event_count().await, before);
    assert_eq!(
        s.get(1).await.issue.title,
        "a",
        "the refused update changed nothing"
    );

    // Rolled back after the operations succeeded.
    let columns = s.columns.clone();
    let outcome = s
        .store
        .write(move |tx| {
            update_issue(
                tx,
                1,
                "TRK",
                1,
                IssuePatch {
                    title: Some("changed".into()),
                    ..IssuePatch::default()
                },
            )?;
            move_issue(tx, 1, "TRK", 2, to(columns[1]))?;
            archive_issue(tx, 1, "TRK", 2)?;
            Err::<(), _>(StoreError::Closed)
        })
        .await;
    assert!(matches!(outcome, Err(StoreError::Closed)));
    assert_eq!(s.event_count().await, before);
    let one = s.get(1).await;
    let two = s.get(2).await;
    assert_eq!(one.issue.title, "a");
    assert_eq!(
        (two.issue.column_id, two.issue.archived),
        (s.columns[0], false)
    );

    // Writes that change nothing write no event.
    s.update(1, IssuePatch::default()).await.unwrap();
    s.update(
        1,
        IssuePatch {
            title: Some(" a ".into()),
            ..IssuePatch::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.event_count().await, before);
    s.store.close().await;
}
