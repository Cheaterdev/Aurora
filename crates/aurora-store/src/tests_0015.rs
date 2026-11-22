//! Migration 0015 and the working copy per run (TRK-16): the project's
//! repository settings, the plan made when a run is created, how the end of a
//! run settles it, start-up reconciliation, replay and the invariant.

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::agents::{self, NewAgent};
use crate::auth::{Principal, Role, Scope};
use crate::events::{self, EventFilter};
use crate::invariants;
use crate::migrations::MIGRATIONS;
use crate::projects::{ProjectPatch, Rejection};
use crate::runs::{
    self, NewRun, Outcome, RunFilter, RunReason, RunStatus, WorktreeEnd, WorktreeState,
    branch_name, create_run, finish_run, get_run, mark_running, worktree_dir,
};
use crate::tests_support::issues::{self, NewIssue};
use crate::tests_support::{projects, test_actor};
use crate::{OpenOptions, Store, StoreError, default_backup_dir};

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(r) => r,
        other => panic!("expected a rejection, got {other}"),
    }
}

fn abs(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("aurora-tests-{name}"))
        .display()
        .to_string()
}

fn owner() -> Principal {
    Principal {
        user_id: crate::tests_support::TEST_USER_ID,
        role: Role::Owner,
        scope: Scope::Write,
    }
}

