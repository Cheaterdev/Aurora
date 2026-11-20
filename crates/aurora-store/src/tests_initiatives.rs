//! Initiatives: validation, assignment, progress, health, listing, deletion and events.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::trace::{TraceEvent, TraceEventCodes};
use rusqlite::{Connection, Transaction};
use serde_json::json;

use crate::auth::{Principal, Role, Scope};
use crate::events::{self, Event, EventFilter};
use crate::initiatives::{
    self as store, Health, InitiativeStatus, MAX_DESCRIPTION_CHARS, MAX_NAME_CHARS, Progress,
    health_of, target_end_millis,
};
use crate::projects::Rejection;
use crate::tests_support::initiatives::{
    InitiativePatch, InitiativeView, NewInitiative, create_initiative, delete_initiative,
    update_initiative,
};
use crate::tests_support::issues::{
    self, IssuePatch, MoveTarget, NewIssue, archive_issue, create_issue, move_issue, update_issue,
};
use crate::tests_support::projects::{create_issue_type, create_project};
use crate::{Store, StoreError};

const DAY: i64 = 86_400_000;
const ROBOT: Principal = Principal {
    user_id: 9,
    role: Role::Agent,
    scope: Scope::Write,
};

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

fn is_invalid(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::Invalid(_))
}

fn is_not_found(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::NotFound(_))
}

fn is_conflict(err: StoreError) -> bool {
    matches!(rejection(err), Rejection::Conflict(_))
}

// ---- pure functions ----

fn progress(total: i64, done: i64) -> Progress {
    Progress::from_counts(total, done, 0, 0)
}

#[test]
fn progress_counts_done_over_total_and_rounds_the_percent_down() {
    let p = Progress::from_counts(7, 2, 1, 3);
    assert_eq!((p.total, p.completed, p.canceled, p.started), (7, 2, 1, 3));
    assert_eq!((p.open, p.done, p.percent), (1, 3, 42));
    assert!((p.done_ratio - 3.0 / 7.0).abs() < 1e-12);

    let empty = Progress::from_counts(0, 0, 0, 0);
    assert_eq!((empty.done_ratio, empty.percent, empty.open), (0.0, 0, 0));
    let all = Progress::from_counts(3, 2, 1, 0);
    assert_eq!((all.done_ratio, all.percent), (1.0, 100));
    assert_eq!(Progress::from_counts(1000, 999, 0, 0).percent, 99);
}

#[test]
fn target_dates_are_real_calendar_dates() {
    assert_eq!(target_end_millis("1970-01-01"), Some(DAY));
    assert_eq!(target_end_millis("2026-12-31"), Some(20_819 * DAY));
    assert!(target_end_millis("2024-02-29").is_some());
    assert!(target_end_millis("2000-02-29").is_some());
    for bad in [
        "2023-02-29",
        "1900-02-29",
        "2026-13-01",
        "2026-00-10",
        "2026-04-31",
        "2026-01-00",
        "2026-1-01",
        "26-01-01",
        "2026/01/01",
        "1969-12-31",
        "10000-01-01",
        "",
        "tomorrow",
        "2026-01-01T00:00",
        "+026-01-01",
    ] {
        assert_eq!(target_end_millis(bad), None, "{bad:?}");
    }
}

