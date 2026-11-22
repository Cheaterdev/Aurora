//! The runner's failpoints (TRK-15), with the real binary: the process is
//! killed while a run is being created, started or finished, restarted on the
//! same data folder, and the run must end up `interrupted` with its key
//! revoked, exactly once, with a database that passes `aurora check`.
//!
//! A killed server leaves a child behind (nothing ties the agent's process to
//! the server yet; TRK-45 does that). The agent used here ends by itself after
//! two seconds, so no test leaves a process running.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;
use common::harness::{Server, assert_checks_green, bootstrap, count, request};

const READY: Duration = Duration::from_secs(60);
const DIES: Duration = Duration::from_secs(30);

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

fn ok(addr: &str, key: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let reply = request(addr, key, method, path, body.as_ref(), None).expect("answered");
    assert!(
        reply.is_success(),
        "{method} {path}: {} {}",
        reply.status,
        reply.body
    );
    serde_json::from_str(reply.body.trim()).unwrap_or(Value::Null)
}

fn wait_for(what: &str, db: &Path, sql: &str, want: i64) {
    let started = Instant::now();
    loop {
        if count(db, sql) == want {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{what}: `{sql}` never became {want} (is {})",
            count(db, sql)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn case(point: &str) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);

    let mut armed = Server::spawn(&db, Some(&format!("{point}:1")));
    armed
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{point}: the armed server did not start: {why}"));
    let a = armed.addr.clone();
    ok(
        &a,
        &key,
        "POST",
        "/api/v1/projects",
        Some(json!({"key": "AA", "name": "A"})),
    );
    ok(
        &a,
        &key,
        "POST",
        "/api/v1/projects/AA/issue-types",
        Some(json!({"name": "Task"})),
    );
    ok(
        &a,
        &key,
        "POST",
        "/api/v1/projects/AA/issues",
        Some(json!({"title": "Crash me"})),
    );
    let made = ok(
        &a,
        &key,
        "POST",
        "/api/v1/agents",
        Some(
            json!({"name": "bot", "command": fake_agent().display().to_string(),
                    "args": ["--sleep", "2"], "projects": ["AA"]}),
        ),
    );
    let user = made["user_id"].as_i64().expect("agent user");
    ok(
        &a,
        &key,
        "PATCH",
        "/api/v1/projects/AA/issues/1",
        Some(json!({"assignee_id": user})),
    );

    let status = armed
        .wait_exit(DIES)
        .unwrap_or_else(|| panic!("{point}: the server did not die:\n{}", armed.log_text()));
    assert!(!status.success(), "{point}: exited cleanly");
    drop(armed);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM run"),
        1,
        "{point}: one run was made"
    );
    assert_checks_green(
        &db,
        &format!("{point}: after the crash, before the restart"),
    );

    let mut server = Server::spawn(&db, None);
    server
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{point}: the restart failed: {why}"));
    wait_for(
        point,
        &db,
        "SELECT COUNT(*) FROM run WHERE status = 'interrupted' AND error = 'server restarted'",
        1,
    );
    // The unacknowledged assignment event is delivered again and must not
    // make a second run.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM run"),
        1,
        "{point}: no second run"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM api_key k JOIN run r ON k.name = 'run-' || r.id
             WHERE k.revoked IS NOT NULL"
        ),
        1,
        "{point}: the run's key is revoked"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM run WHERE status IN ('starting', 'running')"
        ),
        0,
        "{point}: nothing is left active"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM event WHERE kind = 'run.finished'"
        ),
        1,
        "{point}: one finish event"
    );
    if point == "run_after_transcript_write" {
        // What was written before the crash is readable, a half-written last
        // line is ignored, and no end note exists.
        let run_id = count(&db, "SELECT id FROM run");
        let file = dir.path().join("transcripts").join(format!("{run_id}.log"));
        let len = std::fs::metadata(&file)
            .unwrap_or_else(|e| panic!("{point}: no transcript file: {e}"))
            .len();
        assert!(len > 0, "{point}: the transcript has what was flushed");
        let before = aurora::transcript::read_page(&file, 0, 1000)
            .expect("read")
            .expect("a file");
        assert!(!before.entries.is_empty(), "{point}: whole lines are read");
        assert!(
            before.entries.iter().all(|e| e.s != "x"),
            "{point}: a crash writes no end note"
        );
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&file)
                .expect("open");
            f.write_all(br#"{"t":5,"s":"o","d":"torn"#).expect("append");
        }
        let after = aurora::transcript::read_page(&file, 0, 1000)
            .expect("read")
            .expect("a file");
        assert_eq!(
            after.entries.len(),
            before.entries.len(),
            "{point}: the torn last line is ignored"
        );
        assert_eq!(after.next, before.next);
    }
    assert_checks_green(&db, &format!("{point}: after the restart"));
    server.kill();
    assert_checks_green(&db, &format!("{point}: after killing the restarted server"));
}

