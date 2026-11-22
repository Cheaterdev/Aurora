//! A working copy per run (TRK-16), with real git in temporary repositories
//! (never inside this repository) and the `fake_agent` example as the agent.
//! Needs `git` on PATH; without it every test here fails with a clear message.

use std::path::{Path, PathBuf};
use std::process::Command;
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

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git must be installed and on PATH for these tests: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with one commit on `main`, in a temporary folder of its own.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "tests@example.invalid"]);
        git(path, &["config", "user.name", "Tests"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        git(path, &["add", "-A"]);
        git(path, &["commit", "-q", "-m", "first"]);
        Repo { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn branches(&self) -> Vec<String> {
        git(self.path(), &["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn worktrees(&self) -> usize {
        git(self.path(), &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }

    fn run_branches(&self) -> Vec<String> {
        self.branches()
            .into_iter()
            .filter(|b| b.starts_with("aurora/"))
            .collect()
    }
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
        Server::start_with(options()).await
    }

    async fn start_with(options: RunnerOptions) -> Server {
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
        let config = RunnerConfig::for_server(&store, addr, options);
        let runner = Some(aurora::Runner::start(store.clone(), config).await.unwrap());
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

    async fn page(&self, path: &str, key: &str) -> Reply {
        self.raw("GET", path, Some(key), "X-Requested-With: aurora\r\n", "")
            .await
    }

    /// Project AA with a type and `issues` issues; the repository is set when given.
    async fn project(&self, issues: usize, repo: Option<&Repo>) {
        self.owner(
            "POST",
            "/projects",
            Some(json!({"key": "AA", "name": "Project AA"})),
        )
        .await;
        self.owner(
            "POST",
            "/projects/AA/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await;
        for n in 1..=issues {
            self.owner(
                "POST",
                "/projects/AA/issues",
                Some(json!({"title": format!("Issue {n}")})),
            )
            .await;
        }
        if let Some(repo) = repo {
            self.owner(
                "PATCH",
                "/projects/AA",
                Some(
                    json!({"repo_path": repo.path().display().to_string(), "base_branch": "main"}),
                ),
            )
            .await;
        }
    }

    /// Registers `bot`; returns (agent id, user id, key).
    async fn agent(&self, args: &[&str], extra: Value) -> (i64, i64, String) {
        let mut body = json!({"name": "bot", "command": fake_agent().display().to_string(),
                              "args": args, "projects": ["AA"], "create_key": true});
        if let (Some(into), Some(from)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in from {
                into.insert(k.clone(), v.clone());
            }
        }
        let made = self.owner("POST", "/agents", Some(body)).await;
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

    /// Waits until `count` runs exist and are all over; returns their details, oldest first.
    async fn finished(&self, count: usize) -> Vec<Value> {
        let started = Instant::now();
        loop {
            let runs = self.runs().await;
            let over = runs
                .iter()
                .all(|r| !matches!(r["status"].as_str(), Some("starting" | "running")));
            if runs.len() == count && over {
                let mut details = Vec::new();
                for run in runs.iter().rev() {
                    details.push(
                        self.owner("GET", &format!("/runs/{}", run["id"]), None)
                            .await,
                    );
                }
                return details;
            }
            assert!(
                started.elapsed() < Duration::from_secs(40),
                "runs never finished: {runs:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn one_finished(&self) -> Value {
        self.finished(1).await.remove(0)
    }

    async fn running(&self) -> Value {
        let started = Instant::now();
        loop {
            if let Some(run) = self.runs().await.first()
                && run["status"] == "running"
            {
                return self
                    .owner("GET", &format!("/runs/{}", run["id"]), None)
                    .await;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "never running");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits for a run's working copy to leave `created` (a cancel settles it later).
    async fn settled(&self, id: i64) -> Value {
        let started = Instant::now();
        loop {
            let run = self.owner("GET", &format!("/runs/{id}"), None).await;
            if run["worktree_state"] != "created" {
                return run;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "not settled: {run}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn key_revoked(&self, id: i64) -> (bool, i64) {
        let conn = aurora_store::rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(
            "SELECT COALESCE(MAX(revoked IS NOT NULL), 0), COUNT(*) FROM api_key WHERE name = ?1",
            [format!("run-{id}")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn worktree_dir(&self, id: i64) -> PathBuf {
        self.data.join("worktrees").join(id.to_string())
    }
}

fn tail(run: &Value) -> String {
    run["output_tail"].as_str().unwrap_or_default().to_string()
}

// ---- the happy paths ----

#[tokio::test]
async fn a_run_without_changes_leaves_nothing_behind() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server.agent(&["--print-cwd"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();

    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["worktree_state"], "removed", "{run}");
    assert_eq!(run["has_changes"], false);
    assert_eq!(run["changed_files"], 0);
    assert_eq!(run["ahead"], 0);
    assert_eq!(run["branch"], format!("aurora/AA-1-run{id}"));
    assert_eq!(run["base_ref"], "main");
    assert_eq!(
        run["base_commit"].as_str().unwrap(),
        git(repo.path(), &["rev-parse", "main"])
    );
    let path = PathBuf::from(run["worktree_path"].as_str().unwrap());
    assert_eq!(path, server.worktree_dir(id));
    assert!(!path.exists(), "the folder is gone");
    assert!(repo.run_branches().is_empty(), "{:?}", repo.branches());
    assert_eq!(repo.worktrees(), 1, "only the repository itself");
    assert_eq!(server.key_revoked(id), (true, 1));
    // The agent ran in the worktree, which was a real checkout of main.
    let output = tail(&run);
    let cwd = output
        .lines()
        .find_map(|l| l.strip_prefix("PRINT-CWD: "))
        .unwrap();
    assert!(
        Path::new(cwd).ends_with(Path::new("worktrees").join(id.to_string())),
        "{cwd}"
    );
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn an_untracked_file_keeps_the_working_copy() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server.agent(&["--touch", "notes.txt"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();

    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["worktree_state"], "kept");
    assert_eq!(run["has_changes"], true);
    assert_eq!(run["changed_files"], 1);
    assert_eq!(run["ahead"], 0);
    let path = server.worktree_dir(id);
    assert!(path.join("notes.txt").is_file(), "the work is still there");
    assert!(
        repo.run_branches()
            .contains(&format!("aurora/AA-1-run{id}"))
    );
    assert_eq!(repo.worktrees(), 2);
    assert_eq!(server.key_revoked(id), (true, 1), "the key still dies");

    // It is listed by the filter, and a clean run is not.
    let listed = server.owner("GET", "/runs?has_changes=true", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let none = server.owner("GET", "/runs?has_changes=false", None).await;
    assert!(none.as_array().unwrap().is_empty());
    let bad = server
        .api("GET", "/runs?has_changes=maybe", Some(&server.owner), None)
        .await;
    assert_eq!(bad.status, 422, "{}", bad.body);
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn a_commit_keeps_the_working_copy_and_counts_it() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server
        .agent(&["--touch", "feature.txt", "--commit"], json!({}))
        .await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();

    assert_eq!(run["worktree_state"], "kept", "{run}");
    assert_eq!(run["has_changes"], true);
    assert_eq!(run["ahead"], 1);
    assert_eq!(run["changed_files"], 0, "committed, so the tree is clean");
    let branch = format!("aurora/AA-1-run{id}");
    assert_eq!(
        git(
            repo.path(),
            &["rev-list", "--count", &format!("main..{branch}")]
        ),
        "1"
    );
    assert!(server.worktree_dir(id).join("feature.txt").is_file());
    assert_eq!(
        git(repo.path(), &["rev-parse", "main"]),
        run["base_commit"].as_str().unwrap(),
        "main was not touched"
    );
}

#[tokio::test]
async fn two_runs_at_once_get_two_working_copies() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(2, Some(&repo)).await;
    let (_, user, _) = server
        .agent(
            &["--print-cwd", "--sleep", "1", "--touch", "mine.txt"],
            json!({"max_concurrent": 2}),
        )
        .await;
    server.assign(1, user).await;
    server.assign(2, user).await;
    let runs = server.finished(2).await;

    let paths: Vec<&str> = runs
        .iter()
        .map(|r| r["worktree_path"].as_str().unwrap())
        .collect();
    let branches: Vec<&str> = runs.iter().map(|r| r["branch"].as_str().unwrap()).collect();
    assert_ne!(paths[0], paths[1]);
    assert_ne!(branches[0], branches[1]);
    for run in &runs {
        assert_eq!(run["status"], "succeeded", "{run}");
        assert_eq!(run["worktree_state"], "kept");
        assert_eq!(run["changed_files"], 1, "each wrote only its own file");
    }
    assert_eq!(repo.worktrees(), 3);
    assert_eq!(repo.run_branches().len(), 2);
}

// ---- no isolation ----

#[tokio::test]
async fn an_agent_with_its_own_work_dir_gets_no_working_copy() {
    let repo = Repo::new();
    let fixed = tempfile::tempdir().unwrap();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (agent, user, _) = server
        .agent(
            &["--print-cwd"],
            json!({"work_dir": fixed.path().display().to_string()}),
        )
        .await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["worktree_state"], "none");
    assert!(run["branch"].is_null() && run["worktree_path"].is_null());
    assert_eq!(repo.worktrees(), 1);
    assert!(repo.run_branches().is_empty());
    let output = tail(&run);
    let cwd = output
        .lines()
        .find_map(|l| l.strip_prefix("PRINT-CWD: "))
        .unwrap();
    assert_eq!(
        std::fs::canonicalize(cwd).unwrap(),
        std::fs::canonicalize(fixed.path()).unwrap()
    );
    assert!(!output.contains("AURORA_BRANCH=aurora/"), "{output}");

    // A branch override means nothing without isolation.
    let refused = server
        .api(
            "POST",
            &format!("/agents/{agent}/runs"),
            Some(&server.owner),
            Some(json!({"issue": "AA-1", "base_branch": "main"})),
        )
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
}

#[tokio::test]
async fn a_project_without_a_repository_behaves_as_before() {
    let server = Server::start().await;
    server.project(1, None).await;
    let (_, user, _) = server.agent(&["--print-cwd"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["worktree_state"], "none");
    assert!(run["branch"].is_null() && run["worktree_path"].is_null());
    assert!(!server.data.join("worktrees").exists());
    let output = tail(&run);
    let cwd = output
        .lines()
        .find_map(|l| l.strip_prefix("PRINT-CWD: "))
        .unwrap();
    assert!(
        Path::new(cwd).ends_with(Path::new("runs").join(id.to_string()).join("work")),
        "{cwd}"
    );
    let refused = server
        .api(
            "POST",
            &format!("/agents/{}/runs", 1),
            Some(&server.owner),
            Some(json!({"issue": "AA-1", "base_branch": "main"})),
        )
        .await;
    assert_eq!(refused.status, 422, "{}", refused.body);
}

// ---- the prompt and the environment ----

#[tokio::test]
async fn the_prompt_and_the_environment_name_the_real_folder_and_branch() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server.agent(&["--touch", "x"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();
    let path = run["worktree_path"].as_str().unwrap();
    let branch = run["branch"].as_str().unwrap();
    let message = run["message"].as_str().unwrap();
    assert!(message.contains(path), "{message}");
    assert!(message.contains(branch), "{message}");
    let output = tail(&run);
    assert!(
        output.contains(&format!("AURORA_WORKDIR={path}")),
        "{output}"
    );
    assert!(
        output.contains(&format!("AURORA_BRANCH={branch}")),
        "{output}"
    );
    assert!(
        output.contains("ENV: ")
            && output.contains("AURORA_BRANCH")
            && output.contains("AURORA_WORKDIR"),
        "{output}"
    );
    assert_eq!(branch, format!("aurora/AA-1-run{id}"));
}

// ---- failures ----

#[tokio::test]
async fn a_missing_base_branch_fails_the_run_and_leaves_nothing() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    // The branch was valid when it was saved; now it is gone.
    git(repo.path(), &["checkout", "-q", "-b", "side"]);
    git(repo.path(), &["branch", "-q", "-D", "main"]);
    let (_, user, _) = server.agent(&[], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();
    assert_eq!(run["status"], "failed", "{run}");
    let error = run["error"].as_str().unwrap();
    assert!(error.starts_with("could not create worktree: "), "{error}");
    assert!(
        !error.contains(&repo.path().display().to_string()),
        "the repository's folder is not in the error: {error}"
    );
    assert_eq!(run["worktree_state"], "none");
    assert_eq!(server.key_revoked(id), (true, 1));
    assert!(!server.worktree_dir(id).exists());
    assert!(repo.run_branches().is_empty());
    assert_eq!(repo.worktrees(), 1);
    assert!(run["exit_code"].is_null(), "the process never started");
    let report = aurora_store::invariants::check_file(&server.db).unwrap();
    assert!(report.ok(), "{:#?}", report.failed());
}

#[tokio::test]
async fn a_base_branch_override_is_checked_and_used() {
    let repo = Repo::new();
    git(repo.path(), &["branch", "release"]);
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (agent, _, _) = server.agent(&["--touch", "f"], json!({})).await;
    let path = format!("/agents/{agent}/runs");
    let unknown = server
        .api(
            "POST",
            &path,
            Some(&server.owner),
            Some(json!({"issue": "AA-1", "base_branch": "nope"})),
        )
        .await;
    assert_eq!(unknown.status, 422, "{}", unknown.body);
    let evil = server
        .api(
            "POST",
            &path,
            Some(&server.owner),
            Some(json!({"issue": "AA-1", "base_branch": "--upload-pack=x"})),
        )
        .await;
    assert_eq!(evil.status, 422, "{}", evil.body);
    assert!(server.runs().await.is_empty());

    let made = server
        .owner(
            "POST",
            &path,
            Some(json!({"issue": "AA-1", "base_branch": "release"})),
        )
        .await;
    assert_eq!(made["base_ref"], "release", "{made}");
    let run = server.one_finished().await;
    assert_eq!(run["base_ref"], "release");
    assert_eq!(
        run["base_commit"].as_str().unwrap(),
        git(repo.path(), &["rev-parse", "release"])
    );
    assert_eq!(run["worktree_state"], "kept");
}

// ---- every way a run can end ----

#[tokio::test]
async fn cancel_timeout_and_shutdown_clean_up_the_same_way() {
    // Cancel without changes: removed.
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server.agent(&["--sleep", "30"], json!({})).await;
    server.assign(1, user).await;
    let run = server.running().await;
    let id = run["id"].as_i64().unwrap();
    assert!(
        server.worktree_dir(id).is_dir(),
        "the process runs inside it"
    );
    let done = server
        .api(
            "POST",
            &format!("/runs/{id}/cancel"),
            Some(&server.owner),
            None,
        )
        .await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert_eq!(done.json()["status"], "canceled");
    let settled = server.settled(id).await;
    assert_eq!(settled["worktree_state"], "removed", "{settled}");
    assert!(!server.worktree_dir(id).exists());
    assert!(repo.run_branches().is_empty());
    assert_eq!(repo.worktrees(), 1);
    drop(server);

    // Cancel after the agent wrote something: kept.
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server
        .agent(&["--touch", "half.txt", "--sleep", "30"], json!({}))
        .await;
    server.assign(1, user).await;
    let run = server.running().await;
    let id = run["id"].as_i64().unwrap();
    let started = Instant::now();
    while !server.worktree_dir(id).join("half.txt").exists() {
        assert!(started.elapsed() < Duration::from_secs(20), "no file");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server
        .owner("POST", &format!("/runs/{id}/cancel"), None)
        .await;
    let settled = server.settled(id).await;
    assert_eq!(settled["status"], "canceled");
    assert_eq!(settled["worktree_state"], "kept", "{settled}");
    assert_eq!(settled["has_changes"], true);
    assert!(server.worktree_dir(id).join("half.txt").is_file());
    drop(server);

    // A timeout is a failure, and the empty copy goes.
    let repo = Repo::new();
    let server = Server::start_with(RunnerOptions {
        timeout_override: Some(Duration::from_secs(1)),
        ..options()
    })
    .await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server.agent(&["--sleep", "60"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    assert_eq!(run["status"], "failed");
    assert_eq!(run["error"], "timed out after 1 s");
    assert_eq!(run["worktree_state"], "removed", "{run}");
    assert!(repo.run_branches().is_empty());
    drop(server);

    // A shutdown interrupts the run; an untouched copy is removed, a changed one kept.
    for (args, state, expect_dir) in [
        (vec!["--sleep", "60"], "removed", false),
        (vec!["--touch", "w.txt", "--sleep", "60"], "kept", true),
    ] {
        let repo = Repo::new();
        let mut server = Server::start().await;
        server.project(1, Some(&repo)).await;
        let (_, user, _) = server.agent(&args, json!({})).await;
        server.assign(1, user).await;
        let run = server.running().await;
        let id = run["id"].as_i64().unwrap();
        if expect_dir {
            let started = Instant::now();
            while !server.worktree_dir(id).join("w.txt").exists() {
                assert!(started.elapsed() < Duration::from_secs(20), "no file");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        server.shutdown().await;
        let conn = aurora_store::rusqlite::Connection::open(&server.db).unwrap();
        let (status, worktree): (String, String) = conn
            .query_row(
                "SELECT status, worktree_state FROM run WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((status.as_str(), worktree.as_str()), ("interrupted", state));
        assert_eq!(server.worktree_dir(id).exists(), expect_dir);
        assert_eq!(repo.run_branches().len(), usize::from(expect_dir));
        let report = aurora_store::invariants::check_file(&server.db).unwrap();
        assert!(report.ok(), "{:#?}", report.failed());
    }
}

// ---- the project's repository settings ----

#[tokio::test]
async fn the_repository_settings_are_the_owners_and_are_checked() {
    let repo = Repo::new();
    let plain = tempfile::tempdir().unwrap();
    let server = Server::start().await;
    server.project(1, None).await;
    let (_, _, agent_key) = server.agent(&[], json!({})).await;
    let path = repo.path().display().to_string();
    let body = || Some(json!({"repo_path": path, "base_branch": "main"}));

    let anonymous = server.api("PATCH", "/projects/AA", None, body()).await;
    assert_eq!(anonymous.status, 401);
    let member = server
        .api("PATCH", "/projects/AA", Some(MEMBER_KEY), body())
        .await;
    assert_eq!(member.status, 403, "{}", member.body);
    let by_agent = server
        .api("PATCH", "/projects/AA", Some(&agent_key), body())
        .await;
    assert_eq!(by_agent.status, 403, "{}", by_agent.body);
    let unknown = server
        .api("PATCH", "/projects/ZZ", Some(&server.owner), body())
        .await;
    assert_eq!(unknown.status, 404);
    // Renaming stays open to everyone who may write.
    let renamed = server
        .api(
            "PATCH",
            "/projects/AA",
            Some(MEMBER_KEY),
            Some(json!({"name": "Renamed"})),
        )
        .await;
    assert_eq!(renamed.status, 200, "{}", renamed.body);

    for (bad, why) in [
        (json!({"repo_path": "relative/dir"}), "absolute"),
        (
            json!({"repo_path": plain.path().display().to_string()}),
            "git",
        ),
        (
            json!({"repo_path": plain.path().join("missing").display().to_string()}),
            "exist",
        ),
        (json!({"repo_path": 5}), ""),
        (json!({"base_branch": "nope"}), "set repo_path"),
        (json!({"base_branch": "--bad"}), ""),
        (json!({}), ""),
    ] {
        let reply = server
            .api(
                "PATCH",
                "/projects/AA",
                Some(&server.owner),
                Some(bad.clone()),
            )
            .await;
        assert_eq!(reply.status, 422, "{bad}: {}", reply.body);
        assert!(reply.body.contains(why), "{bad}: {}", reply.body);
    }
    let still = server.owner("GET", "/projects/AA", None).await;
    assert!(still["repo_path"].is_null(), "{still}");

    let saved = server.owner("PATCH", "/projects/AA", body()).await;
    assert!(saved["repo_path"].is_string(), "{saved}");
    assert_eq!(saved["base_branch"], "main");
    let top = PathBuf::from(saved["repo_path"].as_str().unwrap());
    assert_eq!(
        std::fs::canonicalize(&top).unwrap(),
        std::fs::canonicalize(repo.path()).unwrap()
    );
    assert!(
        !saved["repo_path"].as_str().unwrap().starts_with(r"\\?\"),
        "{saved}"
    );
    let missing = server
        .api(
            "PATCH",
            "/projects/AA",
            Some(&server.owner),
            Some(json!({"base_branch": "nope"})),
        )
        .await;
    assert_eq!(missing.status, 422, "{}", missing.body);
    assert!(missing.body.contains("nope"), "{}", missing.body);
    // A subfolder is stored as the top-level folder.
    std::fs::create_dir(repo.path().join("sub")).unwrap();
    let sub = server
        .owner(
            "PATCH",
            "/projects/AA",
            Some(json!({"repo_path": repo.path().join("sub").display().to_string()})),
        )
        .await;
    assert_eq!(sub["repo_path"], saved["repo_path"]);

    // People see it; an agent never does, nor does the list for an agent.
    let member_view = server
        .api("GET", "/projects/AA", Some(MEMBER_KEY), None)
        .await
        .json();
    assert!(member_view["repo_path"].is_string());
    let agent_view = server
        .api("GET", "/projects/AA", Some(&agent_key), None)
        .await
        .json();
    assert!(agent_view.get("repo_path").is_none(), "{agent_view}");
    assert!(agent_view.get("base_branch").is_none());
    let listed = server
        .api("GET", "/projects", Some(&agent_key), None)
        .await
        .body;
    assert!(!listed.contains("repo_path"), "{listed}");

    // The log says only that it changed.
    let events = server
        .owner("GET", "/events?kind_prefix=project.updated&limit=100", None)
        .await;
    let text = events.to_string();
    assert!(!text.contains(path.as_str()), "{text}");
    assert!(text.contains("changed"), "{text}");

    // Both can be cleared.
    let cleared = server
        .owner(
            "PATCH",
            "/projects/AA",
            Some(json!({"repo_path": null, "base_branch": ""})),
        )
        .await;
    assert!(cleared["repo_path"].is_null() && cleared["base_branch"].is_null());
}

#[tokio::test]
async fn the_run_json_shows_branch_and_hides_the_folder_from_agents() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, agent_key) = server.agent(&["--touch", "k"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();
    server.owner("GET", &format!("/runs/{id}"), None).await["worktree_path"]
        .as_str()
        .unwrap();

    let seen = server
        .api("GET", &format!("/runs/{id}"), Some(&agent_key), None)
        .await
        .json();
    assert_eq!(seen["branch"], run["branch"]);
    assert_eq!(seen["worktree_state"], "kept");
    assert!(seen.get("worktree_path").is_none(), "{seen}");
    assert!(seen.get("worktree_note").is_none(), "{seen}");
    let listed = server
        .api("GET", "/runs?has_changes=true", Some(&agent_key), None)
        .await
        .json();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(listed[0].get("worktree_path").is_none());

    // Events carry the branch and the state, never a folder.
    let events = server
        .owner("GET", "/events?kind_prefix=run.&limit=100", None)
        .await
        .to_string();
    assert!(events.contains("worktree_state"), "{events}");
    assert!(
        !events.contains(&server.data.display().to_string().replace('\\', "\\\\")),
        "{events}"
    );
    assert!(!events.contains(&repo.path().display().to_string().replace('\\', "\\\\")));
}

// ---- pages ----

#[tokio::test]
async fn the_pages_show_the_working_copy_and_only_the_owner_sets_the_repository() {
    let repo = Repo::new();
    let server = Server::start().await;
    server.project(1, Some(&repo)).await;
    let (_, user, _) = server.agent(&["--touch", "k"], json!({})).await;
    server.assign(1, user).await;
    let run = server.one_finished().await;
    let id = run["id"].as_i64().unwrap();

    let board = server.page("/p/AA", &server.owner).await;
    assert_eq!(board.status, 200);
    assert!(board.body.contains("data-form=\"repo-settings\""));
    assert!(board.body.contains("name=\"repo_path\""));
    let member_board = server.page("/p/AA", MEMBER_KEY).await;
    assert_eq!(member_board.status, 200);
    assert!(
        !member_board.body.contains("repo-settings") && !member_board.body.contains("repo_path"),
        "only the owner gets the form"
    );

    let page = server
        .page(&format!("/p/AA/runs/{id}"), &server.owner)
        .await;
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains(run["branch"].as_str().unwrap()),
        "{}",
        page.body
    );
    assert!(page.body.contains("chip-worktree"));
    assert!(page.body.contains("worktree kept"));
    assert!(page.body.contains("Started from"));
    assert!(page.body.contains("1 changed files"));
    let issue = server.page("/p/AA/1", &server.owner).await;
    assert!(
        issue.body.contains(run["branch"].as_str().unwrap()),
        "{}",
        issue.body
    );
    assert!(issue.body.contains("chip-worktree"));

    // Whatever git or the runner said is escaped.
    let conn = aurora_store::rusqlite::Connection::open(&server.db).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute(
        "UPDATE run SET worktree_note = '<script>alert(1)</script>' WHERE id = ?1",
        [id],
    )
    .unwrap();
    let page = server
        .page(&format!("/p/AA/runs/{id}"), &server.owner)
        .await;
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!page.body.contains("<script>alert(1)"));
    assert!(page.head.contains("no-store"));
}
