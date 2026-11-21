//! Kills the real `aurora` binary at every failpoint of
//! `aurora_store::failpoints`, restarts it on the same data folder and checks
//! the invariants: every acknowledged change is there, nothing is half applied,
//! events and state agree, and consumers get unacknowledged events again.
//!
//! The binary is the one `cargo test` builds, which has the failpoints
//! compiled in (a release build has none). Each case runs a scripted sequence
//! of API writes against a server armed to abort at the n-th hit of one
//! failpoint; the script stops at the first request that gets no answer.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::harness::{Resp, Server, assert_checks_green, aurora_check, bootstrap, count, request};

const READY: Duration = Duration::from_secs(60);
const DIES: Duration = Duration::from_secs(30);

/// What the tables must show for the change that was being written when the
/// process died, before the client retries it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InFlight {
    /// Never committed.
    Absent,
    /// Committed, only the answer was lost.
    Present,
    /// The abort may have hit a bookkeeping write instead of the change.
    Either,
}

enum Want {
    Eq(Value),
    AtLeast(i64),
}

struct Inflight {
    method: &'static str,
    path: String,
    body: Option<Value>,
    idem: String,
    /// A query that counts what the request creates; 1 once it is applied.
    once: Option<String>,
}

/// The scripted client: it records what the server acknowledged.
struct Script {
    addr: String,
    key: String,
    n: u32,
    wants: BTreeMap<(String, String), Want>,
    acked: u32,
    inflight: Option<Inflight>,
}

impl Script {
    fn new(addr: &str, key: &str) -> Script {
        Script {
            addr: addr.to_string(),
            key: key.to_string(),
            n: 0,
            wants: BTreeMap::new(),
            acked: 0,
            inflight: None,
        }
    }

    /// A write with a fresh request id. `None`: the server did not answer.
    fn write(
        &mut self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
        once: Option<&str>,
    ) -> Option<Resp> {
        self.n += 1;
        let idem = format!("crash-case-{:04}", self.n);
        self.inflight = Some(Inflight {
            method,
            path: path.to_string(),
            body: body.clone(),
            idem: idem.clone(),
            once: once.map(String::from),
        });
        match request(
            &self.addr,
            &self.key,
            method,
            path,
            body.as_ref(),
            Some(&idem),
        ) {
            Ok(resp) => {
                assert!(
                    resp.is_success(),
                    "{method} {path} answered {}: {}",
                    resp.status,
                    resp.body
                );
                self.inflight = None;
                self.acked += 1;
                Some(resp)
            }
            Err(_) => None,
        }
    }

    fn read(&self, path: &str) -> Option<Resp> {
        request(&self.addr, &self.key, "GET", path, None, None).ok()
    }

    fn expect(&mut self, path: &str, pointer: &str, value: Value) {
        self.wants
            .insert((path.to_string(), pointer.to_string()), Want::Eq(value));
    }

    fn expect_at_least(&mut self, path: &str, pointer: &str, value: i64) {
        self.wants.insert(
            (path.to_string(), pointer.to_string()),
            Want::AtLeast(value),
        );
    }

