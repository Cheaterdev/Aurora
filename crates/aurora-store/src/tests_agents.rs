//! The agent registry (TRK-14): CRUD, validation, permissions, events, the
//! dispatch helper and the project limits of search and the event log.

use serde_json::json;

use crate::agents::{self, Agent, AgentPatch, NewAgent, Trigger, TriggerReason};
use crate::auth::{self, Principal, Role, Scope};
use crate::comments::{self, NewComment};
use crate::events::{self, Event, EventFilter};
use crate::issues::{IssuePatch, NewIssue};
use crate::projects::Rejection;
use crate::search::{self, SearchQuery};
use crate::tests_support::{issues as test_issues, projects as test_projects};
use crate::{Store, StoreError};

const SECRET: &str = "sk-SENTINEL-do-not-log";

fn owner() -> Principal {
    Principal {
        user_id: 1,
        role: Role::Owner,
        scope: Scope::Write,
    }
}

fn member() -> Principal {
    Principal {
        user_id: 2,
        role: Role::Member,
        scope: Scope::Write,
    }
}

fn agent_principal(user_id: i64) -> Principal {
    Principal {
        user_id,
        role: Role::Agent,
        scope: Scope::Write,
    }
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(r) => r,
        other => panic!("expected a rejection, got {other}"),
    }
}

/// A store with the owner (id 1), a member (id 2), projects AA and BB, an
/// issue type "Task" in each.
async fn world() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .write(|tx| {
            tx.execute_batch(
                "INSERT INTO user (id, name, kind, role) VALUES
                     (1, 'owner', 'human', 'owner'),
                     (2, 'member', 'human', 'member');",
            )?;
            for key in ["AA", "BB"] {
                test_projects::create_project(tx, 1, key, key, None)?;
                test_projects::create_issue_type(tx, 1, key, "Task")?;
            }
            Ok(())
        })
        .await
        .unwrap();
    (dir, store)
}

fn new_agent(name: &str, projects: &[&str]) -> NewAgent {
    NewAgent {
        name: Some(name.to_string()),
        command: "claude".into(),
        projects: projects.iter().map(|p| p.to_string()).collect(),
        ..NewAgent::default()
    }
}

async fn create(store: &Store, new: NewAgent) -> Agent {
    store
        .write(move |tx| agents::create_agent(tx, 100, &owner(), new))
        .await
        .unwrap()
}

async fn update(store: &Store, id: i64, patch: AgentPatch) -> Agent {
    store
        .write(move |tx| agents::update_agent(tx, 200, &owner(), id, patch))
        .await
        .unwrap()
}

async fn all_events(store: &Store) -> Vec<Event> {
    store
        .read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
        .await
        .unwrap()
}

