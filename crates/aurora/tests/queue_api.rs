//! Limits, the run queue and the scheduler (TRK-18), over HTTP, with the
//! `fake_agent` example. Agents that must stay busy sleep for a long time and
//! are canceled to release their slot; every test that leaves a process behind
//! ends with a shutdown, which kills it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aurora::runner::{RunnerConfig, RunnerOptions};
use aurora_store::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

const MEMBER_KEY: &str = "aur_member_key_for_tests_only_not_a_secret";
const LONG: &str = "20";

fn fake_agent() -> PathBuf {
    let bin = PathBuf::from(common::harness::BIN);
    let name = format!("fake_agent{}", std::env::consts::EXE_SUFFIX);
    let path = bin.parent().expect("a folder").join("examples").join(name);
    assert!(
        path.exists(),
        "build the examples first: {}",
        path.display()
    );
    path
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(self.body.trim())
            .unwrap_or_else(|_| panic!("not JSON: {} {}", self.status, self.body))
    }
}

struct Server {
    addr: std::net::SocketAddr,
    owner: String,
    db: PathBuf,
    runner: Option<aurora::Runner>,
    _dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
    done: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn options(max_runs: i64) -> RunnerOptions {
    RunnerOptions {
        poll_every: Duration::from_millis(50),
        schedule_every: Duration::from_millis(100),
        max_runs,
        ..RunnerOptions::default()
    }
}

impl Server {
    async fn start(max_runs: i64) -> Server {
        Server::start_with(options(max_runs), true).await
    }

    async fn start_with(options: RunnerOptions, with_runner: bool) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.db");
        let store = Store::open(&db).unwrap();
        let owner = common::owner_key(&store).await;
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO user (id, name, kind, role) VALUES (2, 'member', 'human', 'member')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO api_key (user_id, name, prefix, key_hash, scope, created)
                     VALUES (2, 'tests', 'aur_memb', ?1, 'write', 1)",
                    [format!("{:x}", Sha256::digest(MEMBER_KEY.as_bytes()))],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let runner = if with_runner {
            let config = RunnerConfig::for_server(&store, addr, options);
            Some(aurora::Runner::start(store.clone(), config).await.unwrap())
        } else {
            None
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let done = tokio::spawn(aurora::serve_with(
            listener,
            store,
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
            runner.clone(),
        ));
        Server {
            addr,
            owner,
            db,
            runner,
            _dir: dir,
            stop: Some(stop),
            done: Some(done),
        }
    }