    /// About 26 writes over every kind the server has. `None` when the server died.
    fn run(&mut self) -> Option<()> {
        let project = self
            .write(
                "POST",
                "/api/v1/projects",
                Some(json!({"key": "TRK", "name": "Tracker"})),
                Some("SELECT COUNT(*) FROM project WHERE key = 'TRK'"),
            )?
            .json();
        self.expect("/api/v1/projects/TRK", "/name", json!("Tracker"));
        let todo = project["columns"][1]["id"].as_i64().unwrap_or(0);

        let task = self
            .write(
                "POST",
                "/api/v1/projects/TRK/issue-types",
                Some(json!({"name": "Task"})),
                Some("SELECT COUNT(*) FROM issue_type WHERE name = 'Task'"),
            )?
            .json();
        let type_id = task["id"].as_i64().unwrap_or(0);
        let label = self
            .write(
                "POST",
                "/api/v1/projects/TRK/labels",
                Some(json!({"name": "bug", "color": "#aabbcc"})),
                Some("SELECT COUNT(*) FROM label WHERE name = 'bug'"),
            )?
            .json();
        let label_id = label["id"].as_i64().unwrap_or(0);
        let agent = self
            .write(
                "POST",
                "/api/v1/agents",
                Some(json!({"name": "runner", "command": "echo", "projects": ["TRK"]})),
                Some("SELECT COUNT(*) FROM agent"),
            )?
            .json();
        self.expect(
            &format!("/api/v1/agents/{}", agent["id"]),
            "/name",
            json!("runner"),
        );

        for n in 1..=4 {
            let title = format!("issue {n}");
            let once = format!("SELECT COUNT(*) FROM issue WHERE title = '{title}'");
            let labels = if n == 1 { vec![label_id] } else { vec![] };
            self.write(
                "POST",
                "/api/v1/projects/TRK/issues",
                Some(json!({"title": title, "type_id": type_id, "labels": labels})),
                Some(&once),
            )?;
            self.expect(
                &format!("/api/v1/projects/TRK/issues/{n}"),
                "/title",
                json!(title),
            );
        }
        self.write(
            "PATCH",
            "/api/v1/projects/TRK/issues/1",
            Some(json!({"title": "issue 1 renamed", "priority": "high", "description": "details"})),
            None,
        )?;
        self.expect(
            "/api/v1/projects/TRK/issues/1",
            "/title",
            json!("issue 1 renamed"),
        );
        self.expect("/api/v1/projects/TRK/issues/1", "/priority", json!("high"));
        self.write(
            "POST",
            "/api/v1/projects/TRK/issues/2/move",
            Some(json!({"column_id": todo})),
            None,
        )?;
        self.expect("/api/v1/projects/TRK/issues/2", "/column_id", json!(todo));

        let hello = self
            .write(
                "POST",
                "/api/v1/projects/TRK/issues/1/comments",
                Some(json!({"body": "hi @runner"})),
                Some("SELECT COUNT(*) FROM comment WHERE body = 'hi @runner'"),
            )?
            .json();
        let comment = hello["id"].as_i64().unwrap_or(0);
        self.expect(
            &format!("/api/v1/projects/TRK/comments/{comment}"),
            "/body",
            json!("hi @runner"),
        );
        self.write(
            "POST",
            "/api/v1/projects/TRK/issues/1/comments",
            Some(json!({"body": "a reply", "parent": comment})),
            Some("SELECT COUNT(*) FROM comment WHERE body = 'a reply'"),
        )?;
        self.write(
            "POST",
            "/api/v1/projects/TRK/issues/1/links",
            Some(json!({"kind": "blocks", "to": 2})),
            Some("SELECT COUNT(*) FROM issue_link"),
        )?;
        self.expect(
            "/api/v1/projects/TRK/issues/1/links",
            "/blocks/0/number",
            json!(2),
        );

        let doc = self
            .write(
                "POST",
                "/api/v1/projects/TRK/documents",
                Some(json!({"title": "Handbook", "body": "v1"})),
                Some("SELECT COUNT(*) FROM document WHERE title = 'Handbook'"),
            )?
            .json();
        let doc_id = doc["id"].as_i64().unwrap_or(0);
        let doc_path = format!("/api/v1/projects/TRK/documents/{doc_id}");
        self.write(
            "PATCH",
            &doc_path,
            Some(json!({"body": "v2", "base_revision": 1})),
            None,
        )?;
        self.expect(&doc_path, "/body", json!("v2"));
        self.expect(&doc_path, "/revision", json!(2));
        self.write(
            "POST",
            &format!("{doc_path}/links"),
            Some(json!({"issue": 1})),
            Some("SELECT COUNT(*) FROM document_link"),
        )?;

        let initiative = self
            .write(
                "POST",
                "/api/v1/projects/TRK/initiatives",
                Some(json!({"name": "Launch"})),
                Some("SELECT COUNT(*) FROM initiative WHERE name = 'Launch'"),
            )?
            .json();
        let initiative_id = initiative["id"].as_i64().unwrap_or(0);
        self.write(
            "PATCH",
            "/api/v1/projects/TRK/issues/3",
            Some(json!({"initiative": initiative_id})),
            None,
        )?;
        self.expect(
            "/api/v1/projects/TRK/issues/3",
            "/initiative_id",
            json!(initiative_id),
        );

        self.write(
            "PUT",
            "/api/v1/consumers/c1",
            Some(json!({"start": "beginning"})),
            None,
        )?;
        self.poll_and_ack()?;
        for k in 1..=3 {
            let title = format!("tail {k}");
            let once = format!("SELECT COUNT(*) FROM issue WHERE title = '{title}'");
            self.write(
                "POST",
                "/api/v1/projects/TRK/issues",
                Some(json!({"title": title, "type_id": type_id})),
                Some(&once),
            )?;
            self.poll_and_ack()?;
        }
        self.write("POST", "/api/v1/projects/TRK/issues/4/archive", None, None)?;
        self.expect("/api/v1/projects/TRK/issues/4", "/archived", json!(true));
        Some(())
    }

