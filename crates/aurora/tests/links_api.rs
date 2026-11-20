//! Issue links over HTTP.

use aurora_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

struct Api {
    addr: std::net::SocketAddr,
    key: String,
}

impl Api {
    async fn call_as(
        &self,
        key: Option<&str>,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let auth = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!(
            "{method} /api/v1{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             {auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let status = raw.split(' ').nth(1).unwrap().parse().unwrap();
        let text = raw.split("\r\n\r\n").nth(1).unwrap_or("").trim();
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(text).unwrap_or_else(|_| panic!("not JSON: {raw}"))
        };
        (status, value)
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.call_as(Some(&self.key), method, path, body).await
    }
}

fn numbers(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v["number"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn links_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = Api {
        addr: listener.local_addr().unwrap(),
        key: common::owner_key(&store).await,
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(aurora::serve(
        listener,
        store.clone(),
        async {
            let _ = stopped.await;
        },
        aurora::ShutdownHooks::new(),
    ));

    let (_, project) = api
        .call(
            "POST",
            "/projects",
            Some(json!({"key": "TRK", "name": "T"})),
        )
        .await;
    let done = project["columns"][3]["id"].as_i64().unwrap();
    api.call(
        "POST",
        "/projects",
        Some(json!({"key": "OTH", "name": "O"})),
    )
    .await;
    for key in ["TRK", "OTH"] {
        api.call(
            "POST",
            &format!("/projects/{key}/issue-types"),
            Some(json!({"name": "Task"})),
        )
        .await;
        for title in ["one", "two", "three"] {
            let (status, _) = api
                .call(
                    "POST",
                    &format!("/projects/{key}/issues"),
                    Some(json!({"title": title})),
                )
                .await;
            assert_eq!(status, 201);
        }
    }

    // Create: 1 blocks 2.
    let (status, link) = api
        .call(
            "POST",
            "/projects/TRK/issues/1/links",
            Some(json!({"kind": "blocks", "to": 2})),
        )
        .await;
    assert_eq!(status, 201, "{link}");
    assert_eq!(link["kind"], "blocks");
    assert_eq!(link["from"]["identifier"], "TRK-1");
    assert_eq!(link["to"]["identifier"], "TRK-2");
    assert_eq!(link["created_by"], 1);
    let first = link["id"].as_i64().unwrap();

    // direction blocked_by: 3 blocks 1 (so the request is about the blocked issue).
    let (status, link) = api
        .call(
            "POST",
            "/projects/TRK/issues/1/links",
            Some(json!({"kind": "blocks", "to": 3, "direction": "blocked_by"})),
        )
        .await;
    assert_eq!(status, 201, "{link}");
    assert_eq!(link["from"]["number"], 3);
    assert_eq!(link["to"]["number"], 1);
    // `to` can be written as an identifier of the same project; relates works too.
    let (status, link) = api
        .call(
            "POST",
            "/projects/TRK/issues/2/links",
            Some(json!({"kind": "relates", "to": "TRK-3", "direction": "blocks"})),
        )
        .await;
    assert_eq!(status, 201, "{link}");
    assert_eq!(link["kind"], "relates");

    // Read: the list for an issue and the issue itself.
    let (status, list) = api.call("GET", "/projects/TRK/issues/1/links", None).await;
    assert_eq!(status, 200);
    assert_eq!(numbers(&list["blocks"]), [2]);
    assert_eq!(numbers(&list["blocked_by"]), [3]);
    assert_eq!(list["related"], json!([]));
    assert_eq!(list["blocks"][0]["link_id"], first);
    assert_eq!(list["blocks"][0]["identifier"], "TRK-2");
    assert_eq!(list["blocks"][0]["state_type"], "backlog");
    assert_eq!(list["blocks"][0]["archived"], false);
    let (_, issue) = api.call("GET", "/projects/TRK/issues/1", None).await;
    assert_eq!(issue["links"], list);
    assert_eq!(issue["blocked"], true, "3 blocks 1 and is open");
    assert_eq!(issue["blocker_count"], 1);
    let (_, three) = api.call("GET", "/projects/TRK/issues/3", None).await;
    assert_eq!(three["blocked"], false);
    // The list carries the flag too; earlier fields are unchanged.
    let (_, all) = api.call("GET", "/projects/TRK/issues", None).await;
    let flags: Vec<bool> = all["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["blocked"].as_bool().unwrap())
        .collect();
    assert_eq!(flags, [true, true, false]);
    assert_eq!(all["issues"][0]["title"], "one");

    // Blocked until the blocker is done.
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/3/move",
            Some(json!({"column_id": done})),
        )
        .await;
    assert_eq!(status, 200);
    let (_, issue) = api.call("GET", "/projects/TRK/issues/1", None).await;
    assert_eq!(issue["blocked"], false);
    assert_eq!(issue["blocker_count"], 0);

    // Refusals.
    let post = |body: Value| api.call("POST", "/projects/TRK/issues/1/links", Some(body));
    let (status, body) = post(json!({"kind": "blocks", "to": 1})).await;
    assert_eq!((status, body["error"].is_string()), (422, true), "{body}");
    let (status, _) = post(json!({"kind": "blocks", "to": 2})).await;
    assert_eq!(status, 409, "duplicate");
    let (status, body) = post(json!({"kind": "blocks", "to": 3, "direction": "blocked_by"})).await;
    assert_eq!(status, 409, "{body}");
    let (status, body) = post(json!({"kind": "relates", "to": 3, "direction": "blocked_by"})).await;
    assert_eq!(status, 422, "{body}");
    let (status, _) = post(json!({"kind": "blocks", "to": 99})).await;
    assert_eq!(status, 422, "unknown target");
    let (status, body) = post(json!({"kind": "blocks", "to": "OTH-1"})).await;
    assert_eq!(status, 422, "other project: {body}");
    assert!(body["error"].as_str().unwrap().contains("same project"));
    let (status, _) = post(json!({"kind": "blocks", "to": "nonsense"})).await;
    assert_eq!(status, 422);
    let (status, _) = post(json!({"kind": "friends", "to": 2})).await;
    assert_eq!(status, 422, "unknown kind");
    let (status, _) = post(json!({"kind": "blocks"})).await;
    assert_eq!(status, 422, "missing to");
    let (status, _) = post(json!({"kind": "blocks", "to": 2, "extra": 1})).await;
    assert_eq!(status, 422, "unknown field");
    // 3 blocks 1 and 1 blocks 2, so 2 blocking 3 would close a cycle.
    let (status, body) = api
        .call(
            "POST",
            "/projects/TRK/issues/2/links",
            Some(json!({"kind": "blocks", "to": 3})),
        )
        .await;
    assert_eq!(status, 409, "cycle: {body}");
    assert!(body["error"].as_str().unwrap().contains("cycle"));
    // 404s: unknown project, unknown issue.
    let (status, _) = api.call("GET", "/projects/NOPE/issues/1/links", None).await;
    assert_eq!(status, 404);
    let (status, _) = api.call("GET", "/projects/TRK/issues/99/links", None).await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/99/links",
            Some(json!({"kind": "blocks", "to": 1})),
        )
        .await;
    assert_eq!(status, 404);

    // Remove.
    let (status, body) = api
        .call("DELETE", &format!("/projects/TRK/links/{first}"), None)
        .await;
    assert_eq!((status, body), (204, Value::Null));
    let (status, _) = api
        .call("DELETE", &format!("/projects/TRK/links/{first}"), None)
        .await;
    assert_eq!(status, 404);
    // Another project cannot reach it.
    let (_, theirs) = api
        .call(
            "POST",
            "/projects/OTH/issues/1/links",
            Some(json!({"kind": "blocks", "to": 2})),
        )
        .await;
    let theirs = theirs["id"].as_i64().unwrap();
    let (status, _) = api
        .call("DELETE", &format!("/projects/TRK/links/{theirs}"), None)
        .await;
    assert_eq!(status, 404);
    let (status, _) = api.call("DELETE", "/projects/TRK/links/999", None).await;
    assert_eq!(status, 404);
    let (_, list) = api.call("GET", "/projects/TRK/issues/1/links", None).await;
    assert_eq!(list["blocks"], json!([]));

    // Events carry the link kinds and are filterable by prefix.
    let (_, events) = api.call("GET", "/events?kind_prefix=link.", None).await;
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "link.added").count(), 4);
    assert_eq!(kinds.iter().filter(|k| **k == "link.removed").count(), 1);
    assert_eq!(events["events"][0]["actor"], 1);

    // Authentication: no key, or an unknown one, is 401 on every route.
    for (method, path, body) in [
        ("GET", "/projects/TRK/issues/1/links", None),
        (
            "POST",
            "/projects/TRK/issues/1/links",
            Some(json!({"kind": "blocks", "to": 2})),
        ),
        ("DELETE", "/projects/TRK/links/1", None),
    ] {
        let (status, _) = api.call_as(None, method, path, body).await;
        assert_eq!(status, 401, "{method} {path}");
    }
    let (status, _) = api
        .call_as(
            Some("aur_not_a_real_key"),
            "GET",
            "/projects/TRK/issues/1/links",
            None,
        )
        .await;
    assert_eq!(status, 401);

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