/// Created at 0, target 1970-01-10, so the window is ten days.
#[test]
fn health_table() {
    use Health::{AtRisk, OffTrack, OnTrack};
    use InitiativeStatus::{Active, Canceled, Completed, Paused, Planned};
    let target = Some("1970-01-10");
    let none = Health::None;
    #[allow(clippy::type_complexity)]
    let table: Vec<(&str, InitiativeStatus, Option<&str>, i64, Progress, Health)> = vec![
        // No signal.
        ("no target", Active, None, 5 * DAY, progress(4, 0), none),
        (
            "invalid target",
            Active,
            Some("soon"),
            5 * DAY,
            progress(4, 0),
            none,
        ),
        (
            "completed",
            Completed,
            target,
            20 * DAY,
            progress(4, 1),
            none,
        ),
        ("canceled", Canceled, target, 20 * DAY, progress(4, 1), none),
        (
            "planned, no issues",
            Planned,
            target,
            20 * DAY,
            progress(0, 0),
            none,
        ),
        // Planned with issues is judged like an active one.
        (
            "planned with issues",
            Planned,
            target,
            0,
            progress(4, 0),
            OnTrack,
        ),
        (
            "planned, behind",
            Planned,
            target,
            9 * DAY,
            progress(4, 0),
            AtRisk,
        ),
        // The target day is over.
        (
            "over, not done",
            Active,
            target,
            10 * DAY,
            progress(4, 3),
            OffTrack,
        ),
        (
            "over, long ago",
            Active,
            target,
            99 * DAY,
            progress(4, 0),
            OffTrack,
        ),
        (
            "over, paused",
            Paused,
            target,
            10 * DAY,
            progress(4, 3),
            OffTrack,
        ),
        (
            "over, no issues",
            Active,
            target,
            10 * DAY,
            progress(0, 0),
            OffTrack,
        ),
        (
            "over, all done",
            Active,
            target,
            99 * DAY,
            progress(4, 4),
            OnTrack,
        ),
        (
            "last millisecond",
            Active,
            target,
            10 * DAY - 1,
            progress(4, 3),
            AtRisk,
        ),
        (
            "last millisecond, all done",
            Active,
            target,
            10 * DAY - 1,
            progress(4, 4),
            OnTrack,
        ),
        // done_ratio + 0.2 against elapsed, with 3 of 10 done: the line is at half the window.
        (
            "on the line",
            Active,
            target,
            5 * DAY,
            progress(10, 3),
            OnTrack,
        ),
        (
            "just behind the line",
            Active,
            target,
            5 * DAY + 1,
            progress(10, 3),
            AtRisk,
        ),
        (
            "just ahead of the line",
            Active,
            target,
            5 * DAY - 1,
            progress(10, 3),
            OnTrack,
        ),
        (
            "half done on the line",
            Active,
            target,
            7 * DAY,
            progress(10, 5),
            OnTrack,
        ),
        (
            "half done behind",
            Active,
            target,
            7 * DAY + 1,
            progress(10, 5),
            AtRisk,
        ),
        (
            "paused is judged like active",
            Paused,
            target,
            8 * DAY,
            progress(10, 5),
            AtRisk,
        ),
        (
            "ahead of schedule",
            Active,
            target,
            8 * DAY,
            progress(10, 9),
            OnTrack,
        ),
        (
            "canceled counts as done",
            Active,
            target,
            7 * DAY,
            Progress::from_counts(10, 2, 3, 0),
            OnTrack,
        ),
        (
            "started does not count",
            Active,
            target,
            7 * DAY + 1,
            Progress::from_counts(10, 0, 0, 9),
            AtRisk,
        ),
        // No issues yet: ratio 0, so the line is at a fifth of the window.
        (
            "empty, early",
            Active,
            target,
            2 * DAY,
            progress(0, 0),
            OnTrack,
        ),
        (
            "empty, late",
            Active,
            target,
            2 * DAY + 1,
            progress(0, 0),
            AtRisk,
        ),
        // Odd clocks.
        (
            "before it was created",
            Active,
            target,
            -5 * DAY,
            progress(4, 0),
            OnTrack,
        ),
        (
            "all done mid-window",
            Active,
            target,
            5 * DAY,
            progress(4, 4),
            OnTrack,
        ),
    ];
    for (name, status, target, now, progress, expected) in table {
        assert_eq!(
            health_of(status, target, 0, now, &progress),
            expected,
            "{name}"
        );
    }

    // The window starts at creation, not at the epoch: created on day 4 of a
    // ten-day target, a fifth of the six days left is 1.2 days.
    let late = Some("1970-01-10");
    assert_eq!(
        health_of(Active, late, 4 * DAY, 4 * DAY + 1, &progress(5, 0)),
        OnTrack
    );
    assert_eq!(
        health_of(
            Active,
            late,
            4 * DAY,
            5 * DAY + 2 * DAY / 10 + 1,
            &progress(5, 0)
        ),
        AtRisk
    );
    // Created after the target day: the window has no length.
    assert_eq!(
        health_of(Active, target, 20 * DAY, 20 * DAY, &progress(4, 1)),
        OffTrack
    );
    assert_eq!(
        health_of(Active, target, 20 * DAY, 20 * DAY, &progress(4, 4)),
        OnTrack
    );
}

#[test]
fn health_and_status_serialize_as_documented() {
    let text = |h: Health| serde_json::to_value(h).unwrap();
    assert_eq!(text(Health::OnTrack), "on_track");
    assert_eq!(text(Health::AtRisk), "at_risk");
    assert_eq!(text(Health::OffTrack), "off_track");
    assert_eq!(text(Health::None), "none");
    for status in ["planned", "active", "paused", "completed", "canceled"] {
        let parsed = InitiativeStatus::parse(status).unwrap();
        assert_eq!(parsed.as_str(), status);
        assert_eq!(serde_json::to_value(parsed).unwrap(), status);
    }
    assert_eq!(InitiativeStatus::parse("Active"), None);
}

// ---- the store ----

struct S {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
    /// Columns of TRK: Backlog, Todo, In Progress, Done, Canceled.
    cols: Vec<i64>,
}

async fn setup() -> S {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let cols = store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind) VALUES (9, 'robot', 'agent')",
                [],
            )?;
            let trk = create_project(tx, 1, "TRK", "Tracker", None)?;
            create_project(tx, 1, "OTH", "Other", None)?;
            create_issue_type(tx, 1, "TRK", "Task")?;
            create_issue_type(tx, 1, "OTH", "Task")?;
            Ok(trk.columns.iter().map(|c| c.id).collect::<Vec<_>>())
        })
        .await
        .unwrap();
    S {
        _dir: dir,
        path,
        store,
        cols,
    }
}

