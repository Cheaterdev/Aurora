//! The REST route table: the router really serves what the table says, the
//! table is what `docs/api.md` documents, and the routes added by the TRK-12
//! audit work.

use std::collections::BTreeSet;

use aurora::routes::api_endpoints;
use aurora_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

const DOCS: &str = include_str!("../../../docs/api.md");
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

struct Server {
    addr: std::net::SocketAddr,
    key: String,
    _dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
}

impl Server {
    async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        let key = common::owner_key(&store).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(aurora::serve(
            listener,
            store,
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
        ));
        Server {
            addr,
            key,
            _dir: dir,
            stop: Some(stop),
        }
    }

    /// Status and body of a request made with the owner's key.
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.key,
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let status = raw.split(' ').nth(1).unwrap().parse().unwrap();
        let text = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, text)
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, text) = self.call(method, path, body).await;
        assert!(
            (200..300).contains(&status),
            "{method} {path}: {status} {text}"
        );
        serde_json::from_str(text.trim()).unwrap_or(Value::Null)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Fills the `{placeholders}` with values that parse but name nothing.
fn concrete(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .map_or(rest.len(), |e| start + e + 1);
        out.push_str(if &rest[start..end] == "{key}" {
            "NOPE"
        } else {
            "999999"
        });
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn full_path(path: &str) -> String {
    if path == "/" {
        "/api/v1".to_string()
    } else {
        format!("/api/v1{}", concrete(path))
    }
}

/// How the documentation names an endpoint.
fn doc_name(method: &str, path: &str) -> String {
    let path = if path == "/" { "" } else { path };
    format!("{method} /api/v1{path}")
}

#[tokio::test]
async fn the_router_answers_exactly_the_methods_in_the_route_table() {
    let server = Server::start().await;
    let endpoints = api_endpoints();
    let paths: BTreeSet<&str> = endpoints.iter().map(|e| e.path).collect();
    assert!(paths.len() > 40, "the table looks empty: {}", paths.len());
    for path in paths {
        for method in METHODS {
            let declared = endpoints
                .iter()
                .any(|e| e.path == path && e.method == method);
            let (status, text) = server.call(method, &full_path(path), None).await;
            if declared {
                assert_ne!(
                    status, 405,
                    "{method} {path} is in the table but the router refuses the method"
                );
            } else {
                assert_eq!(
                    status, 405,
                    "{method} {path} is served by the router but missing from the route table: {text}"
                );
            }
        }
    }
}

#[tokio::test]
async fn every_endpoint_is_documented_and_every_documented_endpoint_exists() {
    let endpoints = api_endpoints();
    for e in &endpoints {
        let heading = format!("### `{}`", doc_name(e.method, e.path));
        assert!(
            DOCS.lines().any(|line| line.trim() == heading),
            "docs/api.md has no heading {heading}"
        );
    }
    let known: BTreeSet<String> = endpoints
        .iter()
        .map(|e| doc_name(e.method, e.path))
        .collect();
    for line in DOCS.lines() {
        if let Some(name) = line
            .strip_prefix("### `")
            .and_then(|rest| rest.strip_suffix('`'))
        {
            assert!(
                known.contains(name),
                "docs/api.md documents {name}, which is not a route"
            );
        }
    }
}

#[tokio::test]
async fn the_index_lists_the_route_table_and_needs_a_key() {
    let server = Server::start().await;
    let index = server.ok("GET", "/api/v1", None).await;
    assert_eq!(index["mcp"], "/mcp");
    let listed = index["endpoints"].as_array().unwrap();
    assert_eq!(listed.len(), api_endpoints().len());
    assert!(listed.iter().any(|e| e["method"] == "GET"
        && e["path"] == "/projects/{key}/issues/{number}/events"
        && e["public"] == false));

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /api/v1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    assert!(raw.starts_with("HTTP/1.1 401"), "{raw}");
}

#[tokio::test]
async fn a_single_comment_and_the_activity_of_an_issue_can_be_read() {
    let server = Server::start().await;
    server
        .ok(
            "POST",
            "/api/v1/projects",
            Some(json!({"key": "TRK", "name": "T"})),
        )
        .await;
    let issue_type = server
        .ok(
            "POST",
            "/api/v1/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await;
    let type_id = issue_type["id"].as_i64().unwrap();
    server
        .ok(
            "POST",
            "/api/v1/projects/TRK/issues",
            Some(json!({"title": "One", "type_id": type_id})),
        )
        .await;
    let comment = server
        .ok(
            "POST",
            "/api/v1/projects/TRK/issues/1/comments",
            Some(json!({"body": "hello"})),
        )
        .await;
    let id = comment["id"].as_i64().unwrap();

    let found = server
        .ok("GET", &format!("/api/v1/projects/TRK/comments/{id}"), None)
        .await;
    assert_eq!(found["body"], "hello");
    assert_eq!(found["issue"]["identifier"], "TRK-1");
    let (status, _) = server
        .call("GET", "/api/v1/projects/TRK/comments/999", None)
        .await;
    assert_eq!(status, 404);

    let activity = server
        .ok("GET", "/api/v1/projects/TRK/issues/1/events", None)
        .await;
    let kinds: Vec<&str> = activity["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"issue.created"), "{kinds:?}");
    let limited = server
        .ok("GET", "/api/v1/projects/TRK/issues/1/events?limit=1", None)
        .await;
    assert_eq!(limited["events"].as_array().unwrap().len(), 1);
    let (status, _) = server
        .call("GET", "/api/v1/projects/TRK/issues/99/events", None)
        .await;
    assert_eq!(status, 404);
}