#[test]
fn a_crash_after_the_run_is_created_interrupts_it_on_restart() {
    case("run_after_create");
}

#[test]
fn a_crash_after_the_process_is_spawned_interrupts_the_run_on_restart() {
    case("run_after_spawn");
}

#[test]
fn a_crash_after_a_transcript_write_keeps_what_was_flushed() {
    case("run_after_transcript_write");
}

#[test]
fn a_crash_before_the_finish_is_recorded_interrupts_the_run_on_restart() {
    case("run_before_finish");
}

/// A repository with one commit, in a temporary folder of its own.
fn make_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git must be installed and on PATH for this test: {e}"));
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

/// Killed right after the run's working copy was made and before the process
/// starts: after the restart the run is interrupted, its working copy is kept
/// (not deleted, not claimed to be removed), the folder is still there, and
/// nothing runs twice.
#[test]
fn a_crash_after_the_worktree_is_made_keeps_it_and_interrupts_the_run_on_restart() {
    let point = "run_after_worktree";
    let repo = make_repo();
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);

    let mut armed = Server::spawn(&db, Some(&format!("{point}:1")));
    armed
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{point}: the armed server did not start: {why}"));
    let a = armed.addr.clone();
    ok(
        &a,
        &key,
        "POST",
        "/api/v1/projects",
        Some(json!({"key": "AA", "name": "A"})),
    );
    ok(
        &a,
        &key,
        "POST",
        "/api/v1/projects/AA/issue-types",
        Some(json!({"name": "Task"})),
    );
    ok(
        &a,
        &key,
        "PATCH",
        "/api/v1/projects/AA",
        Some(json!({"repo_path": repo.path().display().to_string(), "base_branch": "main"})),
    );
    ok(
        &a,
        &key,
        "POST",
        "/api/v1/projects/AA/issues",
        Some(json!({"title": "Crash me"})),
    );
    let made = ok(
        &a,
        &key,
        "POST",
        "/api/v1/agents",
        Some(
            json!({"name": "bot", "command": fake_agent().display().to_string(),
                    "args": ["--sleep", "2"], "projects": ["AA"]}),
        ),
    );
    let user = made["user_id"].as_i64().expect("agent user");
    ok(
        &a,
        &key,
        "PATCH",
        "/api/v1/projects/AA/issues/1",
        Some(json!({"assignee_id": user})),
    );
    let status = armed
        .wait_exit(DIES)
        .unwrap_or_else(|| panic!("{point}: the server did not die:\n{}", armed.log_text()));
    assert!(!status.success(), "{point}: exited cleanly");
    drop(armed);

    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM run"),
        1,
        "one run was made"
    );
    let id = count(&db, "SELECT id FROM run");
    let folder = dir.path().join("worktrees").join(id.to_string());
    assert!(folder.is_dir(), "the working copy exists after the crash");
    assert_checks_green(&db, "after the crash, before the restart");

    let mut server = Server::spawn(&db, None);
    server
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{point}: the restart failed: {why}"));
    wait_for(
        point,
        &db,
        "SELECT COUNT(*) FROM run WHERE status = 'interrupted' AND error = 'server restarted'",
        1,
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(count(&db, "SELECT COUNT(*) FROM run"), 1, "no second run");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM run WHERE worktree_state = 'kept'"
        ),
        1,
        "the working copy is kept, not claimed removed"
    );
    assert!(
        folder.is_dir(),
        "the folder is still there after the restart"
    );
    let branches = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["branch", "--list", "aurora/*"])
        .output()
        .expect("git");
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains(&format!("run{id}")),
        "the branch is kept: {}",
        String::from_utf8_lossy(&branches.stdout)
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM run WHERE status IN ('starting', 'running')"
        ),
        0
    );
    assert_checks_green(&db, "after the restart");
    server.kill();
    assert_checks_green(&db, "after killing the restarted server");
}