fn named(name: &str) -> NewInitiative {
    NewInitiative {
        name: name.into(),
        ..NewInitiative::default()
    }
}

impl S {
    async fn create(
        &self,
        key: &'static str,
        new: NewInitiative,
    ) -> Result<InitiativeView, StoreError> {
        self.store
            .write(move |tx| create_initiative(tx, 1000, key, new))
            .await
    }

    async fn init(&self, name: &str) -> i64 {
        self.create("TRK", named(name)).await.unwrap().initiative.id
    }

    async fn update(
        &self,
        key: &'static str,
        id: i64,
        patch: InitiativePatch,
    ) -> Result<InitiativeView, StoreError> {
        self.store
            .write(move |tx| update_initiative(tx, 2000, key, id, patch))
            .await
    }

    async fn delete(&self, key: &'static str, id: i64) -> Result<(), StoreError> {
        self.store
            .write(move |tx| delete_initiative(tx, 3000, key, id))
            .await
    }

    /// A TRK issue in the column at `column` of [`S::cols`]; returns its number.
    async fn issue(&self, title: &str, initiative: Option<i64>, column: usize) -> i64 {
        let new = NewIssue {
            title: title.into(),
            column_id: Some(self.cols[column]),
            initiative,
            ..NewIssue::default()
        };
        self.store
            .write(move |tx| create_issue(tx, 1500, "TRK", new))
            .await
            .unwrap()
            .issue
            .number
    }

    async fn assign(&self, number: i64, initiative: Option<i64>) -> Result<(), StoreError> {
        self.store
            .write(move |tx| {
                update_issue(
                    tx,
                    2500,
                    "TRK",
                    number,
                    IssuePatch {
                        initiative: Some(initiative),
                        ..IssuePatch::default()
                    },
                )
                .map(|_| ())
            })
            .await
    }

    async fn mv(&self, number: i64, column: usize) {
        let column_id = self.cols[column];
        self.store
            .write(move |tx| {
                move_issue(
                    tx,
                    2600,
                    "TRK",
                    number,
                    MoveTarget {
                        column_id,
                        ..MoveTarget::default()
                    },
                )
            })
            .await
            .unwrap();
    }

    async fn get(&self, id: i64) -> store::InitiativeDetail {
        self.store
            .read(move |c| store::get_initiative(c, 5000, "TRK", id, None, None))
            .await
            .unwrap()
    }

    async fn list(&self, status: Option<InitiativeStatus>) -> Vec<InitiativeView> {
        self.store
            .read(move |c| store::list_initiatives(c, 5000, "TRK", status))
            .await
            .unwrap()
    }

