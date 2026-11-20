//! Every project and issue write records the acting user on its event.

use crate::auth::{Principal, Role, Scope};
use crate::events::{self, EventFilter};
use crate::issues::{self, IssuePatch, MoveTarget, NewIssue};
use crate::projects::{self, StateType};
use crate::{Store, StoreError};

fn principal(user_id: i64, role: Role) -> Principal {
    Principal {
        user_id,
        role,
        scope: Scope::Write,
    }
}

async fn run<T, F>(store: &Store, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
{
    store.write(f).await.unwrap()
}

/// (kind, actor) of every event, oldest first.
async fn log(store: &Store) -> Vec<(String, Option<i64>)> {
    store
        .read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.kind, e.actor))
        .collect()
}

#[tokio::test]
async fn every_write_records_the_user_who_made_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    run(&store, |tx| {
        for (id, name, kind) in [(7, "ann", "human"), (8, "robot", "agent")] {
            tx.execute(
                "INSERT INTO user (id, name, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, name, kind],
            )?;
        }
        Ok(())
    })
    .await;
    let ann = principal(7, Role::Owner);
    let robot = principal(8, Role::Agent);

    // Each step is one change; the actor alternates so a mix-up cannot pass.
    let project = run(&store, move |tx| {
        projects::create_project(tx, 1, &ann, "TRK", "Tracker", None)
    })
    .await;
    run(&store, move |tx| {
        projects::rename_project(tx, 2, &robot, "TRK", "Tracker 2")
    })
    .await;
    let review = run(&store, move |tx| {
        projects::create_column(tx, 3, &ann, "TRK", "Review", StateType::Started)
    })
    .await
    .id;
    run(&store, move |tx| {
        projects::rename_column(tx, 4, &robot, "TRK", review, "QA")
    })
    .await;
    let mut order: Vec<i64> = project.columns.iter().map(|c| c.id).collect();
    order.push(review);
    order.reverse();
    run(&store, move |tx| {
        projects::reorder_columns(tx, 5, &ann, "TRK", &order)
    })
    .await;
    let spare = run(&store, move |tx| {
        projects::create_column(tx, 6, &robot, "TRK", "Spare", StateType::Started)
    })
    .await
    .id;
    run(&store, move |tx| {
        projects::delete_column(tx, 7, &ann, "TRK", spare)
    })
    .await;
    let task = run(&store, move |tx| {
        projects::create_issue_type(tx, 8, &robot, "TRK", "Task")
    })
    .await
    .id;
    run(&store, move |tx| {
        projects::rename_issue_type(tx, 9, &ann, "TRK", task, "Chore")
    })
    .await;
    run(&store, move |tx| {
        projects::delete_issue_type(tx, 10, &robot, "TRK", task)
    })
    .await;
    let keep = run(&store, move |tx| {
        projects::create_issue_type(tx, 10, &ann, "TRK", "Keep")
    })
    .await
    .id;
    let label = run(&store, move |tx| {
        projects::create_label(tx, 11, &ann, "TRK", "urgent", "#ff0000")
    })
    .await
    .id;
    run(&store, move |tx| {
        projects::rename_label(tx, 12, &robot, "TRK", label, "p0")
    })
    .await;
    run(&store, move |tx| {
        projects::recolor_label(tx, 13, &ann, "TRK", label, "#00ff00")
    })
    .await;
    run(&store, move |tx| {
        projects::delete_label(tx, 14, &robot, "TRK", label)
    })
    .await;
    run(&store, move |tx| {
        issues::create_issue(
            tx,
            15,
            &ann,
            "TRK",
            NewIssue {
                title: "x".into(),
                type_id: Some(keep),
                ..NewIssue::default()
            },
        )
    })
    .await;
    run(&store, move |tx| {
        issues::update_issue(
            tx,
            16,
            &robot,
            "TRK",
            1,
            IssuePatch {
                title: Some("y".into()),
                ..IssuePatch::default()
            },
        )
    })
    .await;
    run(&store, move |tx| {
        issues::move_issue(
            tx,
            17,
            &ann,
            "TRK",
            1,
            MoveTarget {
                column_id: review,
                ..MoveTarget::default()
            },
        )
    })
    .await;
    run(&store, move |tx| {
        issues::archive_issue(tx, 18, &robot, "TRK", 1)
    })
    .await;
    run(&store, move |tx| {
        issues::unarchive_issue(tx, 19, &ann, "TRK", 1)
    })
    .await;

    let (ann, robot) = (Some(7), Some(8));
    let expected = [
        ("project.created", ann),
        ("project.updated", robot),
        ("column.created", ann),
        ("column.renamed", robot),
        ("column.reordered", ann),
        ("column.created", robot),
        ("column.deleted", ann),
        ("issue_type.created", robot),
        ("issue_type.renamed", ann),
        ("issue_type.deleted", robot),
        ("issue_type.created", ann),
        ("label.created", ann),
        ("label.renamed", robot),
        ("label.recolored", ann),
        ("label.deleted", robot),
        ("issue.created", ann),
        ("issue.updated", robot),
        ("issue.moved", ann),
        ("issue.archived", robot),
        ("issue.unarchived", ann),
    ];
    let expected: Vec<(String, Option<i64>)> = expected
        .into_iter()
        .map(|(kind, actor)| (kind.to_string(), actor))
        .collect();
    assert_eq!(log(&store).await, expected);
    store.close().await;
}