async fn events_of(store: &Store, kind: &'static str) -> Vec<Event> {
    all_events(store)
        .await
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

async fn latest(store: &Store) -> i64 {
    store.read(events::latest_event_id).await.unwrap()
}

#[tokio::test]
async fn creating_by_name_makes_the_user_the_row_and_the_events() {
    let (_dir, store) = world().await;
    let made = create(
        &store,
        NewAgent {
            description: Some("  reviews code  ".into()),
            args: vec!["--print".into(), "-p".into()],
            model: Some("opus".into()),
            instructions: Some("Be brief.".into()),
            work_dir: Some("C:\\work".into()),
            max_concurrent: Some(2),
            timeout_seconds: Some(120),
            on_mention: Some(false),
            ..new_agent("reviewer", &["BB", "AA", "AA"])
        },
    )
    .await;
    assert_eq!(made.name, "reviewer");
    assert_eq!(made.description, "reviews code");
    assert!(made.on_assign && !made.on_mention && made.enabled);
    assert_eq!((made.max_concurrent, made.timeout_seconds), (2, 120));
    assert_eq!(made.projects, ["AA", "BB"], "sorted, without duplicates");
    let private = made.private.clone().unwrap();
    assert_eq!(private.command, "claude");
    assert_eq!(private.args, ["--print", "-p"]);
    assert_eq!(private.model.as_deref(), Some("opus"));
    assert_eq!(private.work_dir.as_deref(), Some("C:\\work"));

    let user_id = made.user_id;
    let user = store
        .read(move |c| auth::get_user(c, user_id))
        .await
        .unwrap();
    assert_eq!((user.name.as_str(), user.role), ("reviewer", Role::Agent));

    let created = events_of(&store, "agent.created").await;
    assert_eq!(created.len(), 1);
    let e = &created[0];
    assert_eq!(
        (e.entity_kind.as_str(), e.entity_id, e.actor, e.project_id),
        ("agent", made.id, Some(1), None)
    );
    assert_eq!(
        e.payload,
        json!({
            "name": "reviewer", "user_id": made.user_id, "on_assign": true,
            "on_mention": false, "enabled": true, "max_concurrent": 2,
            "timeout_seconds": 120, "projects": ["AA", "BB"],
        })
    );
    assert_eq!(events_of(&store, "user.created").await.len(), 1);
    store.close().await;
}

#[tokio::test]
async fn defaults_and_an_empty_project_list_allow_nothing() {
    let (_dir, store) = world().await;
    let made = create(&store, new_agent("plain", &[])).await;
    assert!(made.on_assign && made.on_mention && made.enabled);
    assert_eq!((made.max_concurrent, made.timeout_seconds), (1, 3600));
    assert!(made.projects.is_empty(), "default deny");
    let user_id = made.user_id;
    let access = store
        .read(move |c| agents::agent_access(c, user_id))
        .await
        .unwrap()
        .unwrap();
    assert!(access.enabled && access.projects.is_empty());
    let none = store.read(|c| agents::agent_access(c, 2)).await.unwrap();
    assert!(none.is_none(), "a member is not a registered agent");
    store.close().await;
}

#[tokio::test]
async fn an_existing_agent_user_can_be_registered_once() {
    let (_dir, store) = world().await;
    let user = store
        .write(|tx| auth::create_agent_user(tx, 5, &owner(), "old"))
        .await
        .unwrap();
    let register = |id: i64| NewAgent {
        user_id: Some(id),
        command: "x".into(),
        ..NewAgent::default()
    };
    let new = register(user.id);
    let made = store
        .write(move |tx| agents::create_agent(tx, 6, &owner(), new))
        .await
        .unwrap();
    assert_eq!(made.user_id, user.id);

    let new = register(user.id);
    let again = store
        .write(move |tx| agents::create_agent(tx, 7, &owner(), new))
        .await
        .unwrap_err();
    assert!(matches!(rejection(again), Rejection::Conflict(_)));
    let human = store
        .write(move |tx| agents::create_agent(tx, 7, &owner(), register(2)))
        .await
        .unwrap_err();
    assert!(matches!(rejection(human), Rejection::Invalid(_)));
    let missing = store
        .write(move |tx| agents::create_agent(tx, 7, &owner(), register(99)))
        .await
        .unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    for new in [
        NewAgent {
            user_id: Some(user.id),
            name: Some("both".into()),
            command: "x".into(),
            ..NewAgent::default()
        },
        NewAgent {
            command: "x".into(),
            ..NewAgent::default()
        },
    ] {
        let err = store
            .write(move |tx| agents::create_agent(tx, 8, &owner(), new))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Invalid(_)));
    }
    let duplicate_name = store
        .write(|tx| agents::create_agent(tx, 8, &owner(), new_agent("OLD", &[])))
        .await
        .unwrap_err();
    assert!(matches!(rejection(duplicate_name), Rejection::Conflict(_)));
    store.close().await;
}

