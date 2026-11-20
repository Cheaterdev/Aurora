use aurora_store::{Store, latest_version};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[tokio::test]
async fn health_reports_status_and_versions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(aurora::serve(listener, store, async {
        let _ = stopped.await;
    }));

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();

    assert!(
        raw.starts_with("HTTP/1.1 200"),
        "unexpected response: {raw}"
    );
    let body = raw.split("\r\n\r\n").nth(1).unwrap().trim();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["schema_version"], latest_version());
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
