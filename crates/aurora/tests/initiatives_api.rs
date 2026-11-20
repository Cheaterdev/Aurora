//! Initiatives over HTTP: routes, status codes, progress and the issue field.

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

fn numbers(detail: &Value) -> Vec<i64> {
    detail["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["number"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn initiatives_over_http() {
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
    let (_, read_key) = api
        .call(
            "POST",
            "/keys",
            Some(json!({"name": "ro", "scope": "read"})),
        )
        .await;
    let read_key = read_key["key"].as_str().unwrap().to_string();

    // Create: 201 with defaults, progress and health.
    let (status, made) = api
        .call(
            "POST",
            "/projects/TRK/initiatives",
            Some(json!({"name": "Launch"})),
        )
        .await;
    assert_eq!(status, 201, "{made}");
    let id = made["id"].as_i64().unwrap();
    assert_eq!(made["name"], "Launch");
    assert_eq!(made["status"], "planned");
    assert_eq!(made["description"], "");
    assert!(made["target_date"].is_null());
    assert_eq!(made["progress"]["total"], 0);
    assert_eq!(made["progress"]["percent"], 0);
    assert_eq!(made["progress"]["done_ratio"], 0.0);
    assert_eq!(made["health"], "none");

    let (status, second) = api
        .call(
            "POST",
            "/projects/TRK/initiatives",
            Some(json!({
                "name": "Beta",
                "description": "the beta",
                "status": "active",
                "target_date": "2999-12-31"
            })),
        )
        .await;
    assert_eq!(status, 201, "{second}");
    assert_eq!(second["target_date"], "2999-12-31");
    assert_eq!(second["health"], "on_track");
    let second_id = second["id"].as_i64().unwrap();

    // Assign two issues through the issue API, move one to Done: 1 of 2.
    let (status, issue) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/1",
            Some(json!({"initiative": id})),
        )
        .await;
    assert_eq!(status, 200, "{issue}");
    assert_eq!(issue["initiative_id"], id);
    let (status, issue) = api
        .call(
            "POST",
            "/projects/TRK/issues",
            Some(json!({"title": "four", "initiative": id})),
        )
        .await;
    assert_eq!(status, 201, "{issue}");
    assert_eq!(issue["initiative_id"], id);
    assert_eq!(issue["number"], 4);
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/1/move",
            Some(json!({"column_id": done})),
        )
        .await;
    assert_eq!(status, 200);

    let (status, detail) = api
        .call("GET", &format!("/projects/TRK/initiatives/{id}"), None)
        .await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["progress"]["total"], 2);
    assert_eq!(detail["progress"]["completed"], 1);
    assert_eq!(detail["progress"]["done"], 1);
    assert_eq!(detail["progress"]["open"], 1);
    assert_eq!(detail["progress"]["done_ratio"], 0.5);
    assert_eq!(detail["progress"]["percent"], 50);
    assert_eq!(detail["total"], 2);
    assert_eq!(
        (detail["limit"].as_i64(), detail["offset"].as_i64()),
        (Some(50), Some(0))
    );
    assert_eq!(numbers(&detail), [4, 1]);
    let first = &detail["issues"][1];
    assert_eq!(first["identifier"], "TRK-1");
    assert_eq!(first["title"], "one");
    assert_eq!(first["column_id"], done);
    assert_eq!(first["state_type"], "completed");
    let (_, paged) = api
        .call(
            "GET",
            &format!("/projects/TRK/initiatives/{id}?limit=1&offset=1"),
            None,
        )
        .await;
    assert_eq!(numbers(&paged), [1]);
    assert_eq!(
        (paged["total"].as_i64(), paged["limit"].as_i64()),
        (Some(2), Some(1))
    );
    let (_, clamped) = api
        .call(
            "GET",
            &format!("/projects/TRK/initiatives/{id}?limit=9999"),
            None,
        )
        .await;
    assert_eq!(clamped["limit"], 200);
    for bad in ["offset=-1", "limit=abc", "colour=red"] {
        let (status, err) = api
            .call(
                "GET",
                &format!("/projects/TRK/initiatives/{id}?{bad}"),
                None,
            )
            .await;
        assert_eq!(status, 422, "{bad}: {err}");
        assert!(err["error"].is_string());
    }

    // The issue shows its initiative; archived issues drop out of the counts.
    let (_, issue) = api.call("GET", "/projects/TRK/issues/4", None).await;
    assert_eq!(issue["initiative_id"], id);
    let (status, _) = api
        .call("POST", "/projects/TRK/issues/4/archive", None)
        .await;
    assert_eq!(status, 200);
    let (_, detail) = api
        .call("GET", &format!("/projects/TRK/initiatives/{id}"), None)
        .await;
    assert_eq!(
        (
            detail["progress"]["total"].as_i64(),
            detail["progress"]["percent"].as_i64()
        ),
        (Some(1), Some(100))
    );
    assert_eq!(numbers(&detail), [1]);

    // List: both, with progress and health; the status filter.
    let (status, list) = api.call("GET", "/projects/TRK/initiatives", None).await;
    assert_eq!(status, 200);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], id);
    assert_eq!(list[0]["progress"]["total"], 1);
    assert_eq!(list[1]["health"], "on_track");
    let (_, active) = api
        .call("GET", "/projects/TRK/initiatives?status=active", None)
        .await;
    assert_eq!(active.as_array().unwrap().len(), 1);
    assert_eq!(active[0]["id"], second_id);
    let (status, err) = api
        .call("GET", "/projects/TRK/initiatives?status=done", None)
        .await;
    assert_eq!(status, 422, "{err}");
    let (_, none) = api.call("GET", "/projects/OTH/initiatives", None).await;
    assert_eq!(none, json!([]));

    // Patch: fields change, null clears, a no-op is still 200.
    let (status, patched) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/initiatives/{second_id}"),
            Some(json!({"name": "Beta 2", "status": "paused", "target_date": null, "description": null})),
        )
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["name"], "Beta 2");
    assert_eq!(patched["status"], "paused");
    assert!(patched["target_date"].is_null());
    assert_eq!(patched["description"], "");
    assert_eq!(patched["health"], "none");
    let (status, same) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/initiatives/{second_id}"),
            Some(json!({})),
        )
        .await;
    assert_eq!((status, same["updated"] == patched["updated"]), (200, true));

    // Validation: 422.
    for body in [
        json!({"name": ""}),
        json!({"name": "x".repeat(201)}),
        json!({"description": "d".repeat(20_001)}),
        json!({"target_date": "2026-02-30"}),
        json!({"target_date": "31/12/2026"}),
        json!({"status": "done"}),
        json!({"colour": "red"}),
    ] {
        let (status, err) = api
            .call(
                "PATCH",
                &format!("/projects/TRK/initiatives/{id}"),
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, 422, "{body}: {err}");
        assert!(err["error"].is_string());
    }
    for body in [
        json!({}),
        json!({"name": "  "}),
        json!({"name": "ok", "target_date": "soon"}),
        json!({"name": "ok", "status": "nope"}),
        json!({"name": "ok", "extra": 1}),
    ] {
        let (status, err) = api
            .call("POST", "/projects/TRK/initiatives", Some(body.clone()))
            .await;
        assert_eq!(status, 422, "{body}: {err}");
    }

    // Conflict: a name used in the project, on create and on rename.
    let (status, err) = api
        .call(
            "POST",
            "/projects/TRK/initiatives",
            Some(json!({"name": "Launch"})),
        )
        .await;
    assert_eq!(status, 409, "{err}");
    assert!(err["error"].as_str().unwrap().contains("Launch"));
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/initiatives/{second_id}"),
            Some(json!({"name": "Launch"})),
        )
        .await;
    assert_eq!(status, 409);
    let (status, _) = api
        .call(
            "POST",
            "/projects/OTH/initiatives",
            Some(json!({"name": "Launch"})),
        )
        .await;
    assert_eq!(status, 201, "the same name in another project is fine");
    let (_, theirs) = api.call("GET", "/projects/OTH/initiatives", None).await;
    let theirs = theirs[0]["id"].as_i64().unwrap();

    // 404: unknown id, unknown project, an initiative of another project.
    for (method, path) in [
        ("GET", format!("/projects/TRK/initiatives/{}", id + 100)),
        ("PATCH", format!("/projects/TRK/initiatives/{}", id + 100)),
        ("DELETE", format!("/projects/TRK/initiatives/{}", id + 100)),
        ("GET", format!("/projects/TRK/initiatives/{theirs}")),
        ("PATCH", format!("/projects/TRK/initiatives/{theirs}")),
        ("DELETE", format!("/projects/TRK/initiatives/{theirs}")),
        ("GET", format!("/projects/NOPE/initiatives/{id}")),
        ("GET", "/projects/NOPE/initiatives".to_string()),
    ] {
        let body = (method == "PATCH").then(|| json!({"name": "x"}));
        let (status, err) = api.call(method, &path, body).await;
        assert_eq!(status, 404, "{method} {path}: {err}");
        assert!(err["error"].is_string());
    }
    let (status, _) = api
        .call(
            "POST",
            "/projects/NOPE/initiatives",
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(status, 404);

    // An issue body naming another project's or an unknown initiative: 422.
    for target in [theirs, 99_999] {
        let (status, err) = api
            .call(
                "PATCH",
                "/projects/TRK/issues/2",
                Some(json!({"initiative": target})),
            )
            .await;
        assert_eq!(status, 422, "{err}");
        let (status, err) = api
            .call(
                "POST",
                "/projects/TRK/issues",
                Some(json!({"title": "x", "initiative": target})),
            )
            .await;
        assert_eq!(status, 422, "{err}");
    }
    // null clears, and another issue joins and leaves.
    let (status, issue) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/2",
            Some(json!({"initiative": id})),
        )
        .await;
    assert_eq!((status, &issue["initiative_id"]), (200, &json!(id)));
    let (status, issue) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/2",
            Some(json!({"initiative": null})),
        )
        .await;
    assert_eq!(status, 200);
    assert!(issue["initiative_id"].is_null());

    // Authentication and scope: no key 401; a read-only key reads but cannot write.
    for (method, path, body) in [
        ("GET", "/projects/TRK/initiatives".to_string(), None),
        ("GET", format!("/projects/TRK/initiatives/{id}"), None),
        (
            "POST",
            "/projects/TRK/initiatives".to_string(),
            Some(json!({"name": "x"})),
        ),
        (
            "PATCH",
            format!("/projects/TRK/initiatives/{id}"),
            Some(json!({"name": "x"})),
        ),
        ("DELETE", format!("/projects/TRK/initiatives/{id}"), None),
    ] {
        let (status, err) = api.call_as(None, method, &path, body.clone()).await;
        assert_eq!(status, 401, "{method} {path}: {err}");
        let (status, err) = api
            .call_as(Some("aur_not_a_key"), method, &path, body)
            .await;
        assert_eq!(status, 401, "{method} {path}: {err}");
    }
    for path in [
        "/projects/TRK/initiatives".to_string(),
        format!("/projects/TRK/initiatives/{id}"),
    ] {
        let (status, _) = api.call_as(Some(&read_key), "GET", &path, None).await;
        assert_eq!(status, 200, "{path}");
    }
    for (method, path, body) in [
        (
            "POST",
            "/projects/TRK/initiatives".to_string(),
            Some(json!({"name": "ro"})),
        ),
        (
            "PATCH",
            format!("/projects/TRK/initiatives/{id}"),
            Some(json!({"name": "ro"})),
        ),
        ("DELETE", format!("/projects/TRK/initiatives/{id}"), None),
    ] {
        let (status, err) = api.call_as(Some(&read_key), method, &path, body).await;
        assert_eq!(status, 403, "{method}: {err}");
        assert!(err["error"].is_string());
    }
    let (_, unchanged) = api
        .call("GET", &format!("/projects/TRK/initiatives/{id}"), None)
        .await;
    assert_eq!(unchanged["name"], "Launch");

    // Delete: 204, the issues stay and are unassigned, the initiative is gone.
    let (status, body) = api
        .call("DELETE", &format!("/projects/TRK/initiatives/{id}"), None)
        .await;
    assert_eq!((status, body), (204, Value::Null));
    let (_, issue) = api.call("GET", "/projects/TRK/issues/1", None).await;
    assert!(issue["initiative_id"].is_null());
    assert_eq!(issue["title"], "one");
    let (status, _) = api
        .call("GET", &format!("/projects/TRK/initiatives/{id}"), None)
        .await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call("DELETE", &format!("/projects/TRK/initiatives/{id}"), None)
        .await;
    assert_eq!(status, 404);
    let (_, list) = api.call("GET", "/projects/TRK/initiatives", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // The events are on the log with the actor and the unassigned count.
    let (_, log) = api
        .call("GET", "/events?kind_prefix=initiative.&limit=200", None)
        .await;
    let kinds: Vec<&str> = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"initiative.created"));
    assert!(kinds.contains(&"initiative.updated"));
    let deleted = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "initiative.deleted")
        .unwrap();
    assert_eq!(deleted["payload"]["name"], "Launch");
    // Issue 1 and the archived issue 4 were assigned.
    assert_eq!(deleted["payload"]["unassigned"], 2);
    assert_eq!(deleted["actor"], 1);

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