    async fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(done) = self.done.take() {
            tokio::time::timeout(Duration::from_secs(30), done)
                .await
                .expect("shutdown must not hang")
                .unwrap()
                .unwrap();
        }
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        extra: &str,
        body: &str,
    ) -> Reply {
        let auth = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}{extra}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).await.unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        Reply {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            head: head.to_ascii_lowercase(),
            body: body.to_string(),
        }
    }

    async fn api(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> Reply {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        self.raw(method, &format!("/api/v1{path}"), key, "", &body)
            .await
    }

    async fn owner(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let reply = self.api(method, path, Some(&self.owner), body).await;
        assert!(
            (200..300).contains(&reply.status),
            "{method} {path}: {} {}",
            reply.status,
            reply.body
        );
        serde_json::from_str(reply.body.trim()).unwrap_or(Value::Null)
    }

    async fn page(&self, path: &str) -> Reply {
        self.raw(
            "GET",
            path,
            Some(&self.owner),
            "X-Requested-With: aurora\r\n",
            "",
        )
        .await
    }

    /// A project with a type; issue AA-1 exists afterwards.
    async fn project(&self, key: &str) {
        self.owner(
            "POST",
            "/projects",
            Some(json!({"key": key, "name": format!("Project {key}")})),
        )
        .await;
        self.owner(
            "POST",
            &format!("/projects/{key}/issue-types"),
            Some(json!({"name": "Task"})),
        )
        .await;
        self.issue(key, "Issue one").await;
    }

    async fn issue(&self, project: &str, title: &str) -> i64 {
        self.owner(
            "POST",
            &format!("/projects/{project}/issues"),
            Some(json!({"title": title})),
        )
        .await["number"]
            .as_i64()
            .unwrap()
    }

    /// Registers an agent; returns (agent id, user id, key).
    async fn agent(
        &self,
        name: &str,
        args: &[&str],
        projects: &[&str],
        max_concurrent: i64,
    ) -> (i64, i64, String) {
        let made = self
            .owner(
                "POST",
                "/agents",
                Some(json!({
                    "name": name,
                    "command": fake_agent().display().to_string(),
                    "args": args,
                    "projects": projects,
                    "max_concurrent": max_concurrent,
                    "create_key": true
                })),
            )
            .await;
        (
            made["agent"]["id"].as_i64().unwrap(),
            made["agent"]["user_id"].as_i64().unwrap(),
            made["key"].as_str().unwrap().to_string(),
        )
    }

    async fn assign(&self, project: &str, number: i64, user: i64) {
        self.owner(
            "PATCH",
            &format!("/projects/{project}/issues/{number}"),
            Some(json!({"assignee_id": user})),
        )
        .await;
    }

    async fn run(&self, id: i64) -> Value {
        self.owner("GET", &format!("/runs/{id}"), None).await
    }

    /// The id of the run of `issue` once it exists.
    async fn run_of(&self, issue: &str) -> i64 {
        let started = Instant::now();
        loop {
            let found = self
                .owner("GET", &format!("/runs?issue={issue}"), None)
                .await;
            if let Some(run) = found.as_array().unwrap().first() {
                return run["id"].as_i64().unwrap();
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no run for {issue}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn wait_status(&self, id: i64, want: &str) -> Value {
        let started = Instant::now();
        loop {
            let run = self.run(id).await;
            if run["status"] == want {
                return run;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "run {id} never became {want}: {run}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn queue(&self) -> Value {
        self.owner("GET", "/queue", None).await
    }

    async fn cancel(&self, id: i64) -> Value {
        self.owner("POST", &format!("/runs/{id}/cancel"), None)
            .await
    }

    fn sql<T: aurora_store::rusqlite::types::FromSql>(&self, sql: &str) -> T {
        let conn = aurora_store::rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

fn ids(queue: &Value) -> Vec<i64> {
    queue["queued"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["run_id"].as_i64().unwrap())
        .collect()
}

// ---- the limits ----

#[tokio::test]
async fn limit_one_serialises_three_issues_in_order_and_each_cancel_promotes_the_next() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    server.issue("AA", "three").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 3).await;
    for n in 1..=3 {
        server.assign("AA", n, user).await;
        // One at a time, so the run ids follow the issue numbers.
        server.run_of(&format!("AA-{n}")).await;
    }
    let (r1, r2, r3) = (
        server.run_of("AA-1").await,
        server.run_of("AA-2").await,
        server.run_of("AA-3").await,
    );
    assert!(r1 < r2 && r2 < r3);
    server.wait_status(r1, "running").await;

    let queue = server.queue().await;
    assert_eq!(
        (queue["running"].clone(), queue["limit"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(ids(&queue), vec![r2, r3], "first come, first served");
    let entries = queue["queued"].as_array().unwrap();
    assert_eq!(entries[0]["position"], 1);
    assert_eq!(entries[1]["position"], 2);
    assert_eq!(entries[0]["agent"], "bot");
    assert_eq!(entries[0]["issue"], "AA-2");
    assert_eq!(entries[0]["project"], "AA");
    assert_eq!(entries[0]["waiting_on"], "global");
    assert!(entries[0]["queued_at"].as_i64().unwrap() > 0);
    assert_eq!(queue["queue_full_drops"], 0);

    let queued = server.run(r3).await;
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["queue_position"], 2);
    assert!(queued["queued_at"].is_i64());
    assert!(queued["pid"].is_null() && queued["started"].is_null());
    assert_eq!(
        server.sql::<i64>("SELECT COUNT(*) FROM api_key WHERE name = 'run-2'"),
        0,
        "no key while queued"
    );

    server.cancel(r1).await;
    let second = server.wait_status(r2, "running").await;
    assert_eq!(second["queue_position"], Value::Null);
    assert!(second["queued_at"].is_i64(), "the queue time is kept");
    assert_eq!(server.run(r3).await["queue_position"], 1);
    assert_eq!(ids(&server.queue().await), vec![r3]);

    server.cancel(r2).await;
    server.wait_status(r3, "running").await;
    let queue = server.queue().await;
    assert_eq!(queue["queued"].as_array().unwrap().len(), 0);
    assert_eq!(queue["running"], 1);
    // The message of a promoted run is written, with its working folder.
    let third = server.run(r3).await;
    assert!(
        third["message"].as_str().unwrap().contains("AA-3"),
        "{third}"
    );
    assert!(third["template_source"].is_string());
    server.shutdown().await;
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn an_agent_at_its_limit_does_not_hold_other_agents_back() {
    let mut server = Server::start(4).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    server.issue("AA", "three").await;
    let (_, bot, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 1).await;
    let (_, other, _) = server.agent("other", &["--sleep", LONG], &["AA"], 1).await;
    server.assign("AA", 1, bot).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, bot).await;
    let r2 = server.run_of("AA-2").await;
    // `bot` is at its limit of 1: queued, waiting on the agent.
    let queue = server.queue().await;
    assert_eq!(ids(&queue), vec![r2]);
    assert_eq!(queue["queued"][0]["waiting_on"], "agent");
    // A newer run of another agent is not stuck behind it.
    server.assign("AA", 3, other).await;
    let r3 = server.run_of("AA-3").await;
    server.wait_status(r3, "running").await;
    assert_eq!(server.run(r2).await["status"], "queued");
    assert_eq!(server.queue().await["running"], 2);
    server.cancel(r1).await;
    server.wait_status(r2, "running").await;
    server.shutdown().await;
}

#[tokio::test]
async fn the_overall_limit_holds_runs_of_different_agents_in_arrival_order() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    server.issue("AA", "three").await;
    let (_, a, _) = server.agent("alpha", &["--sleep", LONG], &["AA"], 1).await;
    let (_, b, _) = server.agent("beta", &["--sleep", LONG], &["AA"], 1).await;
    let (_, c, _) = server.agent("gamma", &["--sleep", LONG], &["AA"], 1).await;
    server.assign("AA", 1, a).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, b).await;
    let r2 = server.run_of("AA-2").await;
    server.assign("AA", 3, c).await;
    let r3 = server.run_of("AA-3").await;
    let queue = server.queue().await;
    assert_eq!(ids(&queue), vec![r2, r3]);
    assert!(
        queue["queued"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["waiting_on"] == "global")
    );
    server.cancel(r1).await;
    server.wait_status(r2, "running").await;
    assert_eq!(server.run(r3).await["status"], "queued");
    server.shutdown().await;
}

#[tokio::test]
async fn a_second_trigger_for_an_issue_with_a_queued_run_is_dropped() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 2).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;
    assert_eq!(server.run(r2).await["status"], "queued");
    // A mention is another trigger for the same issue: the queued run is the
    // pending intent, so it is dropped, not queued a second time.
    server
        .owner(
            "POST",
            "/projects/AA/issues/2/comments",
            Some(json!({"body": "@bot again"})),
        )
        .await;
    server.settle().await;
    let runs = server.owner("GET", "/runs?issue=AA-2", None).await;
    assert_eq!(runs.as_array().unwrap().len(), 1);
    assert_eq!(ids(&server.queue().await), vec![r2]);
    assert_eq!(
        server.queue().await["queue_full_drops"],
        0,
        "a drop for an active issue is not a full queue"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn canceling_a_queued_run_is_immediate_and_safe_to_repeat() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    server.issue("AA", "three").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 3).await;
    for n in 1..=3 {
        server.assign("AA", n, user).await;
        server.run_of(&format!("AA-{n}")).await;
    }
    let (r1, r2, r3) = (
        server.run_of("AA-1").await,
        server.run_of("AA-2").await,
        server.run_of("AA-3").await,
    );
    server.wait_status(r1, "running").await;

    let path = format!("/api/v1/runs/{r2}/cancel");
    let header = "Idempotency-Key: cancel-queued-0001\r\n";
    let started = Instant::now();
    let first = server
        .raw("POST", &path, Some(&server.owner), header, "")
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a queued run has no process to wait for: {:?}",
        started.elapsed()
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let run = first.json();
    assert_eq!(run["status"], "canceled");
    assert!(run["finished"].is_i64() && run["queue_position"].is_null());
    assert_eq!(run["exit_code"], Value::Null);
    let again = server
        .raw("POST", &path, Some(&server.owner), header, "")
        .await;
    assert_eq!(again.status, 200);
    assert!(again.head.contains("idempotent-replay: true"));
    let second = server
        .api(
            "POST",
            &format!("/runs/{r2}/cancel"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(second.status, 409, "{}", second.body);

    assert_eq!(ids(&server.queue().await), vec![r3]);
    assert_eq!(server.run(r3).await["queue_position"], 1);
    assert_eq!(
        server.sql::<i64>(&format!(
            "SELECT COUNT(*) FROM event WHERE kind = 'run.finished' AND entity_id = {r2}"
        )),
        1
    );
    assert_eq!(
        server.sql::<i64>("SELECT COUNT(*) FROM event WHERE kind = 'run.promoted'"),
        0
    );
    // Its issue is free again: a new trigger queues a new run.
    server.assign("AA", 2, user).await; // a no-op change (same assignee) ...
    server
        .owner(
            "POST",
            "/projects/AA/issues/2/comments",
            Some(json!({"body": "@bot go"})),
        )
        .await;
    let again = server.owner("GET", "/runs?issue=AA-2", None).await;
    let started = Instant::now();
    loop {
        let all = server.owner("GET", "/runs?issue=AA-2", None).await;
        if all.as_array().unwrap().len() == 2 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{again}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_timeout_frees_the_slot_and_the_queue_moves_on() {
    let mut server = Server::start_with(
        RunnerOptions {
            timeout_override: Some(Duration::from_secs(1)),
            ..options(1)
        },
        true,
    )
    .await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    let (_, user, _) = server.agent("bot", &["--sleep", "30"], &["AA"], 2).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;
    assert_eq!(server.run(r2).await["status"], "queued");

    let first = server.wait_status(r1, "failed").await;
    assert_eq!(first["error"], "timed out after 1 s");
    let text = server
        .api(
            "GET",
            &format!("/runs/{r1}/transcript.txt"),
            Some(&server.owner),
            None,
        )
        .await;
    assert!(text.body.contains("run timed out"), "{}", text.body);
    let second = server.wait_status(r2, "failed").await;
    assert_eq!(second["error"], "timed out after 1 s");
    assert!(
        second["started"].as_i64().unwrap() >= first["finished"].as_i64().unwrap(),
        "the queued run starts after the stuck one ended"
    );
    // The wait did not count against the timeout: it ran for about a second.
    let ran = second["finished"].as_i64().unwrap() - second["started"].as_i64().unwrap();
    assert!((900..8000).contains(&ran), "{ran} ms");
    let queue = server.queue().await;
    assert_eq!(queue["running"], 0);
    server.shutdown().await;
}

#[tokio::test]
async fn a_full_queue_drops_the_trigger_and_counts_it() {
    let mut server = Server::start_with(
        RunnerOptions {
            max_queued_per_agent: 1,
            ..options(1)
        },
        true,
    )
    .await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    server.issue("AA", "three").await;
    let (agent, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 3).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;
    server.assign("AA", 3, user).await;
    server.settle().await;
    assert_eq!(
        server
            .owner("GET", "/runs?issue=AA-3", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        0,
        "the third trigger found the queue full"
    );
    let queue = server.queue().await;
    assert_eq!(queue["queue_full_drops"], 1);
    assert_eq!(ids(&queue), vec![r2]);
    // A manual start meets the same wall, and says so.
    let manual = server
        .api(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(&server.owner),
            Some(json!({"issue": "AA-3"})),
        )
        .await;
    assert_eq!(manual.status, 409, "{}", manual.body);
    assert!(manual.body.contains("queue full"), "{}", manual.body);
    assert_eq!(server.runner.as_ref().unwrap().queue_full_drops(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn a_queued_run_is_checked_again_before_it_starts() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    for title in ["two", "three", "four", "five"] {
        server.issue("AA", title).await;
    }
    let (agent, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 5).await;
    let (_, keeper, _) = server.agent("keeper", &["--sleep", LONG], &["AA"], 1).await;
    // keeper holds the only slot.
    server.assign("AA", 1, keeper).await;
    let held = server.run_of("AA-1").await;
    server.wait_status(held, "running").await;
    for n in 2..=5 {
        server.assign("AA", n, user).await;
        server.run_of(&format!("AA-{n}")).await;
    }
    let ids_of: Vec<i64> = {
        let mut out = Vec::new();
        for n in 2..=5 {
            out.push(server.run_of(&format!("AA-{n}")).await);
        }
        out
    };

    // The issue is archived while its run waits.
    server
        .owner("POST", "/projects/AA/issues/2/archive", None)
        .await;
    let archived = server.wait_status(ids_of[0], "canceled").await;
    assert_eq!(archived["error"], "issue archived");
    assert!(archived["finished"].is_i64());
    assert!(archived["pid"].is_null());

    // The agent is disabled: everything it has queued ends, with the reason.
    server
        .owner("POST", &format!("/agents/{agent}/disable"), None)
        .await;
    for id in &ids_of[1..] {
        let run = server.wait_status(*id, "canceled").await;
        assert_eq!(run["error"], "agent disabled", "{run}");
    }
    assert_eq!(server.run(held).await["status"], "running");
    assert_eq!(server.queue().await["queued"].as_array().unwrap().len(), 0);
    assert_eq!(
        server.sql::<i64>("SELECT COUNT(*) FROM event WHERE kind = 'run.promoted'"),
        0,
        "nothing was started"
    );
    server.shutdown().await;
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn a_queued_run_whose_project_access_was_taken_away_is_canceled() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    let (_, keeper, _) = server.agent("keeper", &["--sleep", LONG], &["AA"], 1).await;
    let (agent, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 1).await;
    server.assign("AA", 1, keeper).await;
    let held = server.run_of("AA-1").await;
    server.wait_status(held, "running").await;
    server.assign("AA", 2, user).await;
    let queued = server.run_of("AA-2").await;
    assert_eq!(server.run(queued).await["status"], "queued");
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"projects": []})),
        )
        .await;
    let run = server.wait_status(queued, "canceled").await;
    assert_eq!(run["error"], "agent no longer has access to project AA");
    server.shutdown().await;
}

#[tokio::test]
async fn a_queued_run_that_waited_too_long_is_canceled_and_a_higher_limit_promotes() {
    let mut server = Server::start_with(
        RunnerOptions {
            max_queue_wait: Some(Duration::from_millis(500)),
            ..options(1)
        },
        true,
    )
    .await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 3).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;
    let run = server.wait_status(r2, "canceled").await;
    assert_eq!(run["error"], "queue wait exceeded");
    assert_eq!(server.run(r1).await["status"], "running");
    server.shutdown().await;
}

#[tokio::test]
async fn raising_the_limit_promotes_at_once_and_lowering_it_stops_nothing() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    server.issue("AA", "three").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 3).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;
    assert_eq!(server.run(r2).await["status"], "queued");
    let runner = server.runner.clone().unwrap();
    runner.set_max_runs(2);
    server.wait_status(r2, "running").await;
    assert_eq!(server.queue().await["limit"], 2);
    // Lowering never stops anything; a new run just waits.
    runner.set_max_runs(1);
    server.assign("AA", 3, user).await;
    let r3 = server.run_of("AA-3").await;
    server.settle().await;
    assert_eq!(server.run(r3).await["status"], "queued");
    assert_eq!(server.run(r1).await["status"], "running");
    assert_eq!(server.run(r2).await["status"], "running");
    server.cancel(r1).await;
    server.settle().await;
    assert_eq!(
        server.run(r3).await["status"],
        "queued",
        "one run is still going and the limit is 1"
    );
    server.cancel(r2).await;
    server.wait_status(r3, "running").await;
    runner.set_max_runs(500);
    assert_eq!(runner.max_runs(), 64, "the limit is clamped to 64");
    server.shutdown().await;
}

// ---- manual starts, messages and the API ----

#[tokio::test]
async fn a_manual_start_over_the_limit_returns_a_queued_run_and_keeps_the_message() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    let (agent, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 2).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    let reply = server
        .api(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(&server.owner),
            Some(json!({"issue": "AA-2", "message": "My own words for the agent"})),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let made = reply.json();
    assert_eq!(made["status"], "queued");
    assert_eq!(made["queue_position"], 1);
    assert_eq!(made["reason"], "manual");
    assert_eq!(made["edited"], true);
    assert!(made["pid"].is_null());
    let id = made["id"].as_i64().unwrap();
    server.cancel(r1).await;
    let run = server.wait_status(id, "running").await;
    assert_eq!(run["message"], "My own words for the agent");
    assert_eq!(run["edited"], true);
    server.shutdown().await;
}

#[tokio::test]
async fn the_first_message_of_a_queued_run_is_written_when_it_starts() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "Original title").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 2).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;
    let waiting = server.run(r2).await;
    assert_eq!(waiting["message"], "", "nothing is written while it waits");
    assert!(waiting["template_source"].is_null());
    server
        .owner(
            "PATCH",
            "/projects/AA/issues/2",
            Some(json!({"title": "Changed while waiting"})),
        )
        .await;
    server.cancel(r1).await;
    let run = server.wait_status(r2, "running").await;
    let message = run["message"].as_str().unwrap();
    assert!(message.contains("Changed while waiting"), "{message}");
    assert!(!message.contains("Original title"));
    assert_eq!(run["edited"], false);
    server.shutdown().await;
}