    /// A consumer reads what it has not acknowledged and acknowledges it.
    fn poll_and_ack(&mut self) -> Option<()> {
        let polled = self.read("/api/v1/consumers/c1/events?limit=500")?.json();
        let last = polled["events"]
            .as_array()
            .and_then(|events| events.last())
            .and_then(|event| event["id"].as_i64())?;
        self.write(
            "POST",
            "/api/v1/consumers/c1/ack",
            Some(json!({"up_to": last})),
            None,
        )?;
        self.expect_at_least(
            "/api/v1/consumers/c1/events",
            "/consumer/last_event_id",
            last,
        );
        Some(())
    }
}

fn pointer_of(value: &Value, pointer: &str) -> Value {
    value.pointer(pointer).cloned().unwrap_or(Value::Null)
}

fn verify_acknowledged(addr: &str, key: &str, script: &Script) {
    for ((path, pointer), want) in &script.wants {
        let resp = request(addr, key, "GET", path, None, None)
            .unwrap_or_else(|err| panic!("GET {path}: {err}"));
        assert_eq!(
            resp.status, 200,
            "GET {path} after the restart: {}",
            resp.body
        );
        let found = pointer_of(&resp.json(), pointer);
        match want {
            Want::Eq(expected) => assert_eq!(
                &found, expected,
                "{path} {pointer}: an acknowledged change is missing or different"
            ),
            Want::AtLeast(minimum) => assert!(
                found.as_i64().is_some_and(|n| n >= *minimum),
                "{path} {pointer} is {found}, an acknowledged ack of {minimum} was lost"
            ),
        }
    }
}

/// An event a consumer did not acknowledge comes again, none is skipped.
fn verify_redelivery(addr: &str, key: &str, db: &Path) {
    let resp = request(
        addr,
        key,
        "GET",
        "/api/v1/consumers/c1/events?limit=500",
        None,
        None,
    )
    .expect("consumer poll");
    if resp.status == 404 {
        return;
    }
    assert_eq!(resp.status, 200, "{}", resp.body);
    let json = resp.json();
    let cursor = json["consumer"]["last_event_id"].as_i64().unwrap_or(-1);
    let ids: Vec<i64> = json["events"]
        .as_array()
        .map(|events| events.iter().filter_map(|e| e["id"].as_i64()).collect())
        .unwrap_or_default();
    let latest = count(db, "SELECT COALESCE(MAX(id), 0) FROM event");
    let expected: Vec<i64> = (cursor + 1..=latest.min(cursor + 500)).collect();
    assert_eq!(
        ids, expected,
        "the consumer at {cursor} must be given every later event, once"
    );
}

