//! Kill-the-process tests for request ids (TRK-43). A child process (this test
//! binary, running `helper_serves_until_killed`) serves the real router and
//! aborts at a failpoint of `crate::idem`; the parent then reopens the same
//! data file and retries with the same `Idempotency-Key`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aurora_store::auth::{Principal, Role, Scope};
use aurora_store::{Store, now_millis, projects};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::ShutdownHooks;

const DB: &str = "AURORA_CRASH_HELPER_DB";
const PORT_FILE: &str = "AURORA_CRASH_HELPER_PORT_FILE";
const HELPER: &str = "idem_crash_tests::helper_serves_until_killed";
const KEY: &str = "aur_crash_tests_only_not_a_secret";
const IDEM: &str = "crash-retry-0001";
const BODY: &str = r#"{"title":"Crash test"}"#;

/// Runs only as the child of the tests below.
#[tokio::test]
async fn helper_serves_until_killed() {
    let Ok(db) = std::env::var(DB) else {
        return;
    };
    let store = Store::open(db).expect("the child opens the database");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let file = std::env::var(PORT_FILE).unwrap();
    let temporary = format!("{file}.tmp");
    std::fs::write(&temporary, port.to_string()).unwrap();
    std::fs::rename(&temporary, &file).unwrap();
    let _ = crate::serve(
        listener,
        store,
        std::future::pending::<()>(),
        ShutdownHooks::new(),
    )
    .await;
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Child {
    fn exited_abnormally(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(status) = self.0.try_wait().unwrap() {
                return !status.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

async fn prepare(path: &Path) {
    let store = Store::open(path).unwrap();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind, role) VALUES (1, 'owner', 'human', 'owner')",
                [],
            )?;
            tx.execute(
                "INSERT INTO api_key (user_id, name, prefix, key_hash, scope, created)
                 VALUES (1, 'tests', 'aur_cras', ?1, 'write', 1)",
                [format!("{:x}", Sha256::digest(KEY.as_bytes()))],
            )?;
            let owner = Principal {
                user_id: 1,
                role: Role::Owner,
                scope: Scope::Write,
            };
            projects::create_project(tx, now_millis(), &owner, "TRK", "Tracker", None)?;
            projects::create_issue_type(tx, now_millis(), &owner, "TRK", "Task")?;
            Ok(())
        })
        .await
        .unwrap();
    store.close().await;
}

fn spawn_helper(db: &Path, port_file: &Path, failpoint: &str) -> Child {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([HELPER, "--exact", "--test-threads=1", "--nocapture"])
        .env(DB, db)
        .env(PORT_FILE, port_file)
        .env("AURORA_TEST_FAILPOINT", failpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Child(child)
}

async fn helper_port(port_file: &Path) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(port_file) {
            return text.trim().parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the helper process did not report its port");
}

/// The raw response, or `None` when the connection died without one.
async fn post_issue(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let request = format!(
        "POST /api/v1/projects/TRK/issues HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Authorization: Bearer {KEY}\r\nIdempotency-Key: {IDEM}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{BODY}",
        BODY.len()
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut raw = String::new();
    let read = tokio::time::timeout(Duration::from_secs(30), stream.read_to_string(&mut raw)).await;
    match read {
        Ok(Ok(_)) if !raw.is_empty() => Some(raw.to_ascii_lowercase()),
        _ => None,
    }
}

async fn counts(store: &Store) -> (i64, i64, i64) {
    store
        .read(|c| {
            let one = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0));
            Ok((
                one("SELECT COUNT(*) FROM issue")?,
                one("SELECT COUNT(*) FROM event WHERE kind = 'issue.created'")?,
                one("SELECT COUNT(*) FROM request_log")?,
            ))
        })
        .await
        .unwrap()
}

/// The server on the data folder the dead child used, in this process.
async fn restart(path: &Path) -> (Store, u16, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let store = Store::open(path).unwrap();
    assert!(
        store.open_report().unclean_shutdown,
        "the killed child left its run marker"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = oneshot::channel::<()>();
    let served = store.clone();
    let server = tokio::spawn(async move {
        let _ = crate::serve(
            listener,
            served,
            async {
                let _ = stopped.await;
            },
            ShutdownHooks::new(),
        )
        .await;
    });
    (store, port, stop, server)
}

#[tokio::test]
async fn the_harness_gets_an_answer_when_no_failpoint_fires() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    prepare(&db).await;
    let port_file = dir.path().join("port");
    let _child = spawn_helper(&db, &port_file, "no-such-failpoint");
    let port = helper_port(&port_file).await;
    let raw = post_issue(port).await.expect("an answer");
    assert!(raw.starts_with("http/1.1 201"), "{raw}");
    assert!(!raw.contains("idempotent-replay"), "{raw}");
}

#[tokio::test]
async fn a_crash_after_the_commit_and_before_the_response_is_replayed_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    prepare(&db).await;
    let port_file = dir.path().join("port");
    let mut child = spawn_helper(&db, &port_file, "after_commit_before_response");
    let port = helper_port(&port_file).await;

    let answer = post_issue(port).await;
    assert!(
        answer.is_none(),
        "the client never got an answer: {answer:?}"
    );
    assert!(
        child.exited_abnormally(),
        "the child must die at the failpoint"
    );

    let (store, port, stop, server) = restart(&db).await;
    assert_eq!(
        counts(&store).await,
        (1, 1, 1),
        "the change and its request id were committed together"
    );

    let retry = post_issue_to(port).await;
    assert!(retry.starts_with("http/1.1 201"), "{retry}");
    assert!(retry.contains("idempotent-replay: true"), "{retry}");
    assert!(retry.contains("\"number\":1"), "{retry}");
    assert!(retry.contains("crash test"), "{retry}");
    assert_eq!(
        counts(&store).await,
        (1, 1, 1),
        "still one issue, one event"
    );

    let again = post_issue_to(port).await;
    assert!(again.contains("idempotent-replay: true"), "{again}");
    assert_eq!(counts(&store).await, (1, 1, 1));

    stop.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn a_crash_before_the_commit_leaves_nothing_and_the_retry_applies_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    prepare(&db).await;
    let port_file = dir.path().join("port");
    let mut child = spawn_helper(&db, &port_file, "before_commit");
    let port = helper_port(&port_file).await;

    let answer = post_issue(port).await;
    assert!(
        answer.is_none(),
        "the client never got an answer: {answer:?}"
    );
    assert!(
        child.exited_abnormally(),
        "the child must die at the failpoint"
    );

    let (store, port, stop, server) = restart(&db).await;
    assert_eq!(
        counts(&store).await,
        (0, 0, 0),
        "neither the change nor the id survived"
    );

    let retry = post_issue_to(port).await;
    assert!(retry.starts_with("http/1.1 201"), "{retry}");
    assert!(
        !retry.contains("idempotent-replay"),
        "the retry is the first apply"
    );
    assert_eq!(counts(&store).await, (1, 1, 1));

    let again = post_issue_to(port).await;
    assert!(again.contains("idempotent-replay: true"), "{again}");
    assert_eq!(counts(&store).await, (1, 1, 1), "applied exactly once");

    stop.send(()).unwrap();
    server.await.unwrap();
}

async fn post_issue_to(port: u16) -> String {
    post_issue(port)
        .await
        .expect("an answer from the restarted server")
}