#[tokio::test]
async fn the_run_list_filters_on_queued_and_a_queued_run_has_no_transcript_yet() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "two").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 2).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;

    let listed = server.owner("GET", "/runs?status=queued", None).await;
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], r2);
    assert_eq!(listed[0]["queue_position"], 1);
    assert!(listed[0]["queued_at"].is_i64());
    assert!(listed[0].get("message").is_none());
    let bad = server
        .api("GET", "/runs?status=waiting", Some(&server.owner), None)
        .await;
    assert_eq!(bad.status, 422);
    assert!(bad.body.contains("queued"), "{}", bad.body);

    let transcript = server
        .api(
            "GET",
            &format!("/runs/{r2}/transcript"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(transcript.status, 200, "{}", transcript.body);
    let body = transcript.json();
    assert_eq!(body["entries"], json!([]));
    assert_eq!(body["done"], false);
    assert_eq!(body["status"], "queued");
    let text = server
        .api(
            "GET",
            &format!("/runs/{r2}/transcript.txt"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(text.status, 200);
    assert_eq!(text.body, "");
    server.shutdown().await;
}

#[tokio::test]
async fn the_queue_endpoint_needs_a_key_and_shows_agents_only_their_projects() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.project("BB").await;
    server.issue("AA", "two").await;
    server.issue("BB", "two").await;
    let (_, keeper, _) = server
        .agent("keeper", &["--sleep", LONG], &["AA", "BB"], 1)
        .await;
    let (a_agent, a_user, a_key) = server.agent("alpha", &["--sleep", LONG], &["AA"], 1).await;
    let (_, b_user, _) = server.agent("beta", &["--sleep", LONG], &["BB"], 1).await;
    server.assign("AA", 1, keeper).await;
    let held = server.run_of("AA-1").await;
    server.wait_status(held, "running").await;
    server.assign("AA", 2, a_user).await;
    let ra = server.run_of("AA-2").await;
    server.assign("BB", 2, b_user).await;
    let rb = server.run_of("BB-2").await;

    let none = server.api("GET", "/queue", None, None).await;
    assert_eq!(none.status, 401);
    let bad = server.api("GET", "/queue", Some("aur_nope"), None).await;
    assert_eq!(bad.status, 401);

    let owner = server.queue().await;
    assert_eq!(ids(&owner), vec![ra, rb]);
    let member = server.api("GET", "/queue", Some(MEMBER_KEY), None).await;
    assert_eq!(member.status, 200);
    assert_eq!(ids(&member.json()), vec![ra, rb]);

    let scoped = server.api("GET", "/queue", Some(&a_key), None).await;
    assert_eq!(scoped.status, 200, "{}", scoped.body);
    let scoped = scoped.json();
    assert_eq!(ids(&scoped), vec![ra], "alpha never sees BB");
    assert_eq!(scoped["queued"][0]["position"], 1, "positions are overall");
    assert_eq!(scoped["running"], 1, "the running run is in AA");
    assert_eq!(scoped["limit"], 1);

    let one = server
        .owner("GET", &format!("/queue?agent={a_agent}"), None)
        .await;
    assert_eq!(ids(&one), vec![ra]);
    let unknown = server
        .api("GET", "/queue?agent=9999", Some(&server.owner), None)
        .await;
    assert_eq!(unknown.status, 404);
    let junk = server
        .api("GET", "/queue?bogus=1", Some(&server.owner), None)
        .await;
    assert!(junk.status == 400 || junk.status == 422, "{}", junk.status);
    let post = server
        .api("POST", "/queue", Some(&server.owner), None)
        .await;
    assert_eq!(post.status, 405);
    server.shutdown().await;

    let plain = Server::start_with(options(1), false).await;
    let reply = plain.api("GET", "/queue", Some(&plain.owner), None).await;
    assert_eq!(reply.status, 503, "a server without a runner says so");
}

// ---- pages ----

#[tokio::test]
async fn the_pages_show_the_queue_position_a_cancel_button_and_the_agent_panel() {
    let mut server = Server::start(1).await;
    server.project("AA").await;
    server.issue("AA", "<script>alert(1)</script> & more").await;
    let (agent, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 2).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.assign("AA", 2, user).await;
    let r2 = server.run_of("AA-2").await;

    let issue = server.page("/p/AA/2").await;
    assert_eq!(issue.status, 200);
    assert!(issue.body.contains("Queued (position 1)"), "{}", issue.body);
    assert!(issue.body.contains("data-action=\"cancel-run\""));
    assert!(issue.body.contains(&format!("data-run-id=\"{r2}\"")));
    assert!(!issue.body.contains("<script>alert"), "escaped");
    let fragment = server.page("/p/AA/2/runs").await;
    assert!(fragment.body.contains("Queued (position 1)"));
    assert!(fragment.body.contains("queued"), "the queue time is shown");

    let run = server.page(&format!("/p/AA/runs/{r2}")).await;
    assert_eq!(run.status, 200);
    assert!(run.body.contains("Queued (position 1)"));
    assert!(run.body.contains("Waiting for a free slot"), "{}", run.body);
    assert!(run.body.contains("It is written when the run starts"));
    assert!(run.body.contains("data-action=\"cancel-run\""));
    assert!(!run.body.contains("<script>alert"));
    let live = server.page(&format!("/p/AA/runs/{r2}/live")).await;
    assert!(live.body.contains("data-status=\"queued\""));

    let panel = server.page(&format!("/agents/{agent}")).await;
    assert_eq!(panel.status, 200);
    assert!(
        panel.body.contains("id=\"queue-heading\""),
        "{}",
        panel.body
    );
    assert!(panel.body.contains("1 running of 2 allowed for this agent"));
    assert!(panel.body.contains("overall limit 1 runs at once"));
    assert!(panel.body.contains("Queued (position 1)"));
    assert!(panel.body.contains(&format!("href=\"/p/AA/runs/{r2}\"")));
    assert!(panel.body.contains("href=\"/p/AA/2\""));
    assert!(panel.body.contains("waiting for a free slot overall"));

    server.cancel(r2).await;
    let after = server.page(&format!("/agents/{agent}")).await;
    assert!(after.body.contains("No run is waiting."));
    let after = server.page("/p/AA/2").await;
    assert!(!after.body.contains("Queued (position"));
    server.shutdown().await;
}

#[tokio::test]
async fn an_interrupted_run_says_what_that_means_on_its_page() {
    let mut server = Server::start(2).await;
    server.project("AA").await;
    let (_, user, _) = server.agent("bot", &["--sleep", LONG], &["AA"], 1).await;
    server.assign("AA", 1, user).await;
    let r1 = server.run_of("AA-1").await;
    server.wait_status(r1, "running").await;
    server.shutdown().await;
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
    // A fresh server on the same data shows the page of the interrupted run.
    let status: String = server.sql("SELECT status FROM run WHERE id = 1");
    assert_eq!(status, "interrupted");
    let again = Server::start_on(&server.db, options(2)).await;
    let page = again.page(&format!("/p/AA/runs/{r1}")).await;
    assert!(
        page.body
            .contains("Interrupted: the server stopped or crashed"),
        "{}",
        page.body
    );
}

impl Server {
    /// A server on an existing database (no new owner or member).
    async fn start_on(db: &Path, options: RunnerOptions) -> Server {
        let store = Store::open(db).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = RunnerConfig::for_server(&store, addr, options);
        let runner = aurora::Runner::start(store.clone(), config).await.unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let done = tokio::spawn(aurora::serve_with(
            listener,
            store,
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
            Some(runner.clone()),
        ));
        Server {
            addr,
            owner: "aur_integration_tests_only_not_a_secret".to_string(),
            db: db.to_path_buf(),
            runner: Some(runner),
            _dir: tempfile::tempdir().unwrap(),
            stop: Some(stop),
            done: Some(done),
        }
    }
}
