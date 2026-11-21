//! The runner (TRK-15): runs start from assignments and mentions, the process
//! gets the prompt and its own key, and every way a run can end is recorded.
//! The agent is the `fake_agent` example built next to the test binaries.

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
    data: PathBuf,
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

fn options() -> RunnerOptions {
    RunnerOptions {
        poll_every: Duration::from_millis(100),
        timeout_override: None,
    }
}

impl Server {
    async fn start() -> Server {
        Server::start_with(options(), true).await
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
            runner,
        ));
        Server {
            addr,
            owner,
            data: dir.path().to_path_buf(),
            db,
            _dir: dir,
            stop: Some(stop),
            done: Some(done),
        }
    }

    /// Stops the server as a signal would and waits for it.
    async fn shutdown(&mut self) -> Duration {
        let started = Instant::now();
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
        started.elapsed()
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

    /// Project AA with a type and issue 1 "Fix the widget".
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
        self.owner(
            "POST",
            &format!("/projects/{key}/issues"),
            Some(json!({"title": "Fix the widget"})),
        )
        .await;
    }

    /// Registers `bot` (allowed on AA) running `command args`; returns
    /// (agent id, user id, key).
    async fn agent(&self, command: &Path, args: &[&str]) -> (i64, i64, String) {
        let made = self
            .owner(
                "POST",
                "/agents",
                Some(
                    json!({"name": "bot", "command": command.display().to_string(),
                            "args": args, "projects": ["AA"], "create_key": true}),
                ),
            )
            .await;
        (
            made["agent"]["id"].as_i64().unwrap(),
            made["agent"]["user_id"].as_i64().unwrap(),
            made["key"].as_str().unwrap().to_string(),
        )
    }

    async fn assign(&self, number: i64, user: i64) {
        self.owner(
            "PATCH",
            &format!("/projects/AA/issues/{number}"),
            Some(json!({"assignee_id": user})),
        )
        .await;
    }

    async fn runs(&self) -> Vec<Value> {
        self.owner("GET", "/runs", None)
            .await
            .as_array()
            .unwrap()
            .clone()
    }

    /// The only run, once it exists and `done`.
    async fn wait_run(&self, id: Option<i64>, finished: bool) -> Value {
        let started = Instant::now();
        loop {
            let runs = self.runs().await;
            let found = match id {
                Some(id) => runs.iter().find(|r| r["id"] == id).cloned(),
                None => runs.first().cloned(),
            };
            if let Some(run) = found {
                let status = run["status"].as_str().unwrap();
                let over = !matches!(status, "starting" | "running");
                if over == finished && (finished || status == "running") {
                    return self
                        .owner("GET", &format!("/runs/{}", run["id"]), None)
                        .await;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no run reached the wanted state: {runs:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn key_of_run(&self, id: i64) -> (bool, i64) {
        let conn = aurora_store::rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(
            "SELECT revoked IS NOT NULL, COUNT(*) FROM api_key WHERE name = ?1",
            [format!("run-{id}")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }
}

fn tail(run: &Value) -> String {
    run["output_tail"].as_str().unwrap_or_default().to_string()
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

// ---- starting runs ----

#[tokio::test]
async fn assigning_an_issue_starts_a_run_with_the_prompt_and_its_own_key() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &[]).await;
    server.assign(1, user).await;

    let run = server.wait_run(None, true).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["exit_code"], 0);
    assert_eq!(run["reason"], "assigned");
    assert_eq!(run["issue"], "AA-1");
    let output = tail(&run);
    assert!(output.contains("Fix the widget"), "{output}");
    assert!(output.contains("AURORA_API_KEY"), "{output}");
    assert!(output.contains("AURORA_MCP_URL"), "{output}");
    assert!(output.contains("AURORA_ISSUE=AA-1"), "{output}");
    assert!(output.contains("AURORA_TRIGGER=assigned"), "{output}");
    assert!(run["message"].as_str().unwrap().contains("Fix the widget"));
    assert_eq!(run["template_source"], "builtin");
    assert!(run["started"].is_number() && run["finished"].is_number());
    let (revoked, count) = server.key_of_run(run["id"].as_i64().unwrap());
    assert_eq!((revoked, count), (true, 1), "the per-run key is revoked");
    assert!(
        server
            .data
            .join("runs")
            .join(run["id"].to_string())
            .is_dir(),
        "a folder per run"
    );
}

#[tokio::test]
async fn mentioning_the_agent_starts_a_run() {
    let server = Server::start().await;
    server.project("AA").await;
    server.agent(&fake_agent(), &[]).await;
    server
        .owner(
            "POST",
            "/projects/AA/issues/1/comments",
            Some(json!({"body": "@bot please have a look"})),
        )
        .await;
    let run = server.wait_run(None, true).await;
    assert_eq!(run["reason"], "mentioned");
    assert!(tail(&run).contains("AURORA_TRIGGER=mentioned"));
    assert!(
        run["message"]
            .as_str()
            .unwrap()
            .contains("please have a look"),
        "the trigger comment is in the prompt"
    );
}

#[tokio::test]
async fn disabled_agents_other_projects_and_switched_off_triggers_start_nothing() {
    let server = Server::start().await;
    server.project("AA").await;
    server.project("BB").await;
    let (agent, user, _) = server.agent(&fake_agent(), &[]).await;

    server
        .owner("POST", &format!("/agents/{agent}/disable"), None)
        .await;
    server.assign(1, user).await;
    settle().await;
    assert!(server.runs().await.is_empty(), "a disabled agent");
    server
        .owner("POST", &format!("/agents/{agent}/enable"), None)
        .await;

    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"on_assign": false})),
        )
        .await;
    server
        .owner(
            "PATCH",
            "/projects/AA/issues/1",
            Some(json!({"assignee_id": null})),
        )
        .await;
    server.assign(1, user).await;
    settle().await;
    assert!(server.runs().await.is_empty(), "on_assign is off");
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"on_assign": true})),
        )
        .await;

    server
        .owner(
            "POST",
            "/projects/BB/issues",
            Some(json!({"title": "elsewhere", "assignee_id": user})),
        )
        .await;
    settle().await;
    assert!(
        server.runs().await.is_empty(),
        "a project that is not allowed"
    );
}

