//! Runs the real `aurora` binary as a child process for the crash and soak
//! tests, talks to it over plain blocking HTTP, and runs `aurora check`.
//! Only this test process's own children are ever killed.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_aurora");

pub struct Resp {
    pub status: u16,
    pub headers: String,
    pub body: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(self.body.trim())
            .unwrap_or_else(|_| panic!("not JSON ({}): {}", self.status, self.body))
    }

    pub fn replayed(&self) -> bool {
        self.headers.contains("idempotent-replay: true")
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// One request on its own connection. `Err` means the connection died or the
/// server did not answer: for the crash tests, that the process is gone.
pub fn request(
    addr: &str,
    key: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    idem: Option<&str>,
) -> std::io::Result<Resp> {
    let body = body.map(ToString::to_string).unwrap_or_default();
    let idem = idem
        .map(|id| format!("Idempotency-Key: {id}\r\n"))
        .unwrap_or_default();
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Authorization: Bearer {key}\r\n{idem}\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    // A connection that dies mid-response is an error even if some bytes arrived.
    stream.read_to_end(&mut raw)?;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let Some((head, text)) = raw.split_once("\r\n\r\n") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "no response",
        ));
    };
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("bad status line"))?;
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    Ok(Resp {
        status,
        headers: head.to_ascii_lowercase(),
        body: if chunked {
            dechunk(text)
        } else {
            text.to_string()
        },
    })
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

/// Creates the data file with an owner and the test key, as `common::owner_key`
/// does for the in-process tests, and returns the key. The log stays empty.
pub fn bootstrap(db: &Path) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let store = aurora_store::Store::open(db).expect("open");
        let key = super::owner_key(&store).await;
        store.close().await;
        key
    })
}

pub struct Server {
    child: Child,
    pub addr: String,
    pub log: PathBuf,
}

impl Server {
    /// Starts the binary on `db` (port 0) and returns without waiting.
    /// `failpoint` arms `AURORA_TEST_FAILPOINT`.
    pub fn spawn(db: &Path, failpoint: Option<&str>) -> Server {
        Server::spawn_with(db, failpoint, &[])
    }

    /// Like [`Server::spawn`], with more environment variables (for example
    /// `AURORA_MAX_RUNS`).
    pub fn spawn_with(db: &Path, failpoint: Option<&str>, env: &[(&str, &str)]) -> Server {
        let dir = db.parent().expect("a parent folder");
        let mut n = 0;
        let log = loop {
            let candidate = dir.join(format!("server-{n}.log"));
            if !candidate.exists() {
                break candidate;
            }
            n += 1;
        };
        let file = std::fs::File::create(&log).expect("log file");
        let mut command = Command::new(BIN);
        command
            .arg("--db")
            .arg(db)
            .args(["--port", "0"])
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .env_remove("AURORA_TEST_FAILPOINT")
            .stdin(Stdio::null())
            .stdout(file.try_clone().expect("clone"))
            .stderr(file);
        for (name, value) in env {
            command.env(name, value);
        }
        if let Some(spec) = failpoint {
            command.env("AURORA_TEST_FAILPOINT", spec);
        }
        let child = command.spawn().expect("spawn aurora");
        Server {
            child,
            addr: String::new(),
            log,
        }
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits until the server answers `/health`. `Err` carries the exit status
    /// and log when the process died first.
    pub fn wait_ready(&mut self, within: Duration) -> Result<(), String> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Err(format!("exited with {status}:\n{}", self.log_text()));
            }
            if let Some(addr) = listening_addr(&self.log_text())
                && http_ok(&addr)
            {
                self.addr = addr;
                return Ok(());
            }
            if started.elapsed() > within {
                return Err(format!("not ready in time:\n{}", self.log_text()));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `Some(status)` once the process has ended, waiting up to `within`.
    pub fn wait_exit(&mut self, within: Duration) -> Option<ExitStatus> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if started.elapsed() > within {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Hard kill (TerminateProcess / SIGKILL), no cleanup in the child.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

fn listening_addr(log: &str) -> Option<String> {
    let rest = log.split("addr=").nth(1)?;
    Some(rest.split_whitespace().next()?.to_owned())
}

fn http_ok(addr: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(addr) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return false;
    }
    let mut raw = String::new();
    stream.read_to_string(&mut raw).is_ok() && raw.starts_with("HTTP/1.1 200")
}

/// What `aurora check --db <db> --json` printed and returned.
pub struct CheckRun {
    pub code: i32,
    pub json: Value,
    pub stdout: String,
}

/// Runs the real binary's `check` command.
pub fn aurora_check(db: &Path) -> CheckRun {
    let output = Command::new(BIN)
        .arg("check")
        .arg("--db")
        .arg(db)
        .arg("--json")
        .env_remove("AURORA_TEST_FAILPOINT")
        .output()
        .expect("run aurora check");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    CheckRun {
        code: output.status.code().unwrap_or(-1),
        json: serde_json::from_str(&stdout).unwrap_or(Value::Null),
        stdout,
    }
}

/// Fails the test, with the failing checks, unless the library check and the
/// command both pass on `db`.
pub fn assert_checks_green(db: &Path, when: &str) {
    let report = aurora_store::invariants::check_file(db)
        .unwrap_or_else(|err| panic!("{when}: the library check could not open the db: {err}"));
    assert!(report.ok(), "{when}: failed checks: {:#?}", report.failed());
    let run = aurora_check(db);
    assert_eq!(run.code, 0, "{when}: `aurora check` said:\n{}", run.stdout);
    assert_eq!(run.json["ok"], true, "{when}: {}", run.stdout);
}

pub fn count(db: &Path, sql: &str) -> i64 {
    let conn = aurora_store::rusqlite::Connection::open_with_flags(
        db,
        aurora_store::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open read-only");
    conn.busy_timeout(Duration::from_secs(5)).expect("timeout");
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}