#[tokio::test]
async fn populated_version_fourteen_database_gets_the_working_copy_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let v14 =
        Store::open_with_migrations(&path, &OpenOptions::default(), &MIGRATIONS[..14]).unwrap();
    assert_eq!(v14.schema_version(), 14);
    v14.close().await;
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(
        "INSERT INTO user (id, name, kind, role) VALUES
             (1, 'Owner', 'human', 'owner'), (2, 'bot', 'agent', 'agent');
         INSERT INTO project (id, key, name, created, next_issue_number, rules)
             VALUES (1, 'AA', 'First', 10, 2, 'be kind');
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
    assert_eq!(store.schema_version(), 17);
    let backup = store.open_report().backup.clone().expect("backup taken");
    assert!(backup.starts_with(default_backup_dir(&path)));
    let (repo, base, rules, run) = store
        .read(|c| {
            let project = c.query_row(
                "SELECT repo_path, base_branch, rules FROM project WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )?;
            let run = get_run(c, 7)?;
            Ok((project.0, project.1, project.2, run))
        })
        .await
        .unwrap();
    assert_eq!((repo, base), (None, None));
    assert_eq!(rules.as_deref(), Some("be kind"), "old data is untouched");
    assert_eq!(run.message, "hello");
    assert_eq!(run.run.worktree_state, WorktreeState::None);
    assert_eq!(run.run.worktree_path, None);
    assert_eq!(run.run.branch, None);
    assert!(!run.run.has_changes);
    assert_eq!((run.run.changed_files, run.run.ahead), (0, 0));

    let bad = store
        .write(|tx| {
            tx.execute("UPDATE run SET worktree_state = 'gone' WHERE id = 7", [])?;
            Ok(())
        })
        .await;
    assert!(bad.is_err(), "the state is constrained");
    store.close().await;

    let old = Connection::open(&backup).unwrap();
    let columns: i64 = old
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('run') WHERE name = 'worktree_state'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(columns, 0, "the backup is the version-14 database");
}

#[test]
fn run_branches_are_valid_ref_names_of_bounded_length() {
    assert_eq!(branch_name("TRK", 15, 42), "aurora/TRK-15-run42");
    assert_eq!(branch_name("A_B-c", 1, 2), "aurora/A_B-c-1-run2");
    // Anything outside letters, digits, `-` and `_` is replaced.
    assert_eq!(branch_name("a b/c..d~e", 3, 4), "aurora/a-b-c--d-e-3-run4");
    assert_eq!(branch_name("\u{e9}x:y", 3, 4), "aurora/-x-y-3-run4");
    assert_eq!(branch_name("", 3, 4), "aurora/x-3-run4");
    let long = branch_name(&"K".repeat(300), 123_456, 7_654_321);
    assert_eq!(long.len(), 100);
    assert!(long.ends_with("-123456-run7654321"), "{long}");
    for name in [
        branch_name("A.B", 1, 1),
        branch_name("x@{y", 1, 1),
        branch_name("a\\b", 1, 1),
        long,
    ] {
        assert!(
            !name.contains(['.', '@', '\\', ' ', '~', '^', ':', '?', '*', '[']),
            "{name}"
        );
        assert!(
            name.starts_with("aurora/") && !name.ends_with('/'),
            "{name}"
        );
        assert!(name.len() <= 100);
    }
}

#[test]
fn a_runs_folder_is_a_function_of_its_id_under_the_data_folder() {
    let root = abs("data");
    let path = worktree_dir(&root, 12);
    assert!(path.starts_with(&root), "{path}");
    assert!(path.ends_with("12"), "{path}");
    assert!(
        std::path::Path::new(&path).ends_with(std::path::Path::new("worktrees").join("12")),
        "{path}"
    );
}

async fn world() -> (tempfile::TempDir, Store, i64, i64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let repo = abs("repo");
    let ids = store
        .write(move |tx| {
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
            let plain = agents::create_agent(
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
            let fixed = agents::create_agent(
                tx,
                3,
                &actor,
                NewAgent {
                    name: Some("fixed".into()),
                    command: "x".into(),
                    work_dir: Some(abs("fixed")),
                    projects: vec!["TRK".into()],
                    ..NewAgent::default()
                },
            )?;
            crate::projects::update_project(
                tx,
                4,
                &actor,
                "TRK",
                ProjectPatch {
                    repo_path: Some(Some(repo)),
                    base_branch: Some(Some("main".into())),
                    ..ProjectPatch::default()
                },
            )?;
            Ok((plain.id, fixed.id))
        })
        .await
        .unwrap();
    (dir, store, ids.0, ids.1)
}

fn new_run(agent: i64, number: i64, root: Option<&str>) -> NewRun {
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
        worktree_root: root.map(str::to_string),
        base_branch: None,
        comment_id: None,
        limits: None,
    }
}

async fn events_of(store: &Store, kind: &'static str) -> Vec<Value> {
    store
        .read(move |c| {
            let all = events::read_events(
                c,
                0,
                Some(500),
                &EventFilter {
                    kind_prefix: Some(kind.to_string()),
                    ..EventFilter::default()
                },
            )?;
            Ok(all.into_iter().map(|e| e.payload).collect())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn repository_settings_are_the_owners_checked_cleared_by_blank_and_never_logged() {
    let (_dir, store, _, _) = world().await;
    let repo = abs("repo");
    let project = store
        .read(|c| projects::get_project(c, "TRK"))
        .await
        .unwrap();
    assert_eq!(project.repo_path.as_deref(), Some(repo.as_str()));
    assert_eq!(project.base_branch.as_deref(), Some("main"));
    let plain = serde_json::to_value(&project).unwrap();
    assert!(plain.get("repo_path").is_none(), "{plain}");
    assert!(plain.get("base_branch").is_none(), "{plain}");
    let people = project.for_people();
    assert_eq!(people["repo_path"], repo);
    assert_eq!(people["base_branch"], "main");

    let patch = |p: ProjectPatch, who: Principal| {
        store.write(move |tx| crate::projects::update_project(tx, 50, &who, "TRK", p))
    };
    let member = Principal {
        user_id: crate::tests_support::TEST_USER_ID,
        role: Role::Member,
        scope: Scope::Write,
    };
    let agent = Principal {
        role: Role::Agent,
        ..member
    };
    for who in [member, agent] {
        let err = patch(
            ProjectPatch {
                base_branch: Some(Some("dev".into())),
                ..ProjectPatch::default()
            },
            who,
        )
        .await
        .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
    }
    // Renaming stays what it was for everyone who may write.
    patch(
        ProjectPatch {
            name: Some("Renamed".into()),
            ..ProjectPatch::default()
        },
        member,
    )
    .await
    .unwrap();

    for bad in [
        "relative/path",
        "",
        &format!("{}\0", abs("x")),
        &"x".repeat(1001),
    ] {
        let result = patch(
            ProjectPatch {
                repo_path: Some(Some(bad.to_string())),
                ..ProjectPatch::default()
            },
            owner(),
        )
        .await;
        if bad.is_empty() {
            // Blank clears.
            assert_eq!(result.unwrap().repo_path, None);
        } else {
            assert!(
                matches!(rejection(result.unwrap_err()), Rejection::Invalid(_)),
                "{bad:?}"
            );
        }
    }
    let err = patch(ProjectPatch::default(), owner()).await.unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));

    // Set again, change nothing, clear the branch.
    let again = patch(
        ProjectPatch {
            repo_path: Some(Some(repo.clone())),
            base_branch: Some(Some("  ".into())),
            ..ProjectPatch::default()
        },
        owner(),
    )
    .await
    .unwrap();
    assert_eq!(again.repo_path.as_deref(), Some(repo.as_str()));
    assert_eq!(again.base_branch, None);
    let before = events_of(&store, "project.updated").await.len();
    patch(
        ProjectPatch {
            repo_path: Some(Some(repo.clone())),
            ..ProjectPatch::default()
        },
        owner(),
    )
    .await
    .unwrap();
    assert_eq!(events_of(&store, "project.updated").await.len(), before);

    let logged = events_of(&store, "project.updated").await;
    let text = serde_json::to_string(&logged).unwrap();
    assert!(
        !text.contains("aurora-tests"),
        "the path is never logged: {text}"
    );
    assert!(
        logged
            .iter()
            .any(|p| p["repo_path"] == json!({"changed": true, "set": true})),
        "{text}"
    );
    assert!(
        logged
            .iter()
            .any(|p| p["repo_path"] == json!({"changed": true, "set": false})),
        "{text}"
    );
    assert!(
        logged
            .iter()
            .any(|p| p["base_branch"] == json!({"changed": true, "set": false})),
        "{text}"
    );
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn a_run_plans_its_working_copy_only_when_it_will_get_one() {
    let (_dir, store, plain, fixed) = world().await;
    let root = abs("data");

    let made = store
        .write({
            let root = root.clone();
            move |tx| create_run(tx, 10, None, new_run(plain, 1, Some(&root)))
        })
        .await
        .unwrap();
    let run = made.run;
    assert_eq!(run.worktree_state, WorktreeState::Created);
    assert_eq!(run.worktree_path, Some(worktree_dir(&root, run.id)));
    assert_eq!(run.branch, Some(format!("aurora/TRK-1-run{}", run.id)));
    assert_eq!(
        run.base_ref.as_deref(),
        Some("main"),
        "the project's branch"
    );
    assert_eq!(run.base_commit, None, "resolved by the runner");
    let target = store
        .read(move |c| runs::worktree_target(c, run.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.repo, abs("repo"));
    assert_eq!(target.state, WorktreeState::Created);

    // The override wins over the project's branch.
    let mut other = new_run(plain, 2, Some(&root));
    other.base_branch = Some("release".into());
    let other = store
        .write(move |tx| create_run(tx, 11, None, other))
        .await
        .unwrap()
        .run;
    assert_eq!(other.base_ref.as_deref(), Some("release"));

    // An explicit work_dir gets no working copy; neither does a missing root.
    let fixed_run = store
        .write({
            let root = root.clone();
            move |tx| create_run(tx, 12, None, new_run(fixed, 3, Some(&root)))
        })
        .await
        .unwrap()
        .run;
    assert_eq!(fixed_run.worktree_state, WorktreeState::None);
    assert_eq!(fixed_run.worktree_path, None);
    assert_eq!(fixed_run.branch, None);
    let no_root = store
        .write(move |tx| create_run(tx, 13, None, new_run(plain, 4, None)))
        .await
        .unwrap()
        .run;
    assert_eq!(no_root.worktree_state, WorktreeState::None);

    // A project without a repository: none either. Issue 4 has no run yet.
    store
        .write(move |tx| {
            let actor = test_actor(tx);
            crate::projects::update_project(
                tx,
                14,
                &actor,
                "TRK",
                ProjectPatch {
                    repo_path: Some(None),
                    ..ProjectPatch::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let no_repo = store
        .write({
            let root = root.clone();
            move |tx| create_run(tx, 15, None, new_run(plain, 4, Some(&root)))
        })
        .await;
    // Issue 4 got a run above (no root); it is still active.
    assert!(matches!(
        rejection(no_repo.unwrap_err()),
        Rejection::Conflict(_)
    ));

    let created = events_of(&store, "run.created").await;
    assert_eq!(created.len(), 4);
    assert_eq!(created[0]["branch"], format!("aurora/TRK-1-run{}", run.id));
    assert_eq!(created[0]["worktree_state"], "created");
    assert_eq!(created[2]["branch"], Value::Null);
    assert_eq!(created[2]["worktree_state"], "none");
    let text = serde_json::to_string(&created).unwrap();
    assert!(
        !text.contains("aurora-tests"),
        "no path in an event: {text}"
    );
}

async fn planned(store: &Store, agent: i64, number: i64, at: i64) -> i64 {
    let root = abs("data");
    store
        .write(move |tx| create_run(tx, at, None, new_run(agent, number, Some(&root))))
        .await
        .unwrap()
        .run
        .id
}

#[tokio::test]
async fn the_end_of_a_run_settles_its_working_copy_in_the_same_transaction() {
    let (_dir, store, plain, _) = world().await;
    let a = planned(&store, plain, 1, 10).await;
    let b = planned(&store, plain, 2, 11).await;
    let c = planned(&store, plain, 3, 12).await;
    let d = planned(&store, plain, 4, 13).await;
    for id in [a, b, c, d] {
        store
            .write(move |tx| mark_running(tx, 14, id, 5))
            .await
            .unwrap();
    }
    store
        .write(move |tx| runs::set_worktree_base(tx, a, "main", &"a".repeat(40)))
        .await
        .unwrap();
    let ended = store
        .write(move |tx| {
            let mut removed = Outcome::new(RunStatus::Succeeded);
            removed.worktree = Some(WorktreeEnd::removed());
            finish_run(tx, 20, None, a, removed)?;
            let mut kept = Outcome::new(RunStatus::Succeeded);
            kept.worktree = Some(WorktreeEnd::kept(true, 3, 2, None));
            finish_run(tx, 21, None, b, kept)?;
            // Nobody looked: kept, with a note, never "removed".
            finish_run(tx, 22, None, c, Outcome::new(RunStatus::Failed))?;
            let mut none = Outcome::new(RunStatus::Failed);
            none.worktree = Some(WorktreeEnd::none("not created"));
            finish_run(tx, 23, None, d, none)?;
            Ok((
                get_run(tx, a)?,
                get_run(tx, b)?,
                get_run(tx, c)?,
                get_run(tx, d)?,
            ))
        })
        .await
        .unwrap();
    let (ra, rb, rc, rd) = ended;
    let (a, b, c, d) = (ra, rb, rc, rd);
    let b_id = b.run.id;
    assert_eq!(a.run.worktree_state, WorktreeState::Removed);
    assert_eq!(a.run.base_commit, Some("a".repeat(40)));
    assert!(!a.run.has_changes);
    assert_eq!(b.run.worktree_state, WorktreeState::Kept);
    assert!(b.run.has_changes);
    assert_eq!((b.run.changed_files, b.run.ahead), (3, 2));
    assert_eq!(c.run.worktree_state, WorktreeState::Kept);
    assert!(!c.run.has_changes);
    assert!(
        c.run
            .worktree_note
            .as_deref()
            .unwrap()
            .contains("not inspected")
    );
    assert_eq!(d.run.worktree_state, WorktreeState::None);
    assert_eq!(d.run.worktree_note.as_deref(), Some("not created"));
    // The branch stays on a run that never got its working copy.
    assert!(d.run.branch.is_some());

    let finished = events_of(&store, "run.finished").await;
    assert_eq!(finished[0]["worktree_state"], "removed");
    assert_eq!(finished[0]["has_changes"], false);
    assert_eq!(finished[1]["worktree_state"], "kept");
    assert_eq!(finished[1]["has_changes"], true);
    let text = serde_json::to_string(&finished).unwrap();
    assert!(!text.contains("aurora-tests"), "{text}");

    // A late settle changes nothing that was settled.
    let late = store
        .write(move |tx| runs::record_worktree(tx, b_id, &WorktreeEnd::removed()))
        .await
        .unwrap();
    assert!(!late);
    let again = store.read(move |c| get_run(c, b_id)).await.unwrap();
    assert_eq!(again.run.worktree_state, WorktreeState::Kept);

    let changed = store
        .read(|c| {
            runs::list_runs(
                c,
                &RunFilter {
                    has_changes: Some(true),
                    ..RunFilter::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].id, b_id);
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
    let worktrees = report.get("runs.worktrees").unwrap();
    assert!(
        worktrees.details.contains("3 working copies"),
        "{}",
        worktrees.details
    );
}

#[tokio::test]
async fn a_cancel_leaves_the_working_copy_to_be_settled_after_the_process_stops() {
    let (_dir, store, plain, _) = world().await;
    let id = planned(&store, plain, 1, 10).await;
    store
        .write(move |tx| mark_running(tx, 11, id, 5))
        .await
        .unwrap();
    let canceled = store
        .write(move |tx| {
            finish_run(
                tx,
                12,
                Some(crate::tests_support::TEST_USER_ID),
                id,
                Outcome::new(RunStatus::Canceled),
            )?;
            get_run(tx, id)
        })
        .await
        .unwrap();
    assert_eq!(canceled.run.worktree_state, WorktreeState::Created);
    let report = store.read(|c| Ok(invariants::check(c))).await.unwrap();
    assert!(
        report.ok(),
        "this moment is allowed: {:#?}",
        report.failed()
    );
    let settled = store
        .write(move |tx| runs::record_worktree(tx, id, &WorktreeEnd::kept(true, 1, 0, None)))
        .await
        .unwrap();
    assert!(settled);
    let after = store.read(move |c| get_run(c, id)).await.unwrap();
    assert_eq!(after.run.worktree_state, WorktreeState::Kept);
    // No event for the late settle: the row is the authority.
    assert_eq!(events_of(&store, "run.finished").await.len(), 1);
}

#[tokio::test]
async fn reconciliation_keeps_every_unsettled_working_copy_without_touching_the_disk() {
    let (_dir, store, plain, _) = world().await;
    let active = planned(&store, plain, 1, 10).await;
    let canceled = planned(&store, plain, 2, 11).await;
    store
        .write(move |tx| {
            mark_running(tx, 12, canceled, 5)?;
            finish_run(
                tx,
                13,
                Some(crate::tests_support::TEST_USER_ID),
                canceled,
                Outcome::new(RunStatus::Canceled),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut mine = new_run(plain, 3, Some(&abs("data")));
    mine.boot_id = "boot-2".into();
    let mine = store
        .write(move |tx| create_run(tx, 14, None, mine))
        .await
        .unwrap()
        .run
        .id;

    let done = store
        .write(|tx| runs::reconcile_startup(tx, 20, "boot-2"))
        .await
        .unwrap();
    assert_eq!(done, [active]);
    for id in [active, canceled] {
        let run = store.read(move |c| get_run(c, id)).await.unwrap().run;
        assert_eq!(run.worktree_state, WorktreeState::Kept, "run {id}");
        assert!(!run.has_changes);
        assert!(
            run.worktree_note.as_deref().unwrap().contains("inspected"),
            "{:?}",
            run.worktree_note
        );
        assert!(
            !std::path::Path::new(run.worktree_path.as_deref().unwrap()).exists(),
            "nothing was created and nothing is touched"
        );
    }
    let live = store.read(move |c| get_run(c, mine)).await.unwrap().run;
    assert_eq!(
        live.worktree_state,
        WorktreeState::Created,
        "another boot's business"
    );
    let report = store
        .read(|c| Ok(invariants::check_with_boot(c, "boot-2")))
        .await
        .unwrap();
    assert!(report.ok(), "{:#?}", report.failed());

    let swept = store
        .write(|tx| runs::interrupt_boot(tx, 30, "boot-2"))
        .await
        .unwrap();
    assert_eq!(swept, [mine]);
    let live = store.read(move |c| get_run(c, mine)).await.unwrap().run;
    assert_eq!(live.worktree_state, WorktreeState::Kept);
}

async fn corrupted(sql: &'static str) -> invariants::Report {
    let (dir, store, plain, _) = world().await;
    let a = planned(&store, plain, 1, 10).await;
    let b = planned(&store, plain, 2, 11).await;
    store
        .write(move |tx| {
            mark_running(tx, 12, a, 5)?;
            let mut kept = Outcome::new(RunStatus::Succeeded);
            kept.worktree = Some(WorktreeEnd::kept(true, 2, 1, None));
            finish_run(tx, 13, None, a, kept)?;
            Ok(b)
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
async fn corruption_of_working_copies_is_noticed() {
    // Run 1 finished kept with changes, run 2 is starting and `created`.
    let clean = corrupted("SELECT 1;").await;
    assert!(clean.ok(), "{:#?}", clean.failed());
    let note = clean.get("runs.worktrees").unwrap();
    assert!(
        note.details.contains("note:") && note.details.contains("is kept but"),
        "a kept folder that is missing is a note, not a failure: {}",
        note.details
    );

    let details = failed(
        &corrupted("UPDATE run SET status = 'failed', finished = 9, error = 'x' WHERE id = 2;")
            .await,
        "runs.worktrees",
    );
    assert!(
        details.contains("run 2") && details.contains("never settled"),
        "{details}"
    );
    let details = failed(
        &corrupted("UPDATE run SET worktree_path = NULL WHERE id = 1;").await,
        "runs.worktrees",
    );
    assert!(details.contains("without a folder or branch"), "{details}");
    let details = failed(
        &corrupted("UPDATE run SET branch = NULL WHERE id = 1;").await,
        "runs.worktrees",
    );
    assert!(details.contains("run 1"), "{details}");
    let details = failed(
        &corrupted("UPDATE run SET worktree_state = 'removed' WHERE id = 1;").await,
        "runs.worktrees",
    );
    assert!(
        details.contains("removed although it had changes"),
        "{details}"
    );
    let details = failed(
        &corrupted("UPDATE run SET worktree_state = 'kept' WHERE id = 2;").await,
        "runs.worktrees",
    );
    assert!(details.contains("already kept"), "{details}");
    let details = failed(
        &corrupted("UPDATE run SET worktree_state = 'none' WHERE id = 1;").await,
        "runs.worktrees",
    );
    assert!(details.contains("reports changes"), "{details}");

    // A removed working copy whose folder is still there is only a note.
    let dir = tempfile::tempdir().unwrap();
    let still = dir.path().display().to_string().replace('\'', "''");
    let report = corrupted(Box::leak(
        format!(
            "UPDATE run SET worktree_state = 'removed', has_changes = 0, changed_files = 0, \
             ahead = 0, worktree_path = '{still}' WHERE id = 1;"
        )
        .into_boxed_str(),
    ))
    .await;
    assert!(report.ok(), "{:#?}", report.failed());
    assert!(
        report
            .get("runs.worktrees")
            .unwrap()
            .details
            .contains("is removed but"),
        "{}",
        report.get("runs.worktrees").unwrap().details
    );
}

#[tokio::test]
async fn replay_agrees_on_branches_and_repository_flags_and_notices_a_difference() {
    let clean = corrupted("SELECT 1;").await;
    let agreement = clean.get("events.state_agreement").unwrap();
    assert!(agreement.ok, "{}", agreement.details);

    let details = failed(
        &corrupted("UPDATE run SET branch = 'aurora/other' WHERE id = 1;").await,
        "events.state_agreement",
    );
    assert!(details.contains("run"), "{details}");
    let details = failed(
        &corrupted("UPDATE project SET repo_path = NULL;").await,
        "events.state_agreement",
    );
    assert!(details.contains("repository settings"), "{details}");
    let details = failed(
        &corrupted("UPDATE project SET base_branch = NULL;").await,
        "events.state_agreement",
    );
    assert!(details.contains("repository settings"), "{details}");
}