#[tokio::test]
async fn a_redelivered_event_makes_no_second_run_and_history_is_not_replayed_on_first_start() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &[]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;

    let conn = aurora_store::rusqlite::Connection::open(&server.db).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute(
        "UPDATE consumer_cursor SET last_event_id = 0 WHERE name = 'runner'",
        [],
    )
    .unwrap();
    settle().await;
    let runs = server.runs().await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["id"], run["id"]);
}

#[tokio::test]
async fn an_issue_with_an_active_run_drops_the_next_trigger() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["--sleep", "30"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, false).await;
    server
        .owner(
            "POST",
            "/projects/AA/issues/1/comments",
            Some(json!({"body": "@bot again"})),
        )
        .await;
    settle().await;
    assert_eq!(server.runs().await.len(), 1, "no queue yet (TRK-18)");
    let id = run["id"].as_i64().unwrap();
    let cancelled = server
        .api(
            "POST",
            &format!("/runs/{id}/cancel"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(cancelled.status, 200, "{}", cancelled.body);
}

#[tokio::test]
async fn exit_codes_decide_success_and_failure() {
    let server = Server::start().await;
    server.project("AA").await;
    let (agent, user, _) = server.agent(&fake_agent(), &["--exit", "3"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    assert_eq!(run["status"], "failed");
    assert_eq!(run["exit_code"], 3);
    assert_eq!(run["error"], "exited with code 3");
    let _ = agent;
}

#[tokio::test]
async fn long_output_keeps_only_the_tail() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["--flood", "300000"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    assert_eq!(run["status"], "succeeded");
    assert_eq!(run["output_truncated"], true);
    let output = tail(&run);
    assert!(output.len() <= 64 * 1024, "{}", output.len());
    assert!(output.contains("TAIL-MARKER"));
    assert!(!output.contains("STDIN-BEGIN"), "the start was dropped");
}

#[tokio::test]
async fn a_run_that_outlives_its_timeout_is_killed_and_failed() {
    let server = Server::start_with(
        RunnerOptions {
            timeout_override: Some(Duration::from_secs(1)),
            ..options()
        },
        true,
    )
    .await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["--sleep", "60"]).await;
    server.assign(1, user).await;
    let started = Instant::now();
    let run = server.wait_run(None, true).await;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(run["status"], "failed");
    assert_eq!(run["error"], "timed out after 1 s");
}

#[tokio::test]
async fn the_per_run_key_works_during_the_run_and_dies_with_it() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server
        .agent(&fake_agent(), &["--comment", "hello from the run"])
        .await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    assert!(
        tail(&run).contains("COMMENT: http 200 error=false"),
        "{}",
        tail(&run)
    );
    let comments = server
        .owner("GET", "/projects/AA/issues/1/comments", None)
        .await;
    let text = comments.to_string();
    assert!(text.contains("hello from the run"), "{text}");
    assert!(
        text.contains("\"bot\""),
        "the agent user is the author: {text}"
    );
    let (revoked, _) = server.key_of_run(run["id"].as_i64().unwrap());
    assert!(revoked);
}

#[tokio::test]
async fn a_claude_named_command_gets_an_mcp_config_and_placeholders_are_substituted() {
    let server = Server::start().await;
    server.project("AA").await;
    let folder = tempfile::tempdir().unwrap();
    let claude = folder
        .path()
        .join(format!("claude{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(fake_agent(), &claude).unwrap();
    let (_, user, _) = server.agent(&claude, &["--tag={issue}", "{run_id}"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    let id = run["id"].as_i64().unwrap();
    let output = tail(&run);
    assert!(
        output.contains(&format!("--tag=AA-1|{id}|--mcp-config|")),
        "{output}"
    );
    let config = server
        .data
        .join("runs")
        .join(id.to_string())
        .join("mcp.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
    let url = config["mcpServers"]["aurora"]["url"].as_str().unwrap();
    assert!(
        url.ends_with("/mcp") && url.starts_with("http://127.0.0.1:"),
        "{url}"
    );
    assert_eq!(config["mcpServers"]["aurora"]["type"], "http");
    let bearer = config["mcpServers"]["aurora"]["headers"]["Authorization"]
        .as_str()
        .unwrap();
    assert!(bearer.starts_with("Bearer aur_"), "{bearer}");
    assert!(
        !output.contains(&bearer["Bearer ".len()..]),
        "the key is never printed"
    );
    let cwd = output.lines().find(|l| l.starts_with("CWD: ")).unwrap();
    assert!(cwd.contains("work"), "an empty folder per run: {cwd}");
}

#[tokio::test]
async fn the_prompt_goes_into_a_placeholder_instead_of_standard_input() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["{prompt}"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    let output = tail(&run);
    let args = output.lines().find(|l| l.starts_with("ARGS: ")).unwrap();
    assert!(args.contains("Fix the widget"), "{args}");
    assert!(
        output.contains("STDIN-BEGIN\n\nSTDIN-END")
            || output.contains("STDIN-BEGIN\r\n\r\nSTDIN-END"),
        "standard input stays empty: {output}"
    );
}

#[tokio::test]
async fn a_missing_command_or_work_dir_fails_the_run_with_a_clear_error() {
    let server = Server::start().await;
    server.project("AA").await;
    let missing = Path::new("definitely-not-a-command-xyz");
    let (agent, user, _) = server.agent(missing, &[]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    assert_eq!(run["status"], "failed");
    assert!(
        run["error"].as_str().unwrap().contains("was not found"),
        "{run}"
    );
    let (revoked, _) = server.key_of_run(run["id"].as_i64().unwrap());
    assert!(revoked);

    let missing_dir = if cfg!(windows) {
        "C:\\no\\such\\folder"
    } else {
        "/no/such/folder"
    };
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"command": fake_agent().display().to_string(), "work_dir": missing_dir})),
        )
        .await;
    let second = server
        .owner(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(json!({"issue": "AA-1"})),
        )
        .await;
    let run = server.wait_run(second["id"].as_i64(), true).await;
    assert_eq!(run["status"], "failed");
    assert!(run["error"].as_str().unwrap().contains("work_dir"), "{run}");
}

#[cfg(windows)]
#[tokio::test]
async fn cmd_shims_run_and_refuse_a_prompt_argument() {
    let server = Server::start().await;
    server.project("AA").await;
    let folder = tempfile::tempdir().unwrap();
    let shim = folder.path().join("agent-shim.cmd");
    std::fs::write(&shim, "@echo off\r\nfindstr \"^\"\r\n").unwrap();
    let (agent, user, _) = server.agent(&shim, &[]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, true).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert!(
        tail(&run).contains("Fix the widget"),
        "stdin reached the shim: {}",
        tail(&run)
    );

    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"args": ["{prompt}"]})),
        )
        .await;
    let again = server
        .owner(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(json!({"issue": "AA-1"})),
        )
        .await;
    let run = server.wait_run(again["id"].as_i64(), true).await;
    assert_eq!(run["status"], "failed");
    assert!(run["error"].as_str().unwrap().contains("{prompt}"), "{run}");
}