fn run_case(point: &str, hit: u32, before_retry: InFlight) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);

    let spec = format!("{point}:{hit}");
    let mut armed = Server::spawn(&db, Some(&spec));
    armed
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{spec}: the armed server did not start: {why}"));
    let mut script = Script::new(&armed.addr, &key);
    let finished = script.run();
    assert!(
        finished.is_none(),
        "{spec}: the whole script ran ({} writes) and the failpoint never fired",
        script.acked
    );
    let status = armed
        .wait_exit(DIES)
        .unwrap_or_else(|| panic!("{spec}: the server did not die:\n{}", armed.log_text()));
    assert!(!status.success(), "{spec}: exited cleanly");
    drop(armed);

    // The crashed files, before anything repairs them.
    assert_checks_green(&db, &format!("{spec}: after the crash, before the restart"));
    let pending = script.inflight.as_ref();
    let applied = |db: &Path| {
        pending
            .and_then(|p| p.once.as_ref())
            .map(|sql| count(db, sql))
    };
    match (before_retry, applied(&db)) {
        (InFlight::Absent, Some(n)) => assert_eq!(n, 0, "{spec}: an uncommitted change is visible"),
        (InFlight::Present, Some(n)) => assert_eq!(n, 1, "{spec}: a committed change is missing"),
        (_, Some(n)) => assert!(n <= 1, "{spec}: {n} copies of one request"),
        (_, None) => {}
    }

    let mut server = Server::spawn(&db, None);
    server
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{spec}: the restart failed: {why}"));
    assert!(
        server.log_text().contains("unclean_shutdown=true"),
        "{spec}: the restart did not notice the crash:\n{}",
        server.log_text()
    );
    assert_checks_green(&db, &format!("{spec}: after the restart"));
    verify_acknowledged(&server.addr, &key, &script);

    if let Some(pending) = &script.inflight {
        let retry = request(
            &server.addr,
            &key,
            pending.method,
            &pending.path,
            pending.body.as_ref(),
            Some(&pending.idem),
        )
        .expect("the retry is answered");
        assert!(
            retry.is_success(),
            "{spec}: retry of {} {} answered {}: {}",
            pending.method,
            pending.path,
            retry.status,
            retry.body
        );
        if let Some(sql) = &pending.once {
            assert_eq!(
                count(&db, sql),
                1,
                "{spec}: the retried request applied more than once"
            );
        }
        let again = request(
            &server.addr,
            &key,
            pending.method,
            &pending.path,
            pending.body.as_ref(),
            Some(&pending.idem),
        )
        .expect("a second retry");
        assert!(again.replayed(), "{spec}: a repeat is a replay");
        if let Some(sql) = &pending.once {
            assert_eq!(count(&db, sql), 1, "{spec}: still exactly once");
        }
    }
    verify_redelivery(&server.addr, &key, &db);
    assert_checks_green(&db, &format!("{spec}: after the retry"));
    server.kill();
    assert_checks_green(&db, &format!("{spec}: after killing the restarted server"));
}

/// The control: with nothing armed the whole script is answered, so a case
/// above that stops early stopped because the process was killed.
#[test]
fn the_script_runs_to_the_end_when_no_failpoint_is_armed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);
    let mut server = Server::spawn(&db, None);
    server.wait_ready(READY).expect("start");
    let mut script = Script::new(&server.addr, &key);
    assert!(script.run().is_some(), "the script did not finish");
    assert!(
        script.acked >= 26,
        "only {} writes acknowledged",
        script.acked
    );
    verify_acknowledged(&server.addr, &key, &script);
    verify_redelivery(&server.addr, &key, &db);
    assert_checks_green(&db, "after the whole script");
}

