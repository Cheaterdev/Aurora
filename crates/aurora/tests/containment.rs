//! Process containment and start-up reconciliation of runs (TRK-45), with the
//! real binary and the fake agent. The process tests are Windows-only (job
//! objects); everything they start is the server's own child or the agent that
//! server started, and is only ever killed by pid after its recorded start time
//! was checked.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;
use common::harness::{Server, assert_checks_green, bootstrap, count, request};

const READY: Duration = Duration::from_secs(60);

fn fake_agent() -> PathBuf {
    let name = format!("fake_agent{}", std::env::consts::EXE_SUFFIX);
    let path = Path::new(common::harness::BIN)
        .parent()
        .expect("a folder")
        .join("examples")
        .join(name);
    assert!(
        path.exists(),
        "build the examples first: {}",
        path.display()
    );
    path
}

fn call(addr: &str, key: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let reply = request(addr, key, method, path, body.as_ref(), None).expect("answered");
    let json = serde_json::from_str(reply.body.trim()).unwrap_or(Value::Null);
    (reply.status, json)
}

fn ok(addr: &str, key: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, json) = call(addr, key, method, path, body);
    assert!(
        (200..300).contains(&status),
        "{method} {path}: {status} {json}"
    );
    json
}

fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: u32) -> bool {
    aurora::runner::process_identity(pid).is_some()
}

/// Kills `pid` and its tree, but only if it is still the process created at
/// `started` (never a stranger that got the pid).
fn kill_verified(pid: u32, started: i64) {
    if aurora::runner::process_identity(pid) == Some(started) {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
    }
}

struct Setup {
    key: String,
    user: i64,
    agent_key: String,
}

/// Project AA with `issues` issues and an agent `bot` running the fake agent
/// with `args`.
fn setup(addr: &str, key: &str, args: &[&str], issues: usize, repo: Option<&Path>) -> Setup {
    ok(
        addr,
        key,
        "POST",
        "/api/v1/projects",
        Some(json!({"key": "AA", "name": "A"})),
    );
    ok(
        addr,
        key,
        "POST",
        "/api/v1/projects/AA/issue-types",
        Some(json!({"name": "Task"})),
    );
    if let Some(repo) = repo {
        ok(
            addr,
            key,
            "PATCH",
            "/api/v1/projects/AA",
            Some(json!({"repo_path": repo.display().to_string(), "base_branch": "main"})),
        );
    }
    for n in 0..issues {
        ok(
            addr,
            key,
            "POST",
            "/api/v1/projects/AA/issues",
            Some(json!({"title": format!("Issue {n}")})),
        );
    }
    let made = ok(
        addr,
        key,
        "POST",
        "/api/v1/agents",
        Some(
            json!({"name": "bot", "command": fake_agent().display().to_string(),
                    "args": args, "projects": ["AA"], "create_key": true, "max_concurrent": 4}),
        ),
    );
    Setup {
        key: key.to_string(),
        user: made["agent"]["user_id"].as_i64().expect("agent user"),
        agent_key: made["key"].as_str().expect("key").to_string(),
    }
}

fn assign(addr: &str, s: &Setup, number: i64) {
    ok(
        addr,
        &s.key,
        "PATCH",
        &format!("/api/v1/projects/AA/issues/{number}"),
        Some(json!({"assignee_id": s.user})),
    );
}

fn column(addr: &str, key: &str, state: &str) -> i64 {
    let columns = ok(addr, key, "GET", "/api/v1/projects/AA/columns", None);
    columns
        .as_array()
        .and_then(|list| list.iter().find(|c| c["state_type"] == state))
        .and_then(|c| c["id"].as_i64())
        .expect("a column of that kind")
}

fn move_card(addr: &str, key: &str, number: i64, column_id: i64) {
    ok(
        addr,
        key,
        "POST",
        &format!("/api/v1/projects/AA/issues/{number}/move"),
        Some(json!({"column_id": column_id})),
    );
}

/// The one run that is `running`, as `(id, pid)`.
fn running_run(addr: &str, key: &str) -> (i64, u32) {
    let mut found = None;
    wait_until("a running run", Duration::from_secs(30), || {
        let list = ok(addr, key, "GET", "/api/v1/runs?status=running", None);
        found = list
            .as_array()
            .and_then(|l| l.first())
            .and_then(|r| Some((r["id"].as_i64()?, u32::try_from(r["pid"].as_i64()?).ok()?)));
        found.is_some()
    });
    found.expect("a run")
}