// ---- the API ----

#[tokio::test]
async fn manual_start_cancel_and_the_error_cases() {
    let server = Server::start().await;
    server.project("AA").await;
    let (agent, _, agent_key) = server.agent(&fake_agent(), &["--sleep", "30"]).await;
    let path = format!("/agents/{agent}/runs");

    let none = server
        .api("POST", &path, None, Some(json!({"issue": "AA-1"})))
        .await;
    assert_eq!(none.status, 401);
    let by_agent = server
        .api(
            "POST",
            &path,
            Some(&agent_key),
            Some(json!({"issue": "AA-1"})),
        )
        .await;
    assert_eq!(by_agent.status, 403);
    for body in [
        json!({"issue": "nonsense"}),
        json!({"issue": "AA-99"}),
        json!({"issue": "ZZ-1"}),
    ] {
        let reply = server
            .api("POST", &path, Some(&server.owner), Some(body))
            .await;
        assert_eq!(reply.status, 422, "{}", reply.body);
    }
    let unknown = server
        .api(
            "POST",
            "/agents/999/runs",
            Some(&server.owner),
            Some(json!({"issue": "AA-1"})),
        )
        .await;
    assert_eq!(unknown.status, 404);

    let edited = "Do exactly this {{not a template}}";
    let made = server
        .api(
            "POST",
            &path,
            Some(&server.owner),
            Some(json!({"issue": "AA-1", "message": edited})),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.body);
    let made = made.json();
    assert_eq!(made["reason"], "manual");
    assert_eq!(made["edited"], true);
    assert_eq!(made["message"], edited);
    let id = made["id"].as_i64().unwrap();
    let run = server.wait_run(Some(id), false).await;
    assert_eq!(run["status"], "running");

    let busy = server
        .api(
            "POST",
            &path,
            Some(&server.owner),
            Some(json!({"issue": "AA-1"})),
        )
        .await;
    assert_eq!(busy.status, 409, "{}", busy.body);

    let cancel = format!("/runs/{id}/cancel");
    let denied = server.api("POST", &cancel, Some(&agent_key), None).await;
    assert_eq!(denied.status, 403);
    let started = Instant::now();
    let done = server.api("POST", &cancel, Some(&server.owner), None).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.json()["status"], "canceled");
    assert!(started.elapsed() < Duration::from_secs(15));
    let again = server.api("POST", &cancel, Some(&server.owner), None).await;
    assert_eq!(again.status, 409);
    let missing = server
        .api("POST", "/runs/999/cancel", Some(&server.owner), None)
        .await;
    assert_eq!(missing.status, 404);
    // The process is stopped and what it printed before is kept.
    let run = server.owner("GET", &format!("/runs/{id}"), None).await;
    assert_eq!(run["status"], "canceled");
    let started = Instant::now();
    while tail(&run).is_empty() && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let run = server.owner("GET", &format!("/runs/{id}"), None).await;
        if !tail(&run).is_empty() {
            assert!(tail(&run).contains(edited));
            break;
        }
    }
    let (revoked, _) = server.key_of_run(id);
    assert!(revoked);
    let get_missing = server
        .api("GET", "/runs/999", Some(&server.owner), None)
        .await;
    assert_eq!(get_missing.status, 404);
    let bad_status = server
        .api("GET", "/runs?status=weird", Some(&server.owner), None)
        .await;
    assert_eq!(bad_status.status, 422);
    let bad_issue = server
        .api("GET", "/runs?issue=nope", Some(&server.owner), None)
        .await;
    assert_eq!(bad_issue.status, 422);
}

