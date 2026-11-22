//! The live transcript and cancel (TRK-17): output is stored in full in an
//! append-only file and read through the API while the run goes on and
//! afterwards; a cancel ends the process tree and keeps what was printed.
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
        Server::start_with(options(), None).await
    }

    async fn start_with(options: RunnerOptions, cap: Option<u64>) -> Server {
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
        let mut config = RunnerConfig::for_server(&store, addr, options);
        if let Some(cap) = cap {
            config.transcript_max_bytes = cap;
        }
        let runner = aurora::Runner::start(store.clone(), config).await.unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let done = tokio::spawn(aurora::serve_with(
            listener,
            store,
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
            Some(runner),
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
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let head = head.to_ascii_lowercase();
        let body = if head.contains("transfer-encoding: chunked") {
            dechunk(body)
        } else {
            body.to_string()
        };
        Reply {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            head,
            body,
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

    /// (agent id, user id, key) of `bot`, allowed on AA.
    async fn agent(&self, args: &[&str]) -> (i64, i64, String) {
        self.agent_named("bot", args, &["AA"]).await
    }

    async fn agent_named(
        &self,
        name: &str,
        args: &[&str],
        projects: &[&str],
    ) -> (i64, i64, String) {
        let made = self
            .owner(
                "POST",
                "/agents",
                Some(
                    json!({"name": name, "command": fake_agent().display().to_string(),
                        "args": args, "projects": projects, "create_key": true}),
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

    /// The only run, once it is `running` (or finished, when `finished`).
    async fn wait_run(&self, finished: bool) -> Value {
        let started = Instant::now();
        loop {
            let runs = self.owner("GET", "/runs", None).await;
            if let Some(run) = runs.as_array().and_then(|r| r.first()) {
                let status = run["status"].as_str().unwrap();
                let over = !matches!(status, "starting" | "running");
                if (finished && over) || (!finished && status == "running") {
                    return self
                        .owner("GET", &format!("/runs/{}", run["id"]), None)
                        .await;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no run: {runs}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn transcript(&self, id: i64, query: &str) -> Reply {
        self.api(
            "GET",
            &format!("/runs/{id}/transcript{query}"),
            Some(&self.owner),
            None,
        )
        .await
    }

    /// Every entry, read page by page until `done`.
    async fn entries(&self, id: i64) -> Vec<Value> {
        let mut out = Vec::new();
        let mut next = 0;
        let started = Instant::now();
        loop {
            let reply = self
                .transcript(id, &format!("?after={next}&limit=2000"))
                .await;
            assert_eq!(reply.status, 200, "{}", reply.body);
            let page = reply.json();
            out.extend(page["entries"].as_array().unwrap().clone());
            next = page["next"].as_u64().unwrap();
            if page["done"] == true {
                return out;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "never done");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn file(&self, id: i64) -> PathBuf {
        self.data.join("transcripts").join(format!("{id}.log"))
    }

    fn key_revoked(&self, id: i64) -> bool {
        let conn = aurora_store::rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(
            "SELECT revoked IS NOT NULL FROM api_key WHERE name = ?1",
            [format!("run-{id}")],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn assert_checks(&self) {
        let report = aurora_store::invariants::check_file(&self.db).unwrap();
        assert!(report.ok(), "{:#?}", report.failed());
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
}

fn dechunk(mut text: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = text.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 || rest.len() < size {
            break;
        }
        out.push_str(&rest[..size]);
        text = rest[size..].strip_prefix("\r\n").unwrap_or(&rest[size..]);
    }
    out
}

fn text_of(entries: &[Value], stream: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e["s"] == stream)
        .map(|e| e["d"].as_str().unwrap().to_string())
        .collect()
}

fn alive(pid: u64) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
    }
}

async fn gone(pid: u64) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git must be installed for this test: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn make_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(
        dir.path(),
        &["config", "user.email", "tests@example.invalid"],
    );
    git(dir.path(), &["config", "user.name", "Tests"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "first"]);
    dir
}

// ---- storing and reading ----

#[tokio::test]
async fn output_is_stored_in_order_with_stream_tags_and_readable_afterwards() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server
        .agent(&[
            "--say",
            "OUT-ONE",
            "--sleep-ms",
            "250",
            "--stderr",
            "ERR-TWO",
            "--sleep-ms",
            "250",
            "--say",
            "OUT-THREE",
        ])
        .await;
    server.assign(1, user).await;
    let run = server.wait_run(true).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let id = run["id"].as_i64().unwrap();

    let entries = server.entries(id).await;
    let marks: Vec<(String, String)> = entries
        .iter()
        .filter(|e| {
            ["OUT-ONE", "ERR-TWO", "OUT-THREE"]
                .iter()
                .any(|m| e["d"].as_str().unwrap().starts_with(m))
        })
        .map(|e| {
            (
                e["s"].as_str().unwrap().to_string(),
                e["d"].as_str().unwrap().trim_end().to_string(),
            )
        })
        .collect();
    assert_eq!(
        marks,
        [
            ("o".to_string(), "OUT-ONE".to_string()),
            ("e".to_string(), "ERR-TWO".to_string()),
            ("o".to_string(), "OUT-THREE".to_string()),
        ],
        "order and stream tags: {entries:?}"
    );
    assert!(
        text_of(&entries, "o").join("").contains("STDIN-BEGIN"),
        "the whole output, not only the end"
    );
    assert!(
        entries.iter().all(|e| e["s"] != "x"),
        "a successful exit adds no note"
    );
    assert!(entries.iter().all(|e| e["t"].is_u64()));

    // What the database says about the file.
    let size = std::fs::metadata(server.file(id)).unwrap().len();
    assert_eq!(run["transcript_bytes"], size);
    assert_eq!(run["transcript_truncated"], false);
    let finished = server
        .owner("GET", "/events?kind_prefix=run.finished", None)
        .await;
    assert_eq!(
        finished["events"][0]["payload"]["transcript_bytes"], size,
        "{finished}"
    );
    let chunks = finished.to_string();
    assert!(!chunks.contains("OUT-ONE"), "no output in an event");
    server.assert_checks();

    // The plain text download.
    let text = server
        .api(
            "GET",
            &format!("/runs/{id}/transcript.txt?download=1"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(text.status, 200);
    assert!(
        text.head
            .contains("content-type: text/plain; charset=utf-8")
    );
    assert!(text.head.contains(&format!(
        "content-disposition: attachment; filename=\"run-{id}.txt\""
    )));
    let one = text.body.find("OUT-ONE").unwrap();
    let two = text.body.find("ERR-TWO").unwrap();
    let three = text.body.find("OUT-THREE").unwrap();
    assert!(one < two && two < three, "{}", text.body);
    let inline = server
        .api(
            "GET",
            &format!("/runs/{id}/transcript.txt"),
            Some(&server.owner),
            None,
        )
        .await;
    assert!(!inline.head.contains("content-disposition"));
    assert_eq!(inline.body, text.body);
}

#[tokio::test]
async fn polling_while_the_run_goes_on_returns_a_growing_offset_and_done_only_at_the_end() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server
        .agent(&["--line-ms", "300", "--print-lines", "8"])
        .await;
    server.assign(1, user).await;
    let run = server.wait_run(false).await;
    let id = run["id"].as_i64().unwrap();

    let mut next = 0;
    let mut nexts = Vec::new();
    let mut lines = Vec::new();
    let mut saw_partial = false;
    let started = Instant::now();
    loop {
        let reply = server.transcript(id, &format!("?after={next}")).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert!(reply.head.contains("no-store"));
        let page = reply.json();
        let got: Vec<String> = text_of(page["entries"].as_array().unwrap(), "o");
        let new_next = page["next"].as_u64().unwrap();
        assert!(new_next >= next);
        if page["done"] == false && !got.is_empty() {
            saw_partial = true;
        }
        lines.extend(got);
        if new_next > next {
            nexts.push(new_next);
        }
        next = new_next;
        if page["done"] == true {
            assert_eq!(page["status"], "succeeded");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(saw_partial, "entries arrived while the run was still going");
    assert!(nexts.len() >= 3, "the offset grew in steps: {nexts:?}");
    assert!(nexts.windows(2).all(|w| w[0] < w[1]));
    let numbered: Vec<&String> = lines.iter().filter(|l| l.starts_with("LINE ")).collect();
    let expected: Vec<String> = (1..=8).map(|n| format!("LINE {n}")).collect();
    assert_eq!(
        numbered.iter().map(|l| l.trim_end()).collect::<Vec<_>>(),
        expected,
        "every line once, in order"
    );

    // After the end a read from the end is empty and done.
    let end = server
        .transcript(id, &format!("?after={next}"))
        .await
        .json();
    assert_eq!(end["entries"].as_array().unwrap().len(), 0);
    assert_eq!(end["done"], true);
    assert_eq!(end["next"], next);
    // A small limit still returns whole lines only.
    let small = server.transcript(id, "?after=0&limit=1").await.json();
    assert_eq!(small["entries"].as_array().unwrap().len(), 1);
    assert_eq!(small["done"], false);
}

#[tokio::test]
async fn the_cap_truncates_the_file_and_the_run_still_succeeds() {
    let server = Server::start_with(options(), Some(4096)).await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&["--flood", "300000"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(true).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["transcript_truncated"], true);
    let id = run["id"].as_i64().unwrap();
    let bytes = run["transcript_bytes"].as_u64().unwrap();
    assert!(
        (4096 - 200..=4096 + 200).contains(&bytes),
        "the cap plus the one note: {bytes}"
    );
    let entries = server.entries(id).await;
    let notes: Vec<&Value> = entries.iter().filter(|e| e["s"] == "x").collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["d"], "transcript truncated at 4096 bytes");
    assert!(
        text_of(&entries, "o").join("").contains("STDIN-BEGIN"),
        "the start is kept"
    );
    let page = server.transcript(id, "").await.json();
    assert_eq!(page["truncated"], true);
    assert!(
        run["output_tail"].as_str().unwrap().contains("TAIL-MARKER"),
        "the tail on the row is as before"
    );
    server.assert_checks();
}

#[tokio::test]
async fn the_key_and_the_repository_path_are_scrubbed_from_the_file() {
    let repo = make_repo();
    let repo_text = repo.path().display().to_string();
    let server = Server::start().await;
    server.project("AA").await;
    server
        .owner(
            "PATCH",
            "/projects/AA",
            Some(json!({"repo_path": repo_text, "base_branch": "main"})),
        )
        .await;
    let (_, user, _) = server
        .agent(&["--echo-key", "--say", &format!("at {repo_text}")])
        .await;
    server.assign(1, user).await;
    let run = server.wait_run(true).await;
    let id = run["id"].as_i64().unwrap();
    assert_eq!(run["status"], "succeeded", "{run}");
    let entries = server.entries(id).await;
    let out = text_of(&entries, "o");
    assert!(out.iter().any(|l| l == "KEY=[key]\n"), "{out:?}");
    assert!(out.iter().any(|l| l == "at the repository\n"), "{out:?}");
    let raw = std::fs::read_to_string(server.file(id)).unwrap();
    assert!(!raw.contains("aur_"), "no key-shaped text in the file");
    assert!(
        !raw.contains(&repo_text.replace('\\', "\\\\")) && !raw.contains(&repo_text),
        "no repository path in the file"
    );
}

#[tokio::test]
async fn every_end_the_runner_sees_leaves_its_note() {
    // A failing exit.
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&["--exit", "3"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(true).await;
    let entries = server.entries(run["id"].as_i64().unwrap()).await;
    assert_eq!(entries.last().unwrap()["s"], "x");
    assert_eq!(entries.last().unwrap()["d"], "process exited with code 3");
    drop(server);

    // A timeout.
    let server = Server::start_with(
        RunnerOptions {
            timeout_override: Some(Duration::from_secs(1)),
            ..options()
        },
        None,
    )
    .await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&["--sleep", "60"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(true).await;
    assert_eq!(run["status"], "failed");
    let entries = server.entries(run["id"].as_i64().unwrap()).await;
    assert_eq!(entries.last().unwrap()["d"], "run timed out");
    assert!(run["transcript_bytes"].as_u64().unwrap() > 0);
    server.assert_checks();
    drop(server);

    // A shutdown.
    let mut server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server.agent(&["--sleep", "60"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(false).await;
    let id = run["id"].as_i64().unwrap();
    server.shutdown().await;
    let text = std::fs::read_to_string(server.file(id)).unwrap();
    let last = text.lines().last().unwrap();
    let last: Value = serde_json::from_str(last).unwrap();
    assert_eq!(
        (last["s"].as_str(), last["d"].as_str()),
        (Some("x"), Some("server stopped"))
    );
    let conn = aurora_store::rusqlite::Connection::open(&server.db).unwrap();
    let bytes: i64 = conn
        .query_row(
            "SELECT transcript_bytes FROM run WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bytes as u64,
        std::fs::metadata(server.file(id)).unwrap().len()
    );
    server.assert_checks();
}

// ---- cancel ----

#[tokio::test]
async fn cancel_ends_the_process_tree_keeps_the_transcript_and_answers_with_the_settled_run() {
    let repo = make_repo();
    let server = Server::start().await;
    server.project("AA").await;
    server
        .owner(
            "PATCH",
            "/projects/AA",
            Some(json!({"repo_path": repo.path().display().to_string(), "base_branch": "main"})),
        )
        .await;
    let (_, user, agent_key) = server
        .agent(&[
            "--spawn-child",
            "60",
            "--line-ms",
            "100",
            "--print-lines",
            "3",
            "--sleep",
            "60",
        ])
        .await;
    server.assign(1, user).await;
    let run = server.wait_run(false).await;
    let id = run["id"].as_i64().unwrap();
    let agent_pid = run["pid"].as_u64().unwrap();

    // Wait for the child to exist and the lines to be readable while running.
    let started = Instant::now();
    let child_pid = loop {
        let page = server.transcript(id, "").await.json();
        let out = text_of(page["entries"].as_array().unwrap(), "o");
        if out.iter().any(|l| l.starts_with("LINE 3")) {
            let line = out.iter().find(|l| l.starts_with("CHILD-PID: ")).unwrap();
            break line["CHILD-PID: ".len()..].trim().parse::<u64>().unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        alive(agent_pid) && alive(child_pid),
        "both run before the cancel"
    );
    let mid = server.transcript(id, "").await.json();
    assert_eq!(mid["done"], false, "a running run is not done");

    let denied = server
        .api(
            "POST",
            &format!("/runs/{id}/cancel"),
            Some(&agent_key),
            None,
        )
        .await;
    assert_eq!(denied.status, 403);

    let path = format!("/api/v1/runs/{id}/cancel");
    let header = "Idempotency-Key: cancel-run-0001\r\n";
    let began = Instant::now();
    let done = server
        .raw("POST", &path, Some(&server.owner), header, "")
        .await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert!(began.elapsed() < Duration::from_secs(8));
    let settled = done.json();
    assert_eq!(settled["status"], "canceled");
    assert_eq!(
        settled["worktree_state"], "removed",
        "the answer is the settled run, not `created`: {settled}"
    );
    assert!(
        settled["transcript_bytes"].as_u64().unwrap() > 0,
        "{settled}"
    );
    assert!(settled["output_tail"].as_str().unwrap().contains("LINE 3"));

    assert!(gone(agent_pid).await, "the agent is gone");
    assert!(
        gone(child_pid).await,
        "the agent's child is gone too (the tree)"
    );
    assert!(server.key_revoked(id));

    // Everything up to the kill is there, then the note; the reader is done.
    let entries = server.entries(id).await;
    let out = text_of(&entries, "o");
    assert!(out.iter().any(|l| l == "LINE 1\n") && out.iter().any(|l| l == "LINE 3\n"));
    let last = entries.last().unwrap();
    assert_eq!(
        (last["s"].as_str(), last["d"].as_str()),
        (Some("x"), Some("run canceled"))
    );
    let size = std::fs::metadata(server.file(id)).unwrap().len();
    assert_eq!(settled["transcript_bytes"], size);
    server.assert_checks();

    // A repeat of the same request is replayed; a new request is a conflict.
    let again = server
        .raw("POST", &path, Some(&server.owner), header, "")
        .await;
    assert_eq!(again.status, 200);
    assert!(again.head.contains("idempotent-replay: true"));
    assert_eq!(again.json()["id"], settled["id"]);
    assert_eq!(again.json()["status"], "canceled");
    let conflict = server
        .api(
            "POST",
            &format!("/runs/{id}/cancel"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(conflict.status, 409, "{}", conflict.body);
    let finished = server
        .owner("GET", "/events?kind_prefix=run.finished", None)
        .await;
    assert_eq!(
        finished["events"].as_array().unwrap().len(),
        1,
        "one finish event"
    );
    assert!(
        finished["events"][0]["payload"]
            .get("transcript_bytes")
            .is_none(),
        "unknown when the cancel committed"
    );
    assert_eq!(
        server.owner("GET", "/runs?status=running", None).await,
        json!([])
    );
}

#[tokio::test]
async fn a_member_may_cancel_and_the_page_buttons_follow_the_run() {
    let server = Server::start().await;
    server.project("AA").await;
    let (_, user, _) = server
        .agent(&["--say", "<script>alert(1)</script>", "--sleep", "60"])
        .await;
    server.assign(1, user).await;
    let run = server.wait_run(false).await;
    let id = run["id"].as_i64().unwrap();

    let page = server.page(&format!("/p/AA/runs/{id}")).await;
    assert_eq!(page.status, 200);
    for needle in [
        "id=\"run-output\"",
        "data-role=\"view\"",
        "data-role=\"follow\"",
        "data-role=\"more\"",
        &format!("href=\"/api/v1/runs/{id}/transcript.txt?download=1\""),
        "data-action=\"cancel-run\"",
        "data-active=\"true\"",
    ] {
        assert!(page.body.contains(needle), "{needle}\n{}", page.body);
    }
    assert!(!page.body.contains("<script>alert"), "escaped in the page");

    let issue = server.page("/p/AA/1").await;
    assert!(issue.body.contains("data-live-run=\""), "{}", issue.body);
    assert!(issue.body.contains("id=\"issue-output\""));
    assert!(
        issue.body.matches("data-action=\"cancel-run\"").count() == 1,
        "the Runs section has a Cancel button for the active run"
    );
    let fragment = server.page("/p/AA/1/runs").await;
    assert!(fragment.body.contains("data-action=\"cancel-run\""));

    let done = server
        .api(
            "POST",
            &format!("/runs/{id}/cancel"),
            Some(MEMBER_KEY),
            None,
        )
        .await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.json()["status"], "canceled");

    let page = server.page(&format!("/p/AA/runs/{id}")).await;
    assert!(!page.body.contains("cancel-run"), "no button once finished");
    assert!(page.body.contains("data-active=\"false\""));
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    let issue = server.page("/p/AA/1").await;
    assert!(!issue.body.contains("cancel-run"));
    assert!(!issue.body.contains("data-live-run"));
}

#[test]
fn the_script_puts_output_in_with_text_nodes_only() {
    let js = include_str!("../assets/app.js");
    let start = js
        .find("// ---- output: the transcript of a run ----")
        .unwrap();
    let end = js.find("function syncIssueOutput").unwrap();
    let part = &js[start..end];
    for forbidden in [
        "innerHTML",
        "insertAdjacentHTML",
        "outerHTML",
        "document.write",
        "eval(",
    ] {
        assert!(!part.contains(forbidden), "{forbidden} in the output code");
    }
    assert!(part.contains("textContent"));
}

// ---- who may read ----

#[tokio::test]
async fn reading_a_transcript_needs_an_owner_or_member_and_a_run_with_one() {
    let server = Server::start().await;
    server.project("AA").await;
    server.project("BB").await;
    let (agent, user, agent_key) = server.agent(&["--say", "hello"]).await;
    server.assign(1, user).await;
    let run = server.wait_run(true).await;
    let id = run["id"].as_i64().unwrap();
    let url = format!("/runs/{id}/transcript");

    assert_eq!(server.api("GET", &url, None, None).await.status, 401);
    assert_eq!(
        server
            .api("GET", &format!("{url}.txt"), None, None)
            .await
            .status,
        401
    );
    assert_eq!(
        server.api("GET", &url, Some(MEMBER_KEY), None).await.status,
        200
    );
    assert_eq!(
        server.api("GET", &url, Some(&agent_key), None).await.status,
        403,
        "output can hold secrets: a registered agent's key never reads it"
    );
    assert_eq!(
        server
            .api("GET", &format!("{url}.txt"), Some(&agent_key), None)
            .await
            .status,
        403
    );
    assert_eq!(
        server
            .api("GET", "/runs/9999/transcript", Some(&server.owner), None)
            .await
            .status,
        404
    );
    assert_eq!(
        server
            .api(
                "GET",
                "/runs/9999/transcript.txt",
                Some(&server.owner),
                None
            )
            .await
            .status,
        404
    );
    // An agent restricted to another project.
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"projects": ["BB"]})),
        )
        .await;
    assert_eq!(
        server.api("GET", &url, Some(&agent_key), None).await.status,
        403
    );
    // Bad queries.
    for query in ["?after=-1", "?after=x", "?limit=abc", "?nope=1"] {
        let reply = server.transcript(id, query).await;
        assert!(
            (400..500).contains(&reply.status) && reply.status != 404,
            "{query}: {} {}",
            reply.status,
            reply.body
        );
    }
    // An offset past the end, and a limit above the largest, are clamped.
    let past = server
        .transcript(id, "?after=99999999&limit=99999999")
        .await;
    assert_eq!(past.status, 200);
    assert_eq!(past.json()["entries"], json!([]));
    assert_eq!(past.json()["done"], true);

    // A running run that has printed nothing (its file is not there) is empty,
    // not 404.
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"projects": ["AA", "BB"], "args": ["--sleep", "60"]})),
        )
        .await;
    let second = server
        .owner(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(json!({"issue": "BB-1"})),
        )
        .await;
    let second_id = second["id"].as_i64().unwrap();
    let started = Instant::now();
    while !server.file(second_id).exists() {
        assert!(started.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::remove_file(server.file(second_id)).unwrap_or_default();
    let empty = server.transcript(second_id, "").await;
    assert_eq!(empty.status, 200, "{}", empty.body);
    assert_eq!(empty.json()["entries"], json!([]));
    assert_eq!(empty.json()["done"], false);
    let text = server
        .api(
            "GET",
            &format!("/runs/{second_id}/transcript.txt"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!((text.status, text.body.as_str()), (200, ""));
    server
        .owner("POST", &format!("/runs/{second_id}/cancel"), None)
        .await;

    // A finished run that never had a process has no transcript: 404.
    server
        .owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"command": "definitely-not-a-command-xyz"})),
        )
        .await;
    let third = server
        .owner(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(json!({"issue": "BB-1"})),
        )
        .await;
    let third_id = third["id"].as_i64().unwrap();
    let started = Instant::now();
    loop {
        let run = server
            .owner("GET", &format!("/runs/{third_id}"), None)
            .await;
        if run["status"] == "failed" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let none = server.transcript(third_id, "").await;
    assert_eq!(none.status, 404, "{}", none.body);
    let none = server
        .api(
            "GET",
            &format!("/runs/{third_id}/transcript.txt"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(none.status, 404);
}