/// The pid the fake agent printed for its `--spawn-child`.
fn child_pid(addr: &str, key: &str, run: i64) -> u32 {
    let mut pid = None;
    wait_until(
        "the child's pid in the transcript",
        Duration::from_secs(30),
        || {
            let page = ok(
                addr,
                key,
                "GET",
                &format!("/api/v1/runs/{run}/transcript"),
                None,
            );
            pid = page["entries"].as_array().and_then(|entries| {
                entries.iter().find_map(|e| {
                    e["d"]
                        .as_str()?
                        .strip_prefix("CHILD-PID: ")?
                        .trim()
                        .parse()
                        .ok()
                })
            });
            pid.is_some()
        },
    );
    pid.expect("a pid")
}

fn start(db: &Path, failpoint: Option<&str>, env: &[(&str, &str)]) -> Server {
    let mut server = Server::spawn_with(db, failpoint, env);
    server
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("server: {why}"));
    server
}

const LONG: &[&str] = &["--spawn-child", "120", "--say", "going", "--sleep", "120"];

// ---- Windows process tests ----

/// The core guarantee: kill only the server (no `/T`) and the OS closes the
/// job, which kills the agent and the grandchild it started at once. The
/// grandchild is started the moment the agent starts, so this also shows it
/// cannot be created before it is contained.
#[cfg(windows)]
#[test]
fn killing_the_server_kills_the_agent_and_its_grandchild() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let mut server = start(&db, None, &[]);
    let a = server.addr.clone();
    let s = setup(&a, &key, LONG, 1, None);
    assign(&a, &s, 1);
    let (run, agent) = running_run(&a, &key);
    let child = child_pid(&a, &key, run);
    assert!(alive(agent) && alive(child));

    server.kill();
    wait_until(
        "the agent and its grandchild to die",
        Duration::from_secs(10),
        || !alive(agent) && !alive(child),
    );
    assert_checks_green(&db, "after the kill");
}

#[cfg(windows)]
#[test]
fn cancel_ends_the_whole_tree_through_the_job() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let server = start(&db, None, &[]);
    let a = server.addr.clone();
    let s = setup(&a, &key, LONG, 1, None);
    assign(&a, &s, 1);
    let (run, agent) = running_run(&a, &key);
    let child = child_pid(&a, &key, run);

    let done = ok(
        &a,
        &key,
        "POST",
        &format!("/api/v1/runs/{run}/cancel"),
        None,
    );
    assert_eq!(done["status"], "canceled");
    wait_until("the tree to end", Duration::from_secs(10), || {
        !alive(agent) && !alive(child)
    });
    assert!(
        !server.log_text().contains("could not create a job object"),
        "{}",
        server.log_text()
    );
    // Cancel is visible to the user already: no extra comment, no card move.
    assert_eq!(count(&db, "SELECT COUNT(*) FROM comment"), 0);
    assert_checks_green(&db, "after the cancel");
}

/// With job objects "unavailable" the run still works, a warning says so, and
/// cancel falls back to `taskkill`.
#[cfg(windows)]
#[test]
fn without_a_job_the_run_works_warns_and_cancel_uses_taskkill() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let server = start(&db, None, &[("AURORA_TEST_NO_JOB", "1")]);
    let a = server.addr.clone();
    let s = setup(&a, &key, LONG, 1, None);
    assign(&a, &s, 1);
    let (run, agent) = running_run(&a, &key);
    let child = child_pid(&a, &key, run);
    assert!(
        server.log_text().contains("could not create a job object"),
        "{}",
        server.log_text()
    );

    let done = ok(
        &a,
        &key,
        "POST",
        &format!("/api/v1/runs/{run}/cancel"),
        None,
    );
    assert_eq!(done["status"], "canceled");
    wait_until("taskkill to end the tree", Duration::from_secs(15), || {
        !alive(agent) && !alive(child)
    });
    assert_checks_green(&db, "after the cancel");
}