#[tokio::test]
async fn a_server_without_a_runner_says_so() {
    let server = Server::start_with(options(), false).await;
    server.project("AA").await;
    let (agent, _, _) = server.agent(&fake_agent(), &[]).await;
    let reply = server
        .api(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(&server.owner),
            Some(json!({"issue": "AA-1"})),
        )
        .await;
    assert_eq!(reply.status, 503);
}

#[tokio::test]
async fn members_see_everything_agents_only_their_projects_and_never_the_text() {
    let server = Server::start().await;
    server.project("AA").await;
    server.project("BB").await;
    let (agent, user, agent_key) = server.agent(&fake_agent(), &[]).await;
    server.assign(1, user).await;
    let aa = server.wait_run(None, true).await;
    // A run in BB, made by the owner of the registry: allow BB for a moment.
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"projects": ["AA", "BB"]})),
        )
        .await;
    let bb = server
        .owner(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(json!({"issue": "BB-1"})),
        )
        .await;
    server.wait_run(bb["id"].as_i64(), true).await;
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"projects": ["AA"]})),
        )
        .await;

    let member = server.api("GET", "/runs", Some(MEMBER_KEY), None).await;
    assert_eq!(member.json().as_array().unwrap().len(), 2);
    let detail = server
        .api(
            "GET",
            &format!("/runs/{}", aa["id"]),
            Some(MEMBER_KEY),
            None,
        )
        .await
        .json();
    assert!(detail["message"].is_string() && detail["output_tail"].is_string());

    let listed = server
        .api("GET", "/runs", Some(&agent_key), None)
        .await
        .json();
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["issue"], "AA-1");
    assert!(listed[0].get("message").is_none() && listed[0].get("output_tail").is_none());
    let own = server
        .api(
            "GET",
            &format!("/runs/{}", aa["id"]),
            Some(&agent_key),
            None,
        )
        .await;
    assert_eq!(own.status, 200);
    assert!(own.json().get("message").is_none() && own.json().get("output_tail").is_none());
    let other = server
        .api(
            "GET",
            &format!("/runs/{}", bb["id"]),
            Some(&agent_key),
            None,
        )
        .await;
    assert_eq!(other.status, 403);
    let filtered = server
        .api("GET", "/runs?issue=BB-1", Some(&agent_key), None)
        .await;
    assert_eq!(filtered.status, 403);
    let by_issue = server
        .api("GET", "/runs?issue=BB-1", Some(&server.owner), None)
        .await;
    assert_eq!(by_issue.json().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn repeating_a_manual_start_with_the_same_request_id_starts_one_process() {
    let server = Server::start().await;
    server.project("AA").await;
    let (agent, _, _) = server
        .agent(&fake_agent(), &["--comment", "ran once"])
        .await;
    let path = format!("/api/v1/agents/{agent}/runs");
    let body = json!({"issue": "AA-1"}).to_string();
    let header = "Idempotency-Key: manual-start-0001\r\n";
    let first = server
        .raw("POST", &path, Some(&server.owner), header, &body)
        .await;
    assert_eq!(first.status, 201, "{}", first.body);
    let run = server.wait_run(first.json()["id"].as_i64(), true).await;
    assert_eq!(run["status"], "succeeded");
    let second = server
        .raw("POST", &path, Some(&server.owner), header, &body)
        .await;
    assert_eq!(second.status, 201);
    assert!(second.head.contains("idempotent-replay: true"));
    assert_eq!(second.json()["id"], first.json()["id"]);
    settle().await;
    assert_eq!(server.runs().await.len(), 1);
    let comments = server
        .owner("GET", "/projects/AA/issues/1/comments", None)
        .await;
    assert_eq!(comments.to_string().matches("ran once").count(), 1);
}

// ---- pages ----

impl Server {
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
}

#[tokio::test]
async fn the_run_pages_show_the_run_escaped_and_with_a_cancel_button_while_it_is_active() {
    let server = Server::start().await;
    server.project("AA").await;
    server
        .owner(
            "POST",
            "/projects/AA/issues",
            Some(json!({"title": "<script>alert(1)</script> & more"})),
        )
        .await;
    let (agent, user, _) = server.agent(&fake_agent(), &[]).await;
    server.assign(2, user).await;
    let run = server.wait_run(None, true).await;
    let id = run["id"].as_i64().unwrap();

    let page = server.page(&format!("/p/AA/runs/{id}")).await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.head.contains("no-store"));
    assert!(page.body.contains("Run "), "{}", page.body);
    assert!(
        page.body
            .contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; more")
    );
    assert!(!page.body.contains("<script>alert"), "escaped everywhere");
    assert!(!page.body.contains(" style="), "no inline style");
    assert!(page.body.contains("data-run-id=\"") && page.body.contains("data-page=\"run\""));
    assert!(
        page.body.contains("data-ts="),
        "times are localized by the script"
    );
    assert!(
        !page.body.contains("cancel-run"),
        "a finished run has no cancel button"
    );
    assert!(page.body.contains("href=\"/p/AA/2\""));

    let live = server.page(&format!("/p/AA/runs/{id}/live")).await;
    assert_eq!(live.status, 200);
    assert!(
        live.body
            .starts_with("<div class=\"run-live\" id=\"run-live\""),
        "{}",
        live.body
    );
    let issue = server.page("/p/AA/2").await;
    assert!(issue.body.contains("id=\"runs\""));
    assert!(issue.body.contains(&format!("href=\"/p/AA/runs/{id}\"")));
    let fragment = server.page("/p/AA/2/runs").await;
    assert!(
        fragment
            .body
            .starts_with("<section class=\"runs\" id=\"runs\""),
        "{}",
        fragment.body
    );
    let other_issue = server.page("/p/AA/1").await;
    assert!(other_issue.body.contains("No runs yet."));

    server.project("BB").await;
    assert_eq!(server.page(&format!("/p/BB/runs/{id}")).await.status, 404);
    assert_eq!(server.page("/p/AA/runs/9999").await.status, 404);

    let agent_page = server.page(&format!("/agents/{agent}")).await;
    assert!(agent_page.body.contains("data-form=\"start-run\""));
    assert!(agent_page.body.contains("Run on an issue now"));
    assert!(agent_page.body.contains("data-action=\"preview-run\""));
}