#[tokio::test]
async fn a_failed_registration_leaves_neither_user_nor_agent() {
    let (_dir, store) = world().await;
    let before = latest(&store).await;
    let users_before = store.read(auth::list_users).await.unwrap().len();
    let outcome = store
        .write(|tx| {
            agents::create_agent(tx, 5, &owner(), new_agent("ghost", &["AA"]))?;
            Err::<(), _>(StoreError::Panicked)
        })
        .await;
    assert!(outcome.is_err());
    let (users, rows, after) = store
        .read(|c| {
            Ok((
                auth::list_users(c)?.len(),
                c.query_row("SELECT COUNT(*) FROM agent", [], |r| r.get::<_, i64>(0))?,
                events::latest_event_id(c)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((users, rows, after), (users_before, 0, before));

    // An unknown project is refused before the user is created.
    let err = store
        .write(|tx| agents::create_agent(tx, 5, &owner(), new_agent("ghost", &["NOPE"])))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Invalid(_)));
    assert_eq!(
        store.read(auth::list_users).await.unwrap().len(),
        users_before
    );
    store.close().await;
}

async fn refuse(store: &Store, what: &str, mutate: impl FnOnce(&mut NewAgent)) {
    let mut new = new_agent("probe", &[]);
    mutate(&mut new);
    let err = store
        .write(move |tx| agents::create_agent(tx, 5, &owner(), new))
        .await
        .unwrap_err();
    assert!(
        matches!(rejection(err), Rejection::Invalid(_)),
        "{what} must be invalid"
    );
}

#[tokio::test]
async fn invalid_values_are_refused_and_the_limits_are_inclusive() {
    let (_dir, store) = world().await;
    let long = |n: usize| "x".repeat(n);
    refuse(&store, "empty command", |n| n.command = "  ".into()).await;
    refuse(&store, "long command", |n| n.command = "x".repeat(501)).await;
    refuse(&store, "newline", |n| n.command = "a\nb".into()).await;
    refuse(&store, "return", |n| n.command = "a\rb".into()).await;
    refuse(&store, "nul", |n| n.command = "a\0b".into()).await;
    refuse(&store, "51 args", |n| n.args = vec!["a".into(); 51]).await;
    refuse(&store, "long arg", |n| n.args = vec!["x".repeat(2001)]).await;
    refuse(&store, "nul arg", |n| n.args = vec!["a\0".into()]).await;
    refuse(&store, "long model", |n| n.model = Some("m".repeat(201))).await;
    refuse(&store, "relative dir", |n| n.work_dir = Some("work".into())).await;
    refuse(&store, "long dir", |n| {
        n.work_dir = Some(format!("/{}", "d".repeat(1000)));
    })
    .await;
    refuse(&store, "concurrent 0", |n| n.max_concurrent = Some(0)).await;
    refuse(&store, "concurrent 17", |n| n.max_concurrent = Some(17)).await;
    refuse(&store, "timeout 59", |n| n.timeout_seconds = Some(59)).await;
    refuse(&store, "timeout 86401", |n| {
        n.timeout_seconds = Some(86_401)
    })
    .await;
    refuse(&store, "instructions", |n| {
        n.instructions = Some("i".repeat(20_001));
    })
    .await;
    refuse(&store, "description", |n| {
        n.description = Some("d".repeat(1001))
    })
    .await;
    refuse(&store, "project", |n| n.projects = vec!["ZZ".into()]).await;

    let edge = NewAgent {
        command: long(500),
        args: vec![long(2000); 50],
        instructions: Some("i".repeat(20_000)),
        work_dir: Some(format!("/{}", "d".repeat(999))),
        max_concurrent: Some(16),
        timeout_seconds: Some(86_400),
        ..new_agent("edge", &[])
    };
    let made = create(&store, edge).await;
    assert_eq!(made.private.unwrap().args.len(), 50);
    let low = NewAgent {
        max_concurrent: Some(1),
        timeout_seconds: Some(60),
        work_dir: Some("D:/x".into()),
        ..new_agent("low", &[])
    };
    assert_eq!(create(&store, low).await.timeout_seconds, 60);
    let blank = NewAgent {
        model: Some("   ".into()),
        work_dir: Some("".into()),
        ..new_agent("blank", &[])
    };
    let blank = create(&store, blank).await.private.unwrap();
    assert_eq!((blank.model, blank.work_dir), (None, None));
    store.close().await;
}

#[tokio::test]
async fn patch_changes_only_what_is_present_and_null_clears() {
    let (_dir, store) = world().await;
    let made = create(
        &store,
        NewAgent {
            model: Some("opus".into()),
            work_dir: Some("/srv".into()),
            ..new_agent("patched", &["AA"])
        },
    )
    .await;
    let id = made.id;

    let same = update(&store, id, AgentPatch::default()).await;
    assert_eq!(same, made);
    let nothing = update(
        &store,
        id,
        AgentPatch {
            on_assign: Some(true),
            projects: Some(vec!["AA".into()]),
            command: Some("claude".into()),
            ..AgentPatch::default()
        },
    )
    .await;
    assert_eq!(nothing.updated, made.updated, "no change, no write");
    assert!(events_of(&store, "agent.updated").await.is_empty());

    let changed = update(
        &store,
        id,
        AgentPatch {
            on_assign: Some(false),
            max_concurrent: Some(4),
            timeout_seconds: Some(600),
            model: Some(None),
            work_dir: Some(None),
            args: Some(vec!["--x".into()]),
            projects: Some(vec!["BB".into(), "AA".into()]),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(!changed.on_assign && changed.on_mention);
    assert_eq!((changed.max_concurrent, changed.timeout_seconds), (4, 600));
    assert_eq!(changed.projects, ["AA", "BB"]);
    assert_eq!(changed.updated, 200);
    let private = changed.private.unwrap();
    assert_eq!((private.model, private.work_dir), (None, None));
    assert_eq!(private.args, ["--x"]);
    assert_eq!(private.command, "claude", "untouched");

    let shrunk = update(
        &store,
        id,
        AgentPatch {
            projects: Some(vec![]),
            enabled: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(shrunk.projects.is_empty() && !shrunk.enabled);

    let log = events_of(&store, "agent.updated").await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].actor, Some(1));
    assert_eq!(
        log[0].payload,
        json!({"changes": {
            "on_assign": {"from": true, "to": false},
            "max_concurrent": {"from": 1, "to": 4},
            "timeout_seconds": {"from": 3600, "to": 600},
            "model": {"changed": true},
            "work_dir": {"changed": true},
            "args": {"changed": true},
            "projects": {"from": ["AA"], "to": ["AA", "BB"]},
        }})
    );
    assert_eq!(
        log[1].payload,
        json!({"changes": {
            "enabled": {"from": true, "to": false},
            "projects": {"from": ["AA", "BB"], "to": []},
        }})
    );

    let invalid = store
        .write(move |tx| {
            agents::update_agent(
                tx,
                300,
                &owner(),
                id,
                AgentPatch {
                    enabled: Some(true),
                    max_concurrent: Some(99),
                    ..AgentPatch::default()
                },
            )
        })
        .await
        .unwrap_err();
    assert!(matches!(rejection(invalid), Rejection::Invalid(_)));
    let still = store
        .read(move |c| agents::agent_config(c, id))
        .await
        .unwrap();
    assert!(!still.enabled, "a refused patch changes nothing");
    let missing = store
        .write(|tx| agents::update_agent(tx, 1, &owner(), 999, AgentPatch::default()))
        .await
        .unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    store.close().await;
}

#[tokio::test]
async fn enable_and_disable_are_idempotent() {
    let (_dir, store) = world().await;
    let made = create(&store, new_agent("toggler", &["AA"])).await;
    let id = made.id;
    for (enabled, expected_events) in [(true, 0), (false, 1), (false, 1), (true, 2)] {
        let agent = store
            .write(move |tx| agents::set_enabled(tx, 9, &owner(), id, enabled))
            .await
            .unwrap();
        assert_eq!(agent.enabled, enabled);
        assert_eq!(
            events_of(&store, "agent.updated").await.len(),
            expected_events
        );
    }
    store.close().await;
}

#[tokio::test]
async fn nothing_typed_into_the_agent_ever_reaches_the_event_log() {
    let (_dir, store) = world().await;
    let made = create(
        &store,
        NewAgent {
            description: Some(format!("desc {SECRET}")),
            args: vec![format!("--token={SECRET}")],
            model: Some(format!("model-{SECRET}")),
            instructions: Some(format!("use {SECRET}")),
            work_dir: Some(format!("/{SECRET}")),
            ..new_agent(&format!("bot-{}", &SECRET[3..9]), &["AA"])
        },
    )
    .await;
    // Change every sensitive field to another secret-bearing value.
    update(
        &store,
        made.id,
        AgentPatch {
            description: Some(format!("other {SECRET}2")),
            command: Some(format!("run {SECRET}3")),
            args: Some(vec![SECRET.into(), "b".into()]),
            model: Some(Some(format!("m{SECRET}4"))),
            instructions: Some(format!("instr {SECRET}5")),
            work_dir: Some(Some(format!("/w/{SECRET}6"))),
            on_mention: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    let id = made.id;
    store
        .write(move |tx| agents::delete_agent(tx, 400, &owner(), id))
        .await
        .unwrap();

    let dump: String = all_events(&store)
        .await
        .iter()
        .map(|e| format!("{} {}", e.kind, e.payload))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(dump.contains("agent.created") && dump.contains("agent.deleted"));
    assert!(
        !dump.contains("SENTINEL"),
        "an event carries typed-in text:\n{dump}"
    );
    // The stored row does hold it; only the log must not.
    store.close().await;
}

#[tokio::test]
async fn only_the_owner_writes_and_only_the_owner_sees_the_command() {
    let (_dir, store) = world().await;
    let made = create(
        &store,
        NewAgent {
            instructions: Some("secret plan".into()),
            ..new_agent("watched", &["AA"])
        },
    )
    .await;
    let id = made.id;
    let other = create(&store, new_agent("other", &[])).await;

    for actor in [member(), agent_principal(made.user_id)] {
        let denied = [
            store
                .write(move |tx| agents::create_agent(tx, 1, &actor, new_agent("x", &[])))
                .await
                .map(|_| ()),
            store
                .write(move |tx| agents::update_agent(tx, 1, &actor, id, AgentPatch::default()))
                .await
                .map(|_| ()),
            store
                .write(move |tx| agents::set_enabled(tx, 1, &actor, id, false))
                .await
                .map(|_| ()),
            store
                .write(move |tx| agents::delete_agent(tx, 1, &actor, id))
                .await,
        ];
        for outcome in denied {
            assert!(matches!(
                rejection(outcome.unwrap_err()),
                Rejection::Forbidden(_)
            ));
        }

        let one = store
            .read(move |c| agents::get_agent(c, &actor, id))
            .await
            .unwrap();
        assert!(one.private.is_none());
        assert_eq!(one.name, "watched");
        assert_eq!(one.projects, ["AA"]);
        let all = store
            .read(move |c| agents::list_agents(c, &actor))
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|a| a.private.is_none()));
        let json = serde_json::to_string(&all).unwrap();
        assert!(
            !json.contains("claude") && !json.contains("secret plan"),
            "{json}"
        );
    }

    let seen = store
        .read(move |c| agents::get_agent(c, &owner(), id))
        .await
        .unwrap();
    assert_eq!(seen.private.unwrap().instructions, "secret plan");
    let json = serde_json::to_value(&other).unwrap();
    assert_eq!(json["command"], "claude");
    let redacted = store
        .read(move |c| agents::get_agent(c, &member(), other.id))
        .await
        .unwrap();
    assert!(
        serde_json::to_value(&redacted)
            .unwrap()
            .get("command")
            .is_none()
    );

    // Nothing changed through the refused calls.
    assert_eq!(events_of(&store, "agent.updated").await.len(), 0);
    store.close().await;
}

#[tokio::test]
async fn deleting_removes_the_entry_only() {
    let (_dir, store) = world().await;
    let made = create(&store, new_agent("leaving", &["AA", "BB"])).await;
    let (id, user_id) = (made.id, made.user_id);
    let key = store
        .write(move |tx| auth::create_key(tx, 1, &owner(), user_id, "k", Scope::Write))
        .await
        .unwrap();
    store
        .write(move |tx| agents::delete_agent(tx, 50, &owner(), id))
        .await
        .unwrap();

    let (rows, links, user, authenticated) = store
        .read(move |c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM agent", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM agent_project", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                auth::get_user(c, user_id)?,
                auth::authenticate_key(c, 60, &key.key)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((rows, links), (0, 0));
    assert_eq!(user.role, Role::Agent);
    assert!(authenticated.is_some(), "the key keeps working");
    let deleted = events_of(&store, "agent.deleted").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        deleted[0].payload,
        json!({"name": "leaving", "user_id": user_id})
    );
    let again = store
        .write(move |tx| agents::delete_agent(tx, 51, &owner(), id))
        .await
        .unwrap_err();
    assert!(matches!(rejection(again), Rejection::NotFound(_)));
    store.close().await;
}

// ---- the dispatch helper ----

struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    a: Agent,
    b: Agent,
    loose: i64,
}

/// Agent `agenta` (AA only), agent `agentb` (AA and BB) and an unregistered
/// agent user `loose`.
async fn fixture() -> Fixture {
    let (dir, store) = world().await;
    let a = create(&store, new_agent("agenta", &["AA"])).await;
    let b = create(&store, new_agent("agentb", &["AA", "BB"])).await;
    let loose = store
        .write(|tx| auth::create_agent_user(tx, 5, &owner(), "loose"))
        .await
        .unwrap()
        .id;
    Fixture {
        store,
        _dir: dir,
        a,
        b,
        loose,
    }
}

impl Fixture {
    async fn issue(&self, project: &'static str, assignee: Option<i64>) -> Event {
        self.store
            .write(move |tx| {
                let type_id = tx.query_row(
                    "SELECT t.id FROM issue_type t JOIN project p ON p.id = t.project_id WHERE p.key = ?1",
                    [project],
                    |r| r.get(0),
                )?;
                test_issues::create_issue(
                    tx,
                    10,
                    project,
                    NewIssue {
                        title: "work".into(),
                        type_id: Some(type_id),
                        assignee_id: assignee,
                        ..NewIssue::default()
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        self.newest().await
    }

    async fn assign(&self, project: &'static str, number: i64, to: Option<i64>) -> Event {
        self.store
            .write(move |tx| {
                test_issues::update_issue(
                    tx,
                    11,
                    project,
                    number,
                    IssuePatch {
                        assignee_id: Some(to),
                        ..IssuePatch::default()
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        self.newest().await
    }

    /// The mention event of the newest comment, written by `author`.
    async fn mention(
        &self,
        author: Principal,
        project: &'static str,
        number: i64,
        body: &str,
    ) -> Event {
        let body = body.to_string();
        self.store
            .write(move |tx| {
                comments::add_comment(
                    tx,
                    12,
                    &author,
                    project,
                    number,
                    NewComment { body, parent: None },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        self.store
            .read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
            .await
            .unwrap()
            .into_iter()
            .rfind(|e| e.kind == "mention.created")
            .unwrap()
    }

    async fn newest(&self) -> Event {
        self.store
            .read(|c| events::read_events(c, 0, Some(500), &EventFilter::default()))
            .await
            .unwrap()
            .pop()
            .unwrap()
    }

    async fn triggers(&self, event: &Event) -> Vec<Trigger> {
        let event = event.clone();
        self.store
            .read(move |c| agents::triggers_for_event(c, &event))
            .await
            .unwrap()
    }

    async fn patch(&self, id: i64, patch: AgentPatch) {
        update(&self.store, id, patch).await;
    }
}

fn summary(triggers: &[Trigger]) -> Vec<(i64, TriggerReason, &str, &str)> {
    triggers
        .iter()
        .map(|t| (t.user_id, t.reason, t.project.as_str(), t.issue.as_str()))
        .collect()
}

#[tokio::test]
async fn assignment_triggers_follow_the_registry() {
    let f = fixture().await;
    let (a, b) = (f.a.user_id, f.b.user_id);

    // issue.created with an assignee.
    let created = f.issue("AA", Some(a)).await;
    assert_eq!(created.kind, "issue.created");
    let found = f.triggers(&created).await;
    assert_eq!(
        summary(&found),
        [(a, TriggerReason::Assigned, "AA", "AA-1")]
    );
    assert_eq!(found[0].agent_id, f.a.id);
    assert_eq!(found[0].event_id, created.id);
    assert_eq!(found[0].actor, Some(4242));
    assert_eq!(found[0].comment_id, None);
    assert_eq!(
        f.triggers(&created).await,
        found,
        "pure: repeating gives the same"
    );

    // No assignee, a human assignee and an unregistered agent wake nobody.
    let nobody = f.issue("AA", None).await;
    assert!(f.triggers(&nobody).await.is_empty());
    let human = f.issue("AA", Some(2)).await;
    assert!(f.triggers(&human).await.is_empty());
    let loose = f.issue("AA", Some(f.loose)).await;
    assert!(f.triggers(&loose).await.is_empty());

    // A project the agent may not use.
    let outside = f.issue("BB", Some(a)).await;
    assert!(f.triggers(&outside).await.is_empty());
    let allowed = f.issue("BB", Some(b)).await;
    assert_eq!(
        summary(&f.triggers(&allowed).await),
        [(b, TriggerReason::Assigned, "BB", "BB-2")]
    );

    // issue.updated: A to B wakes only B; clearing wakes nobody; B to A wakes only A.
    let to_b = f.assign("AA", 1, Some(b)).await;
    assert_eq!(to_b.kind, "issue.updated");
    assert_eq!(
        summary(&f.triggers(&to_b).await),
        [(b, TriggerReason::Assigned, "AA", "AA-1")]
    );
    let cleared = f.assign("AA", 1, None).await;
    assert!(f.triggers(&cleared).await.is_empty());
    let to_a = f.assign("AA", 1, Some(a)).await;
    assert_eq!(
        summary(&f.triggers(&to_a).await),
        [(a, TriggerReason::Assigned, "AA", "AA-1")]
    );

    // An update that does not touch the assignee wakes nobody.
    let retitled = f
        .store
        .write(|tx| {
            test_issues::update_issue(
                tx,
                12,
                "AA",
                1,
                IssuePatch {
                    title: Some("renamed".into()),
                    ..IssuePatch::default()
                },
            )?;
            Ok(())
        })
        .await;
    assert!(retitled.is_ok());
    let renamed = f.newest().await;
    assert!(f.triggers(&renamed).await.is_empty());

    // The switches.
    f.patch(
        f.a.id,
        AgentPatch {
            on_assign: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(f.triggers(&to_a).await.is_empty(), "on_assign off");
    f.patch(
        f.a.id,
        AgentPatch {
            on_assign: Some(true),
            on_mention: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert_eq!(
        f.triggers(&to_a).await.len(),
        1,
        "on_mention does not matter"
    );
    f.patch(
        f.a.id,
        AgentPatch {
            enabled: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(f.triggers(&to_a).await.is_empty(), "disabled");
    f.patch(
        f.a.id,
        AgentPatch {
            enabled: Some(true),
            projects: Some(vec![]),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(
        f.triggers(&to_a).await.is_empty(),
        "no allowed project means none"
    );
    f.patch(
        f.a.id,
        AgentPatch {
            projects: Some(vec!["BB".into()]),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(f.triggers(&to_a).await.is_empty(), "another project only");
    f.patch(
        f.a.id,
        AgentPatch {
            projects: Some(vec!["AA".into()]),
            ..AgentPatch::default()
        },
    )
    .await;
    assert_eq!(f.triggers(&to_a).await.len(), 1);

    // A disabled user is skipped too.
    f.store
        .write(move |tx| Ok(tx.execute("UPDATE user SET disabled = 1 WHERE id = ?1", [a])?))
        .await
        .unwrap();
    assert!(f.triggers(&to_a).await.is_empty());
    f.store.close().await;
}

#[tokio::test]
async fn mention_triggers_follow_the_registry() {
    let f = fixture().await;
    let (a, b) = (f.a.user_id, f.b.user_id);
    f.issue("AA", None).await;
    f.issue("BB", None).await;

    let hello = f.mention(owner(), "AA", 1, "hello @agenta").await;
    let found = f.triggers(&hello).await;
    assert_eq!(
        summary(&found),
        [(a, TriggerReason::Mentioned, "AA", "AA-1")]
    );
    assert_eq!(found[0].comment_id, hello.payload["comment_id"].as_i64());

    // Both agents in one comment: one event each, each wakes its own agent.
    let both = f
        .store
        .write(|tx| {
            comments::add_comment(
                tx,
                13,
                &owner(),
                "AA",
                1,
                NewComment {
                    body: "@agenta and @agentb".into(),
                    parent: None,
                },
            )?;
            Ok(())
        })
        .await;
    assert!(both.is_ok());
    let events = all_mentions(&f.store).await;
    let woken: Vec<i64> = {
        let mut ids = Vec::new();
        for e in &events {
            ids.extend(f.triggers(e).await.iter().map(|t| t.user_id));
        }
        ids
    };
    assert_eq!(woken, [a, a, b], "hello, then a and b");
    let ids: std::collections::BTreeSet<i64> = events.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), 3, "each mention is its own event");

    // A human, an unregistered agent and a self-mention wake nobody.
    let human = f.mention(owner(), "AA", 1, "hi @member").await;
    assert!(f.triggers(&human).await.is_empty());
    let loose = f.mention(owner(), "AA", 1, "hi @loose").await;
    assert!(f.triggers(&loose).await.is_empty());
    let own = f
        .mention(agent_principal(a), "AA", 1, "note to self @agenta")
        .await;
    assert_eq!(own.payload["user_kind"], "agent");
    assert!(f.triggers(&own).await.is_empty(), "self mention");
    // Another agent mentioning it does wake it.
    let peer = f.mention(agent_principal(b), "AA", 1, "ping @agenta").await;
    assert_eq!(f.triggers(&peer).await.len(), 1);

    // Project rules: agenta may not be woken in BB, agentb may.
    let outside = f.mention(owner(), "BB", 1, "@agenta").await;
    assert!(f.triggers(&outside).await.is_empty());
    let inside = f.mention(owner(), "BB", 1, "@agentb").await;
    assert_eq!(
        summary(&f.triggers(&inside).await),
        [(b, TriggerReason::Mentioned, "BB", "BB-1")]
    );

    // The switches.
    f.patch(
        f.a.id,
        AgentPatch {
            on_mention: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(f.triggers(&hello).await.is_empty(), "on_mention off");
    f.patch(
        f.a.id,
        AgentPatch {
            on_mention: Some(true),
            on_assign: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert_eq!(
        f.triggers(&hello).await.len(),
        1,
        "on_assign does not matter"
    );
    f.patch(
        f.a.id,
        AgentPatch {
            enabled: Some(false),
            ..AgentPatch::default()
        },
    )
    .await;
    assert!(f.triggers(&hello).await.is_empty());
    f.store.close().await;
}

async fn all_mentions(store: &Store) -> Vec<Event> {
    events_of(store, "mention.created").await
}

#[tokio::test]
async fn other_events_and_events_without_a_project_wake_nobody() {
    let f = fixture().await;
    f.issue("AA", Some(f.a.user_id)).await;
    for event in all_events(&f.store).await {
        if event.kind == "issue.created" {
            continue;
        }
        assert!(
            f.triggers(&event).await.is_empty(),
            "{} must not trigger",
            event.kind
        );
    }
    let mut orphan = f.newest().await;
    orphan.kind = "issue.created".into();
    orphan.project_id = None;
    assert!(f.triggers(&orphan).await.is_empty());
    f.store.close().await;
}

// ---- project limits of search and of the event log ----

#[tokio::test]
async fn search_and_the_event_log_honour_an_allow_list() {
    let f = fixture().await;
    for project in ["AA", "BB"] {
        f.store
            .write(move |tx| {
                let type_id = tx.query_row(
                    "SELECT t.id FROM issue_type t JOIN project p ON p.id = t.project_id WHERE p.key = ?1",
                    [project],
                    |r| r.get(0),
                )?;
                test_issues::create_issue(
                    tx,
                    10,
                    project,
                    NewIssue {
                        title: format!("zebra in {project}"),
                        type_id: Some(type_id),
                        ..NewIssue::default()
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let find = |q: &'static str, project: Option<&'static str>, allowed: Option<Vec<String>>| {
        let store = f.store.clone();
        async move {
            store
                .read(move |c| {
                    search::search_in(
                        c,
                        &SearchQuery {
                            q: q.into(),
                            project: project.map(String::from),
                            ..SearchQuery::default()
                        },
                        allowed.as_deref(),
                    )
                })
                .await
        }
    };
    let keys = |hits: &search::SearchResults| -> Vec<String> {
        hits.hits.iter().map(|h| h.project.clone()).collect()
    };
    let only_aa = Some(vec!["AA".to_string()]);

    assert_eq!(keys(&find("zebra", None, None).await.unwrap()).len(), 2);
    assert_eq!(
        keys(&find("zebra", None, only_aa.clone()).await.unwrap()),
        ["AA"]
    );
    assert_eq!(
        keys(&find("zebra", Some("AA"), only_aa.clone()).await.unwrap()),
        ["AA"]
    );
    assert!(
        find("zebra", None, Some(vec![]))
            .await
            .unwrap()
            .hits
            .is_empty()
    );
    // The identifier lookup is limited too.
    let exact = find("BB-1", None, only_aa.clone()).await.unwrap();
    assert!(exact.hits.is_empty(), "{exact:?}");
    assert_eq!(
        find("AA-1", None, only_aa.clone())
            .await
            .unwrap()
            .hits
            .len(),
        1
    );
    for project in ["BB", "NOPE"] {
        let err = find("zebra", Some(project), only_aa.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(rejection(err), Rejection::Forbidden(_)),
            "{project}"
        );
    }

    let read = |allowed: Option<Vec<String>>, project: Option<&'static str>| {
        let store = f.store.clone();
        async move {
            store
                .read(move |c| {
                    events::read_events_in(
                        c,
                        0,
                        Some(500),
                        &EventFilter {
                            project: project.map(String::from),
                            ..EventFilter::default()
                        },
                        allowed.as_deref(),
                    )
                })
                .await
        }
    };
    let everything = read(None, None).await.unwrap();
    assert!(everything.iter().any(|e| e.project_id.is_none()));
    let limited = read(only_aa.clone(), None).await.unwrap();
    assert!(!limited.is_empty());
    assert!(limited.iter().all(|e| e.project.as_deref() == Some("AA")));
    assert!(read(Some(vec![]), None).await.unwrap().is_empty());
    assert_eq!(
        read(only_aa.clone(), Some("AA")).await.unwrap().len(),
        limited.len()
    );
    let err = read(only_aa, Some("BB")).await.unwrap_err();
    assert!(matches!(rejection(err), Rejection::Forbidden(_)));
    f.store.close().await;
}