/// A run whose agent escaped the job (here: it never had one): the next start
/// kills it and its child because the recorded start time matches, marks the
/// run interrupted with the note, comments once, moves the card out of
/// "In Progress", and a further start changes nothing.
#[cfg(windows)]
#[test]
fn start_up_kills_a_verified_leftover_and_explains_it_once() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let mut first = start(&db, None, &[("AURORA_TEST_NO_JOB", "1")]);
    let a = first.addr.clone();
    let s = setup(&a, &key, LONG, 1, None);
    let started_col = column(&a, &key, "started");
    move_card(&a, &key, 1, started_col);
    assign(&a, &s, 1);
    let (run, agent) = running_run(&a, &key);
    let child = child_pid(&a, &key, run);
    let identity = count(
        &db,
        &format!("SELECT pid_started_at FROM run WHERE id = {run}"),
    );
    assert_eq!(aurora::runner::process_identity(agent), Some(identity));
    first.kill();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        alive(agent) && alive(child),
        "no job: they outlive the server"
    );

    let mut second = start(&db, None, &[]);
    wait_until("the leftover to be killed", Duration::from_secs(15), || {
        !alive(agent) && !alive(child)
    });
    let sql = |q: &str| count(&db, q);
    wait_until("the run to be interrupted", Duration::from_secs(10), || {
        sql(&format!(
            "SELECT COUNT(*) FROM run WHERE id = {run} AND status = 'interrupted'"
        )) == 1
    });
    assert_eq!(
        sql(&format!(
            "SELECT COUNT(*) FROM run WHERE id = {run} AND reaped = 'killed leftover process {agent}'"
        )),
        1
    );
    let todo = column(&second.addr.clone(), &key, "unstarted");
    let issue = ok(
        &second.addr,
        &key,
        "GET",
        "/api/v1/projects/AA/issues/1",
        None,
    );
    assert_eq!(
        issue["column_id"].as_i64(),
        Some(todo),
        "the card left the started column"
    );
    assert_eq!(
        sql("SELECT COUNT(*) FROM comment WHERE body LIKE '[Aurora] Run % was interrupted%'"),
        1
    );
    let moves = sql("SELECT COUNT(*) FROM event WHERE kind = 'issue.moved'");
    let housekeeping = ok(
        &second.addr,
        &key,
        "GET",
        "/api/v1/admin/housekeeping",
        None,
    );
    assert_eq!(housekeeping["leftover_processes"][0]["run"], run);
    assert_checks_green(&db, "after the reconciliation");

    second.kill();
    let mut third = start(&db, None, &[]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(sql("SELECT COUNT(*) FROM comment"), 1, "no second comment");
    assert_eq!(
        sql("SELECT COUNT(*) FROM event WHERE kind = 'issue.moved'"),
        moves,
        "no second move"
    );
    third.kill();
    assert_checks_green(&db, "after the third start");
}

/// The pid exists but its creation time is not the recorded one: it is not
/// the run's process, so it is left alone.
#[cfg(windows)]
#[test]
fn a_reused_pid_is_left_alone() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let mut first = start(&db, None, &[("AURORA_TEST_NO_JOB", "1")]);
    let a = first.addr.clone();
    let s = setup(&a, &key, LONG, 1, None);
    assign(&a, &s, 1);
    let (run, agent) = running_run(&a, &key);
    let child = child_pid(&a, &key, run);
    let identity = count(
        &db,
        &format!("SELECT pid_started_at FROM run WHERE id = {run}"),
    );
    first.kill();

    let conn = aurora_store::rusqlite::Connection::open(&db).expect("open");
    conn.execute(
        "UPDATE run SET pid_started_at = pid_started_at + 12345 WHERE id = ?1",
        [run],
    )
    .expect("update");
    drop(conn);

    let mut second = start(&db, None, &[]);
    wait_until("the run to be interrupted", Duration::from_secs(10), || {
        count(&db, "SELECT COUNT(*) FROM run WHERE status = 'interrupted'") == 1
    });
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM run WHERE reaped = 'pid reused, left alone'"
        ),
        1
    );
    assert!(alive(agent) && alive(child), "left alone");
    second.kill();
    // Clean up our own processes, after checking identity once more.
    kill_verified(agent, identity);
    kill_verified(child, aurora::runner::process_identity(child).unwrap_or(-1));
    wait_until("cleanup", Duration::from_secs(10), || !alive(agent));
}