macro_rules! crash_cases {
    ($($name:ident => ($point:expr, $hit:expr, $state:expr)),* $(,)?) => {
        $(#[test] fn $name() { run_case($point, $hit, $state); })*
    };
}

crash_cases! {
    before_commit_hit_1 => ("before_commit", 1, InFlight::Absent),
    before_commit_hit_2 => ("before_commit", 2, InFlight::Absent),
    before_commit_hit_5 => ("before_commit", 5, InFlight::Absent),
    before_commit_hit_17 => ("before_commit", 17, InFlight::Absent),
    after_commit_hit_1 => ("after_commit", 1, InFlight::Either),
    after_commit_hit_2 => ("after_commit", 2, InFlight::Either),
    after_commit_hit_5 => ("after_commit", 5, InFlight::Either),
    after_commit_hit_17 => ("after_commit", 17, InFlight::Either),
    after_commit_before_response_hit_1 => ("after_commit_before_response", 1, InFlight::Present),
    after_commit_before_response_hit_2 => ("after_commit_before_response", 2, InFlight::Present),
    after_commit_before_response_hit_5 => ("after_commit_before_response", 5, InFlight::Present),
    after_commit_before_response_hit_17 => ("after_commit_before_response", 17, InFlight::Present),
    before_event_delivery_hit_1 => ("before_event_delivery", 1, InFlight::Either),
    before_event_delivery_hit_2 => ("before_event_delivery", 2, InFlight::Either),
    before_event_delivery_hit_4 => ("before_event_delivery", 4, InFlight::Either),
    mid_ack_hit_1 => ("mid_ack", 1, InFlight::Either),
    mid_ack_hit_2 => ("mid_ack", 2, InFlight::Either),
    mid_ack_hit_3 => ("mid_ack", 3, InFlight::Either),
}

// ---- the live stream ----

fn open_stream(addr: &str, key: &str, query: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    write!(
        stream,
        "GET /events/stream{query} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {key}\r\n\
         Accept: text/event-stream\r\n\r\n"
    )
    .expect("send");
    stream
}

/// Reads for up to `within` and returns everything received.
fn read_stream(stream: &mut TcpStream, within: Duration) -> String {
    let started = std::time::Instant::now();
    let mut seen = Vec::new();
    let mut buffer = [0_u8; 4096];
    while started.elapsed() < within {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => seen.extend_from_slice(&buffer[..n]),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&seen).into_owned()
}

#[test]
fn a_crash_before_the_live_stream_sends_an_event_loses_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let key = bootstrap(&db);

    let mut armed = Server::spawn(&db, Some("before_event_delivery"));
    armed.wait_ready(READY).expect("armed server");
    let mut stream = open_stream(&armed.addr, &key, "?after=0");
    let head = read_stream(&mut stream, Duration::from_millis(1500));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    let created = request(
        &armed.addr,
        &key,
        "POST",
        "/api/v1/projects",
        Some(&json!({"key": "TRK", "name": "Tracker"})),
        Some("stream-case-0001"),
    );
    let acknowledged = created.as_ref().is_ok_and(Resp::is_success);
    let status = armed
        .wait_exit(DIES)
        .unwrap_or_else(|| panic!("the server did not die:\n{}", armed.log_text()));
    assert!(!status.success());
    let delivered = read_stream(&mut stream, Duration::from_secs(2));
    assert!(
        !delivered.contains("id: "),
        "the stream delivered an event before the failpoint:\n{delivered}"
    );
    drop(armed);
    assert_checks_green(&db, "after the crash");

    let mut server = Server::spawn(&db, None);
    server.wait_ready(READY).expect("restart");
    let created = count(&db, "SELECT COUNT(*) FROM project WHERE key = 'TRK'");
    assert!(
        created == 1 || !acknowledged,
        "an acknowledged project is missing"
    );
    let events = count(&db, "SELECT COUNT(*) FROM event");
    assert_eq!(events, created, "the project and its event commit together");
    if created == 1 {
        let mut again = open_stream(&server.addr, &key, "?after=0");
        let text = read_stream(&mut again, Duration::from_secs(8));
        assert!(
            text.contains("id: 1"),
            "the event is delivered after the restart:\n{text}"
        );
        let consumer = request(
            &server.addr,
            &key,
            "PUT",
            "/api/v1/consumers/late",
            Some(&json!({"start": "beginning"})),
            None,
        )
        .expect("register");
        assert!(consumer.is_success());
        let polled = request(
            &server.addr,
            &key,
            "GET",
            "/api/v1/consumers/late/events",
            None,
            None,
        )
        .expect("poll")
        .json();
        assert_eq!(polled["events"][0]["id"], 1);
    }
    assert_checks_green(&db, "after the restart");
}

// ---- migrations ----

fn backups(db: &Path) -> Vec<std::path::PathBuf> {
    let dir = aurora_store::default_backup_dir(db);
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "db"))
                .collect()
        })
        .unwrap_or_default()
}

fn backup_version(backup: &Path) -> i64 {
    count(backup, "SELECT MAX(version) FROM schema_version")
}