#[tokio::test]
async fn an_active_run_page_offers_cancel() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["--sleep", "30"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, false).await;
    let id = run["id"].as_i64().unwrap();
    let page = server.page(&format!("/p/AA/runs/{id}")).await;
    assert!(
        page.body.contains("data-action=\"cancel-run\""),
        "{}",
        page.body
    );
    assert!(page.body.contains("Nothing yet."));
    let done = server
        .api(
            "POST",
            &format!("/runs/{id}/cancel"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(done.status, 200);
    let page = server.page(&format!("/p/AA/runs/{id}")).await;
    assert!(!page.body.contains("cancel-run"));
}

// ---- shutdown ----

#[tokio::test]
async fn shutdown_kills_children_marks_runs_interrupted_and_does_not_hang() {
    let mut server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["--sleep", "60"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(None, false).await;
    let id = run["id"].as_i64().unwrap();
    let took = server.shutdown().await;
    assert!(took < Duration::from_secs(20), "{took:?}");
    let conn = aurora_store::rusqlite::Connection::open(&server.db).unwrap();
    let (status, error): (String, String) = conn
        .query_row("SELECT status, error FROM run WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (status.as_str(), error.as_str()),
        ("interrupted", "server stopped")
    );
    assert_eq!(server.key_of_run(id), (true, 1));
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn the_database_check_passes_after_runs() {
    let mut server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&fake_agent(), &["--exit", "2"]).await;
    server.assign(1, user).await;
    server.wait_run(None, true).await;
    server.shutdown().await;
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}