/// Killed right after the leftover process was killed and before the database
/// was updated: the next start sees "process already gone" and the run ends
/// interrupted once, with one comment and one card move.
#[cfg(windows)]
#[test]
fn a_crash_between_the_kill_and_the_update_is_repeated_safely() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let mut first = start(&db, None, &[("AURORA_TEST_NO_JOB", "1")]);
    let a = first.addr.clone();
    let s = setup(&a, &key, LONG, 1, None);
    let started_col = column(&a, &key, "started");
    move_card(&a, &key, 1, started_col);
    assign(&a, &s, 1);
    let (run, agent) = running_run(&a, &key);
    first.kill();

    let mut armed = Server::spawn_with(&db, Some("reconcile_after_kill"), &[]);
    let status = armed
        .wait_exit(Duration::from_secs(30))
        .expect("the server dies at the failpoint");
    assert!(!status.success());
    drop(armed);
    wait_until("the agent to be gone", Duration::from_secs(10), || {
        !alive(agent)
    });
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM run WHERE id = {run} AND status = 'running'")
        ),
        1,
        "the database was not updated yet"
    );
    assert_checks_green(&db, "after the crash");

    let mut again = start(&db, None, &[]);
    wait_until("the run to be interrupted", Duration::from_secs(10), || {
        count(&db, "SELECT COUNT(*) FROM run WHERE status = 'interrupted'") == 1
    });
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM run WHERE reaped = 'process already gone'"
        ),
        1
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM comment"), 1);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM event WHERE kind = 'run.finished'"
        ),
        1
    );
    again.kill();
    assert_checks_green(&db, "after the restart");
}

// ---- housekeeping (any platform) ----

fn make_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git must be installed for this test: {e}"));
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "tests@example.invalid"]);
    git(&["config", "user.name", "Tests"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "first"]);
    dir
}

#[test]
fn housekeeping_reports_unknown_kept_and_missing_copies_and_deletes_nothing() {
    let repo = make_repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let server = start(&db, None, &[]);
    let a = server.addr.clone();
    let s = setup(&a, &key, &["--touch", "work.txt"], 2, Some(repo.path()));
    assign(&a, &s, 1);
    assign(&a, &s, 2);
    wait_until("both runs to finish", Duration::from_secs(60), || {
        count(
            &db,
            "SELECT COUNT(*) FROM run WHERE status = 'succeeded' AND worktree_state = 'kept'",
        ) == 2
    });
    let work = dir.path().join("worktrees");
    let kept = work.join("1");
    let missing = work.join("2");
    assert!(kept.is_dir() && missing.is_dir());
    std::fs::remove_dir_all(&missing).expect("remove one by hand");
    std::fs::create_dir_all(work.join("stray-folder")).expect("stray");
    std::fs::write(work.join("note.txt"), "x").expect("file");

    let report = ok(&a, &key, "GET", "/api/v1/admin/housekeeping", None);
    let unknown: Vec<&str> = report["unknown_working_copies"]
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert_eq!(
        unknown,
        ["worktrees/note.txt", "worktrees/stray-folder"],
        "{report}"
    );
    let copies = report["kept_working_copies"].as_array().expect("list");
    assert_eq!(copies.len(), 2);
    for copy in copies {
        let exists = copy["run"] == 1;
        assert_eq!(copy["folder_exists"], exists, "{copy}");
        assert_eq!(copy["state"], "kept");
    }
    assert!(report["checked_at"].as_i64().unwrap_or(0) > 0);

    // Nothing was deleted by looking.
    assert!(kept.is_dir() && work.join("stray-folder").is_dir() && work.join("note.txt").is_file());

    let (status, _) = call(&a, &s.agent_key, "GET", "/api/v1/admin/housekeeping", None);
    assert_eq!(status, 403, "agents are refused");
    let (status, _) = call(
        &a,
        "aur_not_a_key",
        "GET",
        "/api/v1/admin/housekeeping",
        None,
    );
    assert_eq!(status, 401);
    // The agent's view of a run does not carry the reconciliation note.
    let (_, run) = call(&a, &s.agent_key, "GET", "/api/v1/runs/1", None);
    assert!(run.get("reaped").is_none(), "{run}");
    let (_, run) = call(&a, &key, "GET", "/api/v1/runs/1", None);
    assert!(run.get("reaped").is_some(), "{run}");
    drop(server);
}