#[test]
fn a_crash_mid_migration_of_a_database_with_data_is_finished_by_the_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let older = aurora_store::latest_version() - 1;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let key = runtime.block_on(async {
        let store = aurora_store::Store::open_at_version(&db, older as usize).expect("old db");
        let key = common::owner_key(&store).await;
        store
            .write(|tx| {
                // Written as the old release did, in SQL: the current project
                // reader asks for columns the old schema does not have.
                tx.execute_batch(
                    "INSERT INTO project (id, key, name, created, next_issue_number) VALUES (1, 'TRK', 'Tracker', 1, 2);
                     INSERT INTO \"column\" (project_id, name, state_type, position)
                         VALUES (1, 'Todo', 'unstarted', 0), (1, 'Done', 'completed', 1);
                     INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
                         VALUES (1, 1, 1, 'project', 1, 'project.created',
                                 '{\"key\":\"TRK\",\"name\":\"Tracker\",\"columns\":[\"Todo\",\"Done\"]}');
                     INSERT INTO issue_type (id, project_id, name) VALUES (1, 1, 'Task');
                     INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
                         VALUES (1, 1, 1, 'issue_type', 1, 'issue_type.created', '{\"name\":\"Task\"}');
                     INSERT INTO issue (id, project_id, number, type_id, column_id, title, position, created, updated)
                         VALUES (1, 1, 1, 1, (SELECT id FROM \"column\" WHERE name = 'Todo'),
                                 'from the old release', 1.0, 2, 2);
                     INSERT INTO event (time, actor_id, project_id, entity_kind, entity_id, kind, payload_json)
                         VALUES (2, 1, 1, 'issue', 1, 'issue.created',
                                 '{\"number\":1,\"identifier\":\"TRK-1\",\"title\":\"from the old release\",\"type_id\":1,\"column_id\":1,\"parent\":null,\"assignee_id\":null,\"initiative\":null,\"priority\":null,\"labels\":[]}');",
                )?;
                Ok(())
            })
            .await
            .expect("seed");
        store.close().await;
        key
    });
    assert_eq!(count(&db, "SELECT MAX(version) FROM schema_version"), older);

    let mut armed = Server::spawn(&db, Some("mid_migration"));
    let died = armed
        .wait_ready(READY)
        .expect_err("the server must die while migrating");
    assert!(died.contains("exited with"), "{died}");
    drop(armed);
    assert_eq!(
        count(&db, "SELECT MAX(version) FROM schema_version"),
        older,
        "nothing of the migration was committed"
    );
    assert!(aurora_store::marker_path(&db).exists());

    let report = aurora_store::invariants::check_file(&db).expect("opens");
    assert!(!report.ok(), "an unmigrated database is not current");
    assert!(report.get("schema.version").is_some_and(|c| !c.ok));
    let run = aurora_check(&db);
    assert_eq!(run.code, 1, "{}", run.stdout);

    let mut server = Server::spawn(&db, None);
    server
        .wait_ready(READY)
        .expect("restart completes the migration");
    let health = request(&server.addr, &key, "GET", "/health", None, None)
        .expect("health")
        .json();
    assert_eq!(health["schema_version"], aurora_store::latest_version());
    let issue = request(
        &server.addr,
        &key,
        "GET",
        "/api/v1/projects/TRK/issues/1",
        None,
        None,
    )
    .expect("issue")
    .json();
    assert_eq!(issue["title"], "from the old release");
    // The killed attempt and the restart each backed the old schema up first.
    let kept = backups(&db);
    assert!(!kept.is_empty(), "no backup from before the migration");
    for backup in &kept {
        assert_eq!(backup_version(backup), older);
        assert_eq!(count(backup, "SELECT COUNT(*) FROM issue"), 1);
    }
    assert_checks_green(&db, "after the migration finished");
    server.kill();
    assert_checks_green(&db, "after the final kill");
}

fn fresh_case(hit: u32, expect_backup: bool) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("aurora.db");
    let spec = format!("mid_migration:{hit}");
    let mut armed = Server::spawn(&db, Some(&spec));
    armed
        .wait_ready(READY)
        .expect_err("the server must die while migrating");
    drop(armed);

    let mut server = Server::spawn(&db, None);
    server
        .wait_ready(READY)
        .unwrap_or_else(|why| panic!("{spec}: the restart failed: {why}"));
    let health = request(&server.addr, "none", "GET", "/health", None, None)
        .expect("health")
        .json();
    assert_eq!(health["schema_version"], aurora_store::latest_version());
    let kept = backups(&db);
    assert_eq!(!kept.is_empty(), expect_backup, "{spec}: backups {kept:?}");
    for backup in &kept {
        assert_eq!(backup_version(backup), i64::from(hit) - 1);
    }
    assert_checks_green(&db, &format!("{spec}: after the migration finished"));
    server.kill();
}

#[test]
fn a_crash_in_the_first_migration_of_a_new_database_is_harmless() {
    fresh_case(1, false);
}

#[test]
fn a_crash_in_a_middle_migration_of_a_new_database_is_finished_after_a_backup() {
    fresh_case(5, true);
}

#[test]
fn a_crash_in_the_last_migration_of_a_new_database_is_finished_after_a_backup() {
    fresh_case(aurora_store::latest_version() as u32, true);
}