    async fn log(&self, prefix: &str) -> Vec<Event> {
        let prefix = prefix.to_string();
        self.store
            .read(move |c| {
                events::read_events(
                    c,
                    0,
                    Some(500),
                    &EventFilter {
                        kind_prefix: Some(prefix),
                        ..EventFilter::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn event_count(&self) -> i64 {
        self.store
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    async fn initiative_of(&self, number: i64) -> Option<i64> {
        self.store
            .read(move |c| issues::get_issue(c, "TRK", number))
            .await
            .unwrap()
            .issue
            .initiative_id
    }
}

#[tokio::test]
async fn create_and_read_with_defaults_and_all_fields() {
    let s = setup().await;
    let made = s.create("TRK", named("  Launch  ")).await.unwrap();
    let i = &made.initiative;
    assert_eq!(i.name, "Launch", "the name is trimmed");
    assert_eq!(i.status, InitiativeStatus::Planned);
    assert_eq!(
        (i.description.as_str(), i.target_date.as_deref()),
        ("", None)
    );
    assert_eq!((i.created, i.updated), (1000, 1000));
    assert_eq!(made.progress, progress(0, 0));
    assert_eq!(made.health, Health::None);

    let full = s
        .create(
            "TRK",
            NewInitiative {
                name: "Beta".into(),
                description: Some("line one\nline two".into()),
                status: Some(InitiativeStatus::Active),
                target_date: Some(" 2030-06-30 ".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(full.initiative.target_date.as_deref(), Some("2030-06-30"));
    assert_eq!(full.initiative.description, "line one\nline two");
    assert_eq!(full.initiative.status, InitiativeStatus::Active);
    assert_eq!(full.health, Health::OnTrack);

    let detail = s.get(full.initiative.id).await;
    assert_eq!(detail.view, full);
    assert!(detail.issues.is_empty());
    assert_eq!((detail.total, detail.limit, detail.offset), (0, 50, 0));

    let value = serde_json::to_value(&detail).unwrap();
    assert_eq!(value["name"], "Beta");
    assert_eq!(value["progress"]["total"], 0);
    assert_eq!(value["health"], "on_track");
    assert_eq!(value["status"], "active");
    s.store.close().await;
}

#[tokio::test]
async fn validation_refuses_bad_input_and_changes_nothing() {
    let s = setup().await;
    let before = s.event_count().await;
    let long_name = "n".repeat(MAX_NAME_CHARS + 1);
    for new in [
        named(""),
        named("   "),
        named(&long_name),
        NewInitiative {
            description: Some("d".repeat(MAX_DESCRIPTION_CHARS + 1)),
            ..named("ok")
        },
        NewInitiative {
            target_date: Some("2026-02-30".into()),
            ..named("ok")
        },
        NewInitiative {
            target_date: Some("next week".into()),
            ..named("ok")
        },
    ] {
        assert!(is_invalid(s.create("TRK", new).await.unwrap_err()));
    }
    let longest = NewInitiative {
        description: Some("d".repeat(MAX_DESCRIPTION_CHARS)),
        ..named(&"n".repeat(MAX_NAME_CHARS))
    };
    let id = s.create("TRK", longest).await.unwrap().initiative.id;

    for patch in [
        InitiativePatch {
            name: Some(String::new()),
            ..InitiativePatch::default()
        },
        InitiativePatch {
            description: Some(Some("d".repeat(MAX_DESCRIPTION_CHARS + 1))),
            ..InitiativePatch::default()
        },
        InitiativePatch {
            target_date: Some(Some("2026-00-01".into())),
            ..InitiativePatch::default()
        },
    ] {
        assert!(is_invalid(s.update("TRK", id, patch).await.unwrap_err()));
    }
    assert!(is_not_found(
        s.create("NOPE", named("x")).await.unwrap_err()
    ));
    assert_eq!(
        s.event_count().await,
        before + 1,
        "only the valid create wrote"
    );
    assert_eq!(s.list(None).await.len(), 1);
    s.store.close().await;
}

#[tokio::test]
async fn names_are_unique_within_a_project_only() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    s.init("Beta").await;
    assert!(is_conflict(
        s.create("TRK", named("Alpha")).await.unwrap_err()
    ));
    assert!(is_conflict(
        s.update(
            "TRK",
            a,
            InitiativePatch {
                name: Some("Beta".into()),
                ..InitiativePatch::default()
            }
        )
        .await
        .unwrap_err()
    ));
    // The same name in another project is fine, and renaming to itself is a no-op.
    s.create("OTH", named("Alpha")).await.unwrap();
    s.update(
        "TRK",
        a,
        InitiativePatch {
            name: Some("Alpha".into()),
            ..InitiativePatch::default()
        },
    )
    .await
    .unwrap();
    s.store.close().await;
}

#[tokio::test]
async fn patch_changes_only_what_it_names_and_null_clears() {
    let s = setup().await;
    let made = s
        .create(
            "TRK",
            NewInitiative {
                description: Some("about".into()),
                target_date: Some("2030-01-01".into()),
                ..named("Launch")
            },
        )
        .await
        .unwrap();
    let id = made.initiative.id;

    let renamed = s
        .update(
            "TRK",
            id,
            InitiativePatch {
                name: Some("Launch 2".into()),
                status: Some(InitiativeStatus::Active),
                ..InitiativePatch::default()
            },
        )
        .await
        .unwrap();
    let i = &renamed.initiative;
    assert_eq!(
        (i.name.as_str(), i.status),
        ("Launch 2", InitiativeStatus::Active)
    );
    assert_eq!(i.description, "about");
    assert_eq!(i.target_date.as_deref(), Some("2030-01-01"));
    assert_eq!((i.created, i.updated), (1000, 2000));

    let cleared = s
        .update(
            "TRK",
            id,
            InitiativePatch {
                description: Some(None),
                target_date: Some(None),
                ..InitiativePatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cleared.initiative.description, "");
    assert_eq!(cleared.initiative.target_date, None);
    assert_eq!(cleared.health, Health::None);

    let retarget = s
        .update(
            "TRK",
            id,
            InitiativePatch {
                target_date: Some(Some("2031-02-03".into())),
                ..InitiativePatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        retarget.initiative.target_date.as_deref(),
        Some("2031-02-03")
    );

    // The JSON forms the API accepts: absent, null and a value.
    let patch: InitiativePatch =
        serde_json::from_value(json!({"target_date": null, "description": null})).unwrap();
    assert_eq!(
        (patch.target_date, patch.description),
        (Some(None), Some(None))
    );
    let patch: InitiativePatch = serde_json::from_value(json!({"name": "x"})).unwrap();
    assert_eq!((patch.target_date, patch.description), (None, None));
    assert!(serde_json::from_value::<InitiativePatch>(json!({"status": "done"})).is_err());
    assert!(serde_json::from_value::<InitiativePatch>(json!({"color": "red"})).is_err());

    assert!(is_not_found(
        s.update("TRK", id + 100, InitiativePatch::default())
            .await
            .unwrap_err()
    ));
    s.store.close().await;
}

#[tokio::test]
async fn an_initiative_of_another_project_is_not_found() {
    let s = setup().await;
    let other = s
        .create("OTH", named("Theirs"))
        .await
        .unwrap()
        .initiative
        .id;
    assert!(is_not_found(
        s.store
            .read(move |c| store::get_initiative(c, 5000, "TRK", other, None, None))
            .await
            .unwrap_err()
    ));
    assert!(is_not_found(
        s.update("TRK", other, InitiativePatch::default())
            .await
            .unwrap_err()
    ));
    assert!(is_not_found(s.delete("TRK", other).await.unwrap_err()));
    assert!(s.list(None).await.is_empty());
    s.delete("OTH", other).await.unwrap();
    s.store.close().await;
}

// ---- assignment ----

#[tokio::test]
async fn an_issue_joins_one_initiative_of_its_own_project() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let b = s.init("Beta").await;
    let theirs = s
        .create("OTH", named("Theirs"))
        .await
        .unwrap()
        .initiative
        .id;

    let first = s.issue("first", Some(a), 1).await;
    let second = s.issue("second", None, 1).await;
    assert_eq!(s.initiative_of(first).await, Some(a));
    assert_eq!(s.initiative_of(second).await, None);
    let json = serde_json::to_value(
        s.store
            .read(move |c| issues::get_issue(c, "TRK", first))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json["initiative_id"], a);

    // Joining, moving to another initiative, leaving.
    s.assign(second, Some(a)).await.unwrap();
    s.assign(second, Some(b)).await.unwrap();
    assert_eq!(s.initiative_of(second).await, Some(b));
    assert_eq!((s.get(a).await.total, s.get(b).await.total), (1, 1));
    s.assign(second, None).await.unwrap();
    assert_eq!(s.initiative_of(second).await, None);
    assert_eq!(s.get(b).await.total, 0);

    // Another project's initiative, and one that does not exist.
    let before = s.event_count().await;
    for bad in [theirs, 9999] {
        assert!(is_invalid(s.assign(second, Some(bad)).await.unwrap_err()));
        let new = NewIssue {
            title: "x".into(),
            initiative: Some(bad),
            ..NewIssue::default()
        };
        let err = s
            .store
            .write(move |tx| create_issue(tx, 1, "TRK", new))
            .await
            .unwrap_err();
        assert!(is_invalid(err));
    }
    assert_eq!(s.event_count().await, before, "refusals wrote nothing");
    assert_eq!(s.initiative_of(second).await, None);

    // Assigning an issue of the other project the same way is refused too.
    let other_issue = s
        .store
        .write(|tx| {
            create_issue(
                tx,
                1,
                "OTH",
                NewIssue {
                    title: "o".into(),
                    ..NewIssue::default()
                },
            )
        })
        .await
        .unwrap()
        .issue
        .number;
    let err = s
        .store
        .write(move |tx| {
            update_issue(
                tx,
                2,
                "OTH",
                other_issue,
                IssuePatch {
                    initiative: Some(Some(a)),
                    ..IssuePatch::default()
                },
            )
        })
        .await
        .unwrap_err();
    assert!(is_invalid(err));
    s.store.close().await;
}

#[tokio::test]
async fn issue_events_carry_the_initiative() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let number = s.issue("one", Some(a), 1).await;
    s.issue("two", None, 1).await;
    s.assign(number, None).await.unwrap();
    s.assign(number, None).await.unwrap();
    s.assign(number, Some(a)).await.unwrap();
    s.assign(number, Some(a)).await.unwrap();

    let created = s.log("issue.created").await;
    assert_eq!(created[0].payload["initiative"], a);
    assert!(created[1].payload["initiative"].is_null());
    let updated = s.log("issue.updated").await;
    assert_eq!(
        updated.len(),
        2,
        "assigning the same value again is a no-op"
    );
    assert_eq!(
        updated[0].payload["changes"],
        json!({"initiative": {"from": a, "to": null}})
    );
    assert_eq!(
        updated[1].payload["changes"],
        json!({"initiative": {"from": null, "to": a}})
    );
    s.store.close().await;
}

// ---- progress ----

#[tokio::test]
async fn progress_counts_completed_and_canceled_over_the_non_archived_total() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let b = s.init("Beta").await;
    let empty = s.init("Empty").await;

    // Alpha: backlog, todo, started, completed, canceled; plus an archived
    // completed one that must not count, and one in Beta.
    s.issue("backlog", Some(a), 0).await;
    s.issue("todo", Some(a), 1).await;
    s.issue("started", Some(a), 2).await;
    let done = s.issue("done", Some(a), 3).await;
    s.issue("canceled", Some(a), 4).await;
    let hidden = s.issue("hidden", Some(a), 3).await;
    s.issue("beta", Some(b), 3).await;
    s.issue("free", None, 3).await;
    s.store
        .write(move |tx| archive_issue(tx, 2700, "TRK", hidden))
        .await
        .unwrap();

    let detail = s.get(a).await;
    let p = &detail.view.progress;
    assert_eq!(
        (p.total, p.completed, p.canceled, p.started, p.open, p.done),
        (5, 1, 1, 1, 2, 2)
    );
    assert_eq!((p.percent, p.done_ratio), (40, 0.4));
    assert_eq!(detail.total, 5);
    assert_eq!(s.get(b).await.view.progress, progress(1, 1));
    assert_eq!(s.get(empty).await.view.progress, progress(0, 0));
    assert_eq!(s.get(empty).await.view.progress.percent, 0);

    // Moving an issue to Done moves the numbers; restoring the archived one adds it.
    s.mv(done, 2).await;
    assert_eq!(s.get(a).await.view.progress.completed, 0);
    s.mv(done, 3).await;
    assert_eq!(s.get(a).await.view.progress.completed, 1);
    s.store
        .write(move |tx| issues::unarchive_issue(tx, 2800, "TRK", hidden))
        .await
        .unwrap();
    let p = s.get(a).await.view.progress;
    assert_eq!((p.total, p.completed), (6, 2));

    // Every column reports in the same way through the list.
    let listed = s.list(None).await;
    assert_eq!(listed[0].progress, p);
    s.store.close().await;
}

#[tokio::test]
async fn the_detail_lists_non_archived_issues_in_board_order_and_pages() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let late = s.issue("late", Some(a), 3).await;
    let early = s.issue("early", Some(a), 1).await;
    let middle = s.issue("middle", Some(a), 2).await;
    let archived = s.issue("archived", Some(a), 1).await;
    s.issue("other", None, 1).await;
    s.store
        .write(move |tx| archive_issue(tx, 2700, "TRK", archived))
        .await
        .unwrap();

    let detail = s.get(a).await;
    let numbers: Vec<i64> = detail.issues.iter().map(|i| i.number).collect();
    assert_eq!(numbers, [early, middle, late]);
    let first = &detail.issues[0];
    assert_eq!(first.identifier, format!("TRK-{early}"));
    assert_eq!(first.title, "early");
    assert_eq!(first.column_id, s.cols[1]);
    assert_eq!(first.state_type, crate::projects::StateType::Unstarted);
    let value = serde_json::to_value(&detail).unwrap();
    assert_eq!(value["issues"][2]["state_type"], "completed");
    assert_eq!(value["total"], 3);

    let page = s
        .store
        .read(move |c| store::get_initiative(c, 5000, "TRK", a, Some(2), Some(1)))
        .await
        .unwrap();
    let numbers: Vec<i64> = page.issues.iter().map(|i| i.number).collect();
    assert_eq!(numbers, [middle, late]);
    assert_eq!((page.total, page.limit, page.offset), (3, 2, 1));

    let clamped = s
        .store
        .read(move |c| store::get_initiative(c, 5000, "TRK", a, Some(100_000), None))
        .await
        .unwrap();
    assert_eq!(clamped.limit, 200);
    let floor = s
        .store
        .read(move |c| store::get_initiative(c, 5000, "TRK", a, Some(0), None))
        .await
        .unwrap();
    assert_eq!((floor.limit, floor.issues.len()), (1, 1));
    let err = s
        .store
        .read(move |c| store::get_initiative(c, 5000, "TRK", a, None, Some(-1)))
        .await
        .unwrap_err();
    assert!(is_invalid(err));
    s.store.close().await;
}

// ---- listing ----

#[tokio::test]
async fn the_list_has_progress_and_health_and_filters_by_status() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let b = s
        .create(
            "TRK",
            NewInitiative {
                status: Some(InitiativeStatus::Active),
                target_date: Some("1970-01-01".into()),
                ..named("Beta")
            },
        )
        .await
        .unwrap()
        .initiative
        .id;
    s.update(
        "TRK",
        a,
        InitiativePatch {
            status: Some(InitiativeStatus::Paused),
            ..InitiativePatch::default()
        },
    )
    .await
    .unwrap();
    s.issue("one", Some(b), 3).await;
    s.issue("two", Some(b), 1).await;

    let all = s.list(None).await;
    assert_eq!(
        all.iter().map(|v| v.initiative.id).collect::<Vec<_>>(),
        [a, b]
    );
    assert_eq!(all[0].progress, progress(0, 0));
    assert_eq!(all[1].progress, progress(2, 1));
    // The listing clock is 5000 ms: day one of 1970 is not over and half is done.
    assert_eq!(all[1].health, Health::OnTrack);
    assert_eq!(all[0].health, Health::None);
    let later = s
        .store
        .read(|c| store::list_initiatives(c, 2 * DAY, "TRK", None))
        .await
        .unwrap();
    assert_eq!(later[1].health, Health::OffTrack, "the clock is injected");
    assert_eq!(all[1], s.get(b).await.view);

    let active = s.list(Some(InitiativeStatus::Active)).await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].initiative.id, b);
    assert_eq!(
        s.list(Some(InitiativeStatus::Paused)).await[0]
            .initiative
            .id,
        a
    );
    assert!(s.list(Some(InitiativeStatus::Completed)).await.is_empty());

    let err = s
        .store
        .read(|c| store::list_initiatives(c, 5000, "NOPE", None))
        .await
        .unwrap_err();
    assert!(is_not_found(err));
    s.store.close().await;
}

#[tokio::test]
async fn listing_runs_the_same_number_of_statements_for_any_number_of_initiatives() {
    let s = setup().await;
    let counted = |path: PathBuf| async move {
        tokio::task::spawn_blocking(move || {
            let mut conn = Connection::open(path).unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&count);
            conn.trace_v2(
                TraceEventCodes::SQLITE_TRACE_STMT,
                Some(move |event: TraceEvent<'_>| {
                    if matches!(event, TraceEvent::Stmt(..)) {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                }),
            )
            .unwrap();
            let found = store::list_initiatives(&conn, 5000, "TRK", None).unwrap();
            (found.len(), count.load(Ordering::SeqCst))
        })
        .await
        .unwrap()
    };

    for n in 0..3 {
        let id = s.init(&format!("few {n}")).await;
        s.issue("x", Some(id), 3).await;
    }
    let (few, few_statements) = counted(s.path.clone()).await;
    for n in 0..60 {
        let id = s.init(&format!("many {n}")).await;
        s.issue("x", Some(id), n % 5).await;
        s.issue("y", Some(id), 3).await;
    }
    let (many, many_statements) = counted(s.path.clone()).await;
    assert_eq!((few, many), (3, 63));
    assert!(few_statements > 0);
    assert_eq!(
        few_statements, many_statements,
        "the list must not query once per initiative"
    );

    let listed = s.list(None).await;
    assert_eq!(listed[3].progress.total, 2);
    assert_eq!(listed[62].progress.total, 2);
    s.store.close().await;
}

// ---- deleting ----

#[tokio::test]
async fn deleting_unassigns_every_issue_and_emits_their_events() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let keep = s.init("Keep").await;
    let one = s.issue("one", Some(a), 1).await;
    let two = s.issue("two", Some(a), 3).await;
    let hidden = s.issue("hidden", Some(a), 3).await;
    let kept = s.issue("kept", Some(keep), 1).await;
    s.store
        .write(move |tx| archive_issue(tx, 2700, "TRK", hidden))
        .await
        .unwrap();
    let before = s.log("").await.len();

    s.store
        .write(move |tx| crate::initiatives::delete_initiative(tx, 3000, &ROBOT, "TRK", a))
        .await
        .unwrap();

    for number in [one, two, hidden] {
        assert_eq!(s.initiative_of(number).await, None, "issue {number}");
    }
    assert_eq!(s.initiative_of(kept).await, Some(keep));
    assert!(is_not_found(
        s.store
            .read(move |c| store::get_initiative(c, 1, "TRK", a, None, None))
            .await
            .unwrap_err()
    ));
    assert_eq!(s.get(keep).await.total, 1);
    let titles: Vec<String> = {
        let got = s
            .store
            .read(|c| {
                issues::list_issues(
                    c,
                    "TRK",
                    &issues::IssueFilter {
                        include_archived: true,
                        ..issues::IssueFilter::default()
                    },
                )
            })
            .await
            .unwrap();
        got.issues.into_iter().map(|i| i.title).collect()
    };
    assert_eq!(
        titles.len(),
        4,
        "the issues are kept, archived one included"
    );

    let log = s.log("").await;
    let fresh = &log[before..];
    let kinds: Vec<&str> = fresh.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "issue.updated",
            "issue.updated",
            "issue.updated",
            "initiative.deleted"
        ]
    );
    for (event, number) in fresh.iter().zip([one, two, hidden]) {
        assert_eq!(event.actor, Some(9));
        assert_eq!(event.project.as_deref(), Some("TRK"));
        assert_eq!(event.created, 3000);
        assert_eq!(
            event.payload["changes"],
            json!({"initiative": {"from": a, "to": null}})
        );
        let issue_id = s
            .store
            .read(move |c| issues::get_issue(c, "TRK", number))
            .await
            .unwrap()
            .issue
            .id;
        assert_eq!(event.entity_id, issue_id);
    }
    let deleted = &fresh[3];
    assert_eq!(
        (deleted.entity_kind.as_str(), deleted.entity_id),
        ("initiative", a)
    );
    assert_eq!(deleted.actor, Some(9));
    assert_eq!(deleted.payload, json!({"name": "Alpha", "unassigned": 3}));
    s.store.close().await;
}

