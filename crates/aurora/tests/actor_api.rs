//! Every change made through the API records the signed-in user as the
//! event's actor: two users with their own keys, one change each in turn.

use aurora_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

struct Api {
    addr: std::net::SocketAddr,
}

impl Api {
    async fn call(&self, key: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!(
            "{method} /api/v1{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {key}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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

    /// Calls with `key` and expects a 2xx answer.
    async fn ok(&self, key: &str, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.call(key, method, path, body).await;
        assert!(
            (200..300).contains(&status),
            "{method} {path}: {status} {value}"
        );
        value
    }
}

#[tokio::test]
async fn each_change_names_the_user_whose_key_made_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = Api {
        addr: listener.local_addr().unwrap(),
    };
    // common::owner_key creates the owner as user 1.
    let owner = common::owner_key(&store).await;
    let owner_id = 1;
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(aurora::serve(
        listener,
        store.clone(),
        async {
            let _ = stopped.await;
        },
        aurora::ShutdownHooks::new(),
    ));

    let agent_user = api
        .ok(&owner, "POST", "/users", Some(json!({"name": "robot"})))
        .await;
    let agent_id = agent_user["id"].as_i64().unwrap();
    assert_ne!(agent_id, owner_id);
    let agent_key = api
        .ok(
            &owner,
            "POST",
            "/keys",
            Some(json!({"name": "robot key", "user_id": agent_id})),
        )
        .await["key"]
        .as_str()
        .unwrap()
        .to_string();
    let agent = agent_key.as_str();
    let owner = owner.as_str();

    // (event kind, user who made the change), in the order of the changes.
    let mut expected: Vec<(&str, i64)> = Vec::new();

    api.ok(
        owner,
        "POST",
        "/projects",
        Some(json!({"key": "TRK", "name": "T"})),
    )
    .await;
    expected.push(("project.created", owner_id));
    api.ok(
        agent,
        "PATCH",
        "/projects/TRK",
        Some(json!({"name": "Tracker"})),
    )
    .await;
    expected.push(("project.updated", agent_id));

    let review = api
        .ok(
            agent,
            "POST",
            "/projects/TRK/columns",
            Some(json!({"name": "Review", "state_type": "started"})),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    expected.push(("column.created", agent_id));
    api.ok(
        owner,
        "PATCH",
        &format!("/projects/TRK/columns/{review}"),
        Some(json!({"name": "QA"})),
    )
    .await;
    expected.push(("column.renamed", owner_id));
    let columns = api.ok(agent, "GET", "/projects/TRK/columns", None).await;
    let mut order: Vec<i64> = columns
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    order.reverse();
    api.ok(
        agent,
        "PUT",
        "/projects/TRK/columns/order",
        Some(json!({ "ids": order })),
    )
    .await;
    expected.push(("column.reordered", agent_id));
    let spare = api
        .ok(
            owner,
            "POST",
            "/projects/TRK/columns",
            Some(json!({"name": "Spare", "state_type": "started"})),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    expected.push(("column.created", owner_id));
    api.ok(
        agent,
        "DELETE",
        &format!("/projects/TRK/columns/{spare}"),
        None,
    )
    .await;
    expected.push(("column.deleted", agent_id));

    let task = api
        .ok(
            owner,
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    expected.push(("issue_type.created", owner_id));
    api.ok(
        agent,
        "PATCH",
        &format!("/projects/TRK/issue-types/{task}"),
        Some(json!({"name": "Chore"})),
    )
    .await;
    expected.push(("issue_type.renamed", agent_id));
    let spare_type = api
        .ok(
            agent,
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Spare"})),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    expected.push(("issue_type.created", agent_id));
    api.ok(
        owner,
        "DELETE",
        &format!("/projects/TRK/issue-types/{spare_type}"),
        None,
    )
    .await;
    expected.push(("issue_type.deleted", owner_id));

    let label = api
        .ok(
            owner,
            "POST",
            "/projects/TRK/labels",
            Some(json!({"name": "urgent"})),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    expected.push(("label.created", owner_id));
    api.ok(
        agent,
        "PATCH",
        &format!("/projects/TRK/labels/{label}"),
        Some(json!({"name": "p0", "color": "#ff0000"})),
    )
    .await;
    expected.push(("label.renamed", agent_id));
    expected.push(("label.recolored", agent_id));
    api.ok(
        owner,
        "DELETE",
        &format!("/projects/TRK/labels/{label}"),
        None,
    )
    .await;
    expected.push(("label.deleted", owner_id));

    api.ok(
        owner,
        "POST",
        "/projects/TRK/issues",
        Some(json!({"title": "x"})),
    )
    .await;
    expected.push(("issue.created", owner_id));
    api.ok(
        agent,
        "PATCH",
        "/projects/TRK/issues/1",
        Some(json!({"title": "y"})),
    )
    .await;
    expected.push(("issue.updated", agent_id));
    api.ok(
        owner,
        "POST",
        "/projects/TRK/issues/1/move",
        Some(json!({"column_id": review})),
    )
    .await;
    expected.push(("issue.moved", owner_id));
    api.ok(agent, "POST", "/projects/TRK/issues/1/archive", None)
        .await;
    expected.push(("issue.archived", agent_id));
    api.ok(owner, "POST", "/projects/TRK/issues/1/unarchive", None)
        .await;
    expected.push(("issue.unarchived", owner_id));

    let log = api
        .ok(owner, "GET", "/events?project=TRK&limit=200", None)
        .await;
    let actual: Vec<(&str, i64)> = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["kind"].as_str().unwrap(), e["actor"].as_i64().unwrap()))
        .collect();
    assert_eq!(actual, expected);

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
