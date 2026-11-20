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
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let key = &self.key;
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
}

fn event_ids(body: &Value) -> Vec<i64> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn events_and_consumers_over_http() {
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

    // Empty log.
    let (status, body) = api.call("GET", "/events", None).await;
    assert_eq!((status, &body["events"]), (200, &json!([])));
    let (status, body) = api.call("GET", "/consumers", None).await;
    assert_eq!((status, body), (200, json!([])));

    // Three events: two projects and a type.
    api.call(
        "POST",
        "/projects",
        Some(json!({"key": "TRK", "name": "T"})),
    )
    .await;
    api.call(
        "POST",
        "/projects",
        Some(json!({"key": "OTH", "name": "O"})),
    )
    .await;
    api.call(
        "POST",
        "/projects/TRK/issue-types",
        Some(json!({"name": "Task"})),
    )
    .await;
    api.call("POST", "/projects/TRK/issues", Some(json!({"title": "x"})))
        .await;

    // GET /events: order, shape, after, limit, filters.
    let (status, body) = api.call("GET", "/events", None).await;
    assert_eq!(status, 200);
    assert_eq!(event_ids(&body), [1, 2, 3, 4]);
    let first = &body["events"][0];
    assert_eq!(first["kind"], "project.created");
    assert_eq!(first["entity_kind"], "project");
    assert_eq!(first["project"], "TRK");
    assert_eq!(first["payload"]["key"], "TRK");
    assert!(first["created"].is_i64());
    // common::owner_key creates the owner as user 1.
    assert_eq!(first["actor"], 1);
    let (_, body) = api.call("GET", "/events?after=2&limit=1", None).await;
    assert_eq!(event_ids(&body), [3]);
    let (_, body) = api.call("GET", "/events?project=OTH", None).await;
    assert_eq!(event_ids(&body), [2]);
    let (_, body) = api.call("GET", "/events?entity_kind=issue", None).await;
    assert_eq!(event_ids(&body), [4]);
    let (_, body) = api
        .call("GET", "/events?kind_prefix=issue_type.", None)
        .await;
    assert_eq!(event_ids(&body), [3]);
    for bad in [
        "/events?after=-1",
        "/events?after=abc",
        "/events?surprise=1",
    ] {
        let (status, body) = api.call("GET", bad, None).await;
        assert_eq!(status, 422, "{bad}: {body}");
        assert!(body["error"].is_string());
    }
    let (status, body) = api.call("GET", "/events?project=NOPE", None).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("project not found"))
    );

    // PUT /consumers/{name}: 201 then 200, cursor kept.
    let (status, body) = api
        .call(
            "PUT",
            "/consumers/runner",
            Some(json!({"start": "beginning"})),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["name"], "runner");
    assert_eq!(body["last_event_id"], 0);
    let (status, body) = api
        .call("PUT", "/consumers/runner", Some(json!({"start": "latest"})))
        .await;
    assert_eq!((status, &body["last_event_id"]), (200, &json!(0)));
    let (status, body) = api
        .call("PUT", "/consumers/live.1", Some(json!({"start": "latest"})))
        .await;
    assert_eq!((status, &body["last_event_id"]), (201, &json!(4)));
    for (path, body) in [
        ("/consumers/bad%20name", json!({"start": "beginning"})),
        ("/consumers/runner", json!({"start": "middle"})),
        ("/consumers/runner", json!({})),
        ("/consumers/runner", json!({"start": "latest", "x": 1})),
    ] {
        let (status, reply) = api.call("PUT", path, Some(body)).await;
        assert_eq!(status, 422, "{path}: {reply}");
    }
    let (status, _) = api.call("PUT", "/consumers/runner", None).await;
    assert_eq!(status, 422);

    // GET /consumers lists them by name.
    let (_, body) = api.call("GET", "/consumers", None).await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["live.1", "runner"]);

    // Poll does not advance; ack does.
    let (status, body) = api.call("GET", "/consumers/runner/events", None).await;
    assert_eq!(status, 200);
    assert_eq!(event_ids(&body), [1, 2, 3, 4]);
    assert_eq!(body["consumer"]["last_event_id"], 0);
    let (_, body) = api
        .call("GET", "/consumers/runner/events?limit=2", None)
        .await;
    assert_eq!(event_ids(&body), [1, 2]);
    let (_, again) = api.call("GET", "/consumers/runner/events", None).await;
    assert_eq!(event_ids(&again), [1, 2, 3, 4]);
    let (_, body) = api
        .call("GET", "/consumers/runner/events?kind_prefix=issue.", None)
        .await;
    assert_eq!(event_ids(&body), [4]);

    let (status, body) = api
        .call("POST", "/consumers/runner/ack", Some(json!({"up_to": 2})))
        .await;
    assert_eq!((status, &body["last_event_id"]), (200, &json!(2)));
    let (_, body) = api.call("GET", "/consumers/runner/events", None).await;
    assert_eq!(event_ids(&body), [3, 4]);
    let (status, body) = api
        .call("POST", "/consumers/runner/ack", Some(json!({"up_to": 1})))
        .await;
    assert_eq!(
        (status, &body["last_event_id"]),
        (200, &json!(2)),
        "an older ack is a no-op"
    );

    // Ack and poll refusals.
    for (path, body) in [
        ("/consumers/runner/ack", json!({"up_to": 5})),
        ("/consumers/runner/ack", json!({"up_to": -1})),
        ("/consumers/runner/ack", json!({"up_to": "two"})),
        ("/consumers/runner/ack", json!({})),
        ("/consumers/bad%20name/ack", json!({"up_to": 1})),
    ] {
        let (status, reply) = api.call("POST", path, Some(body)).await;
        assert_eq!(status, 422, "{path}: {reply}");
        assert!(reply["error"].is_string());
    }
    let (status, _) = api.call("GET", "/consumers/bad%20name/events", None).await;
    assert_eq!(status, 422);
    let (status, _) = api.call("DELETE", "/consumers/bad%20name", None).await;
    assert_eq!(status, 422);
    let (status, _) = api
        .call("GET", "/consumers/runner/events?limit=x", None)
        .await;
    assert_eq!(status, 422);
    let (_, body) = api.call("GET", "/consumers/runner/events", None).await;
    assert_eq!(event_ids(&body), [3, 4], "refused acks changed nothing");

    // Unknown consumer: 404 everywhere.
    let (status, body) = api.call("GET", "/consumers/ghost/events", None).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("consumer not found"))
    );
    let (status, _) = api
        .call("POST", "/consumers/ghost/ack", Some(json!({"up_to": 0})))
        .await;
    assert_eq!(status, 404);
    let (status, _) = api.call("DELETE", "/consumers/ghost", None).await;
    assert_eq!(status, 404);

    // DELETE: 204, and the consumer is gone.
    let (status, body) = api.call("DELETE", "/consumers/runner", None).await;
    assert_eq!((status, body), (204, Value::Null));
    let (status, _) = api.call("GET", "/consumers/runner/events", None).await;
    assert_eq!(status, 404);
    let (_, body) = api.call("GET", "/consumers", None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