#[tokio::test]
async fn deleting_an_empty_initiative_and_an_unknown_one() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    s.delete("TRK", a).await.unwrap();
    let deleted = s.log("initiative.deleted").await;
    assert_eq!(
        deleted[0].payload,
        json!({"name": "Alpha", "unassigned": 0})
    );
    assert!(is_not_found(s.delete("TRK", a).await.unwrap_err()));
    assert!(is_not_found(s.delete("NOPE", a).await.unwrap_err()));
    // The name is free again.
    s.init("Alpha").await;
    s.store.close().await;
}

// ---- events ----

#[tokio::test]
async fn events_record_actor_project_and_only_real_changes() {
    let s = setup().await;
    let made = s
        .store
        .write(|tx| {
            crate::initiatives::create_initiative(
                tx,
                100,
                &ROBOT,
                "TRK",
                NewInitiative {
                    target_date: Some("2030-05-06".into()),
                    ..named("Launch")
                },
            )
        })
        .await
        .unwrap();
    let id = made.initiative.id;
    let patch = |f: fn(&mut InitiativePatch)| {
        let mut p = InitiativePatch::default();
        f(&mut p);
        p
    };
    for (now, p) in [
        (101, patch(|p| p.name = Some("Launch 2".into()))),
        (102, patch(|p| p.status = Some(InitiativeStatus::Active))),
        (103, patch(|p| p.description = Some(Some("text".into())))),
        (104, patch(|p| p.target_date = Some(None))),
        (
            105,
            InitiativePatch {
                name: Some("Launch 3".into()),
                status: Some(InitiativeStatus::Paused),
                target_date: Some(Some("2031-01-02".into())),
                ..InitiativePatch::default()
            },
        ),
    ] {
        s.store
            .write(move |tx| crate::initiatives::update_initiative(tx, now, &ROBOT, "TRK", id, p))
            .await
            .unwrap();
    }
    let before = s.event_count().await;
    // Writing what is already there changes nothing: no event, no new timestamp.
    let same = s
        .store
        .write(move |tx| {
            crate::initiatives::update_initiative(
                tx,
                999,
                &ROBOT,
                "TRK",
                id,
                InitiativePatch {
                    name: Some(" Launch 3 ".into()),
                    description: Some(Some("text".into())),
                    status: Some(InitiativeStatus::Paused),
                    target_date: Some(Some("2031-01-02".into())),
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(same.initiative.updated, 105);
    let empty = s
        .store
        .write(move |tx| {
            crate::initiatives::update_initiative(
                tx,
                999,
                &ROBOT,
                "TRK",
                id,
                InitiativePatch::default(),
            )
        })
        .await
        .unwrap();
    assert_eq!(empty.initiative.updated, 105);
    assert_eq!(s.event_count().await, before);

    let log = s.log("initiative.").await;
    let kinds: Vec<&str> = log.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "initiative.created",
            "initiative.updated",
            "initiative.updated",
            "initiative.updated",
            "initiative.updated",
            "initiative.updated"
        ]
    );
    for event in &log {
        assert_eq!(event.actor, Some(9));
        assert_eq!(event.project.as_deref(), Some("TRK"));
        assert_eq!(
            (event.entity_kind.as_str(), event.entity_id),
            ("initiative", id)
        );
    }
    assert_eq!(
        log[0].payload,
        json!({"name": "Launch", "status": "planned", "target_date": "2030-05-06"})
    );
    assert_eq!(
        log[1].payload,
        json!({"changes": {"name": {"from": "Launch", "to": "Launch 2"}}})
    );
    assert_eq!(
        log[2].payload,
        json!({"changes": {"status": {"from": "planned", "to": "active"}}})
    );
    assert_eq!(
        log[3].payload,
        json!({"changes": {"description": {"from": "", "to": "text"}}})
    );
    assert_eq!(
        log[4].payload,
        json!({"changes": {"target_date": {"from": "2030-05-06", "to": null}}})
    );
    assert_eq!(
        log[5].payload,
        json!({"changes": {
            "name": {"from": "Launch 2", "to": "Launch 3"},
            "status": {"from": "active", "to": "paused"},
            "target_date": {"from": null, "to": "2031-01-02"},
        }})
    );
    s.store.close().await;
}

#[tokio::test]
async fn a_rolled_back_change_leaves_no_row_and_no_event() {
    let s = setup().await;
    let a = s.init("Alpha").await;
    let number = s.issue("one", Some(a), 1).await;
    let before = s.event_count().await;

    let result: Result<(), StoreError> = s
        .store
        .write(move |tx: &Transaction<'_>| {
            create_initiative(tx, 10, "TRK", named("Doomed"))?;
            update_initiative(
                tx,
                10,
                "TRK",
                a,
                InitiativePatch {
                    name: Some("Renamed".into()),
                    ..InitiativePatch::default()
                },
            )?;
            delete_initiative(tx, 10, "TRK", a)?;
            // Fails after the writes above, so all of them are undone.
            create_initiative(tx, 10, "TRK", named("  "))?;
            Ok(())
        })
        .await;
    assert!(is_invalid(result.unwrap_err()));
    assert_eq!(s.event_count().await, before);
    let all = s.list(None).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].initiative.name, "Alpha");
    assert_eq!(
        s.initiative_of(number).await,
        Some(a),
        "the unassign was undone"
    );
    s.store.close().await;
}
