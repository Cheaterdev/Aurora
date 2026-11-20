//! Runs the real binary and stops it with a real console / termination signal.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
fn spawn_in_own_group(command: &mut Command) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;
    // Without its own group, Ctrl+Break would also hit this test process.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP).spawn()
}

#[cfg(not(windows))]
fn spawn_in_own_group(command: &mut Command) -> std::io::Result<Child> {
    command.spawn()
}

/// Delivers the platform's stop signal to the child.
#[cfg(windows)]
fn send_stop_signal(child: &Child) {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    // The child is its own process group leader, so its pid is the group id.
    // SAFETY: plain FFI call with integer arguments.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    assert_ne!(sent, 0, "GenerateConsoleCtrlEvent failed (no console?)");
}

#[cfg(not(windows))]
fn send_stop_signal(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn http_get(addr: &str, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    Some(raw)
}

fn listening_addr(log: &str) -> Option<String> {
    let rest = log.split("addr=").nth(1)?;
    let addr = rest.split_whitespace().next()?;
    Some(addr.to_owned())
}

#[test]
fn stop_signal_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("aurora.db");
    let log_path = dir.path().join("server.log");
    let log = std::fs::File::create(&log_path).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_aurora"));
    command
        .arg("--db")
        .arg(&db)
        .args(["--port", "0"])
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    let mut child = spawn_in_own_group(&mut command).unwrap();

    let read_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let started = Instant::now();
    let addr = loop {
        if let Some(addr) = listening_addr(&read_log())
            && http_get(&addr, "/health").is_some_and(|r| r.starts_with("HTTP/1.1 200"))
        {
            break addr;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("server exited early with {status}:\n{}", read_log());
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "server did not start:\n{}",
            read_log()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(addr.starts_with("127.0.0.1:"));

    let marker = dir.path().join("aurora.db.running");
    assert!(marker.exists(), "the marker exists while running");

    send_stop_signal(&child);

    let stopped = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if stopped.elapsed() > Duration::from_secs(8) {
            let _ = child.kill();
            panic!("server did not stop in time:\n{}", read_log());
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let log = read_log();
    assert!(status.success(), "exit status {status}:\n{log}");
    assert!(!marker.exists(), "the marker is removed:\n{log}");
    let wal = dir.path().join("aurora.db-wal");
    assert!(
        std::fs::metadata(&wal).map_or(true, |m| m.len() == 0),
        "the WAL is absent or empty"
    );
    for phase in [
        "shutdown signal received",
        "draining in-flight requests",
        "http server stopped",
        "closing the store",
        "shutdown complete",
    ] {
        assert!(log.contains(phase), "log lacks {phase:?}:\n{log}");
    }

    let store = aurora_store::Store::open(&db).unwrap();
    assert!(!store.open_report().unclean_shutdown);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.close());
}
