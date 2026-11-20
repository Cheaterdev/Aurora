use aurora_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

struct Api {
    addr: std::net::SocketAddr,
}

impl Api {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!(
            "{method} /api/v1{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
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

fn numbers(list: &Value) -> Vec<i64> {
    list["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["number"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn issues_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = Api {
        addr: listener.local_addr().unwrap(),
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
    let backlog = project["columns"][0]["id"].as_i64().unwrap();
    let todo = project["columns"][1]["id"].as_i64().unwrap();
    let started = project["columns"][2]["id"].as_i64().unwrap();
    api.call(
        "POST",
        "/projects",
        Some(json!({"key": "OTH", "name": "O"})),
    )
    .await;
    let (_, other_type) = api
        .call(
            "POST",
            "/projects/OTH/issue-types",
            Some(json!({"name": "T"})),
        )
        .await;
    let other_type = other_type["id"].as_i64().unwrap();

    // No type yet: required, so 422.
    let (status, body) = api
        .call("POST", "/projects/TRK/issues", Some(json!({"title": "x"})))
        .await;
    assert_eq!(status, 422, "{body}");
    assert!(body["error"].is_string());
    let (_, task) = api
        .call(
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await;
    let task = task["id"].as_i64().unwrap();
    let (_, label) = api
        .call(
            "POST",
            "/projects/TRK/labels",
            Some(json!({"name": "urgent"})),
        )
        .await;
    let label = label["id"].as_i64().unwrap();

    // Create: defaults, then every field.
    let (status, first) = api
        .call(
            "POST",
            "/projects/TRK/issues",
            Some(json!({"title": " First "})),
        )
        .await;
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["number"], 1);
    assert_eq!(first["identifier"], "TRK-1");
    assert_eq!(first["title"], "First");
    assert_eq!(first["type_id"], task);
    assert_eq!(first["column_id"], backlog);
    assert_eq!(first["children"], json!([]));
    assert_eq!(first["parent"], Value::Null);
    assert_eq!(first["archived"], false);

    let (status, second) = api
        .call(
            "POST",
            "/projects/TRK/issues",
            Some(
                json!({"title": "Second", "description": "# hi", "type_id": task,
                        "column_id": todo, "parent": 1, "priority": "high",
                        "labels": [label]}),
            ),
        )
        .await;
    assert_eq!(status, 201, "{second}");
    assert_eq!(second["identifier"], "TRK-2");
    assert_eq!(second["parent"], 1);
    assert_eq!(second["priority"], "high");
    assert_eq!(second["labels"], json!([label]));
    assert_eq!(second["column_id"], todo);

    // Create refusals: 422 for bad input and references, 404 for a project.
    for bad in [
        json!({"title": ""}),
        json!({"title": "x", "type_id": other_type}),
        json!({"title": "x", "column_id": 99999}),
        json!({"title": "x", "parent": 99}),
        json!({"title": "x", "labels": [12345]}),
        json!({"title": "x", "priority": "asap"}),
        json!({"title": "x", "surprise": 1}),
        json!({}),
    ] {
        let (status, body) = api.call("POST", "/projects/TRK/issues", Some(bad)).await;
        assert_eq!(status, 422, "{body}");
        assert!(body["error"].is_string());
    }
    let (status, body) = api
        .call("POST", "/projects/NOPE/issues", Some(json!({"title": "x"})))
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("project not found"))
    );
    let (status, _) = api.call("POST", "/projects/TRK/issues", None).await;
    assert_eq!(status, 422);

    // Read: with children and a parent, and the 404s.
    let (status, one) = api.call("GET", "/projects/TRK/issues/1", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        one["children"],
        json!([{"number": 2, "title": "Second", "column_id": todo, "archived": false}])
    );
    let (status, body) = api.call("GET", "/projects/TRK/issues/99", None).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("issue not found"))
    );
    let (status, _) = api.call("GET", "/projects/NOPE/issues/1", None).await;
    assert_eq!(status, 404);

    // Update: PATCH semantics, refusals.
    let (status, updated) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/2",
            Some(json!({"title": "Renamed", "priority": null, "parent": null, "labels": []})),
        )
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["title"], "Renamed");
    assert_eq!(updated["priority"], Value::Null);
    assert_eq!(updated["parent"], Value::Null);
    assert_eq!(updated["description"], "# hi", "absent keeps");
    assert_eq!(updated["labels"], json!([]));
    let (status, same) = api
        .call("PATCH", "/projects/TRK/issues/2", Some(json!({})))
        .await;
    assert_eq!((status, same["title"].as_str()), (200, Some("Renamed")));
    for bad in [
        json!({"title": " "}),
        json!({"column_id": todo}),
        json!({"type_id": other_type}),
        json!({"priority": "nope"}),
    ] {
        let (status, body) = api.call("PATCH", "/projects/TRK/issues/2", Some(bad)).await;
        assert_eq!(status, 422, "{body}");
    }
    let (status, _) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/99",
            Some(json!({"title": "x"})),
        )
        .await;
    assert_eq!(status, 404);
    // 409: a cycle.
    api.call(
        "PATCH",
        "/projects/TRK/issues/2",
        Some(json!({"parent": 1})),
    )
    .await;
    let (status, body) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/1",
            Some(json!({"parent": 2})),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, _) = api
        .call(
            "PATCH",
            "/projects/TRK/issues/1",
            Some(json!({"parent": 1})),
        )
        .await;
    assert_eq!(status, 409);

    // Move.
    let (_, third) = api
        .call(
            "POST",
            "/projects/TRK/issues",
            Some(json!({"title": "Third"})),
        )
        .await;
    assert_eq!(third["number"], 3);
    let (status, moved) = api
        .call(
            "POST",
            "/projects/TRK/issues/3/move",
            Some(json!({"column_id": backlog, "before": 1})),
        )
        .await;
    assert_eq!(status, 200, "{moved}");
    let (_, list) = api
        .call(
            "GET",
            &format!("/projects/TRK/issues?column_id={backlog}"),
            None,
        )
        .await;
    assert_eq!(numbers(&list), [3, 1]);
    let (status, moved) = api
        .call(
            "POST",
            "/projects/TRK/issues/3/move",
            Some(json!({"column_id": started})),
        )
        .await;
    assert_eq!((status, moved["column_id"].clone()), (200, json!(started)));
    for bad in [
        json!({"column_id": 99999}),
        json!({"column_id": backlog, "before": 1, "after": 2}),
        json!({"column_id": backlog, "before": 99}),
        json!({"column_id": backlog, "before": 3}),
        json!({"before": 1}),
    ] {
        let (status, body) = api
            .call("POST", "/projects/TRK/issues/3/move", Some(bad))
            .await;
        assert_eq!(status, 422, "{body}");
    }
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/99/move",
            Some(json!({"column_id": backlog})),
        )
        .await;
    assert_eq!(status, 404);

    // Archive, list, unarchive.
    let (status, archived) = api
        .call("POST", "/projects/TRK/issues/3/archive", None)
        .await;
    assert_eq!((status, archived["archived"].clone()), (200, json!(true)));
    let (_, list) = api.call("GET", "/projects/TRK/issues", None).await;
    assert_eq!(numbers(&list), [1, 2]);
    assert_eq!(list["total"], 2);
    assert_eq!(
        (list["limit"].clone(), list["offset"].clone()),
        (json!(50), json!(0))
    );
    let (_, list) = api
        .call("GET", "/projects/TRK/issues?archived_only=true", None)
        .await;
    assert_eq!(numbers(&list), [3]);
    let (_, list) = api
        .call("GET", "/projects/TRK/issues?include_archived=true", None)
        .await;
    assert_eq!(list["total"], 3);
    let (status, _) = api
        .call("POST", "/projects/TRK/issues/99/archive", None)
        .await;
    assert_eq!(status, 404);
    let (status, back) = api
        .call("POST", "/projects/TRK/issues/3/unarchive", None)
        .await;
    assert_eq!((status, back["archived"].clone()), (200, json!(false)));
    let (status, _) = api
        .call("POST", "/projects/TRK/issues/99/unarchive", None)
        .await;
    assert_eq!(status, 404);

    // Numbers are never reused.
    api.call("POST", "/projects/TRK/issues/3/archive", None)
        .await;
    let (_, fourth) = api
        .call(
            "POST",
            "/projects/TRK/issues",
            Some(json!({"title": "Fourth"})),
        )
        .await;
    assert_eq!(fourth["number"], 4);

    // List filters and their errors.
    let (_, list) = api
        .call("GET", "/projects/TRK/issues?parent=none", None)
        .await;
    assert_eq!(numbers(&list), [1, 4]);
    let (_, list) = api.call("GET", "/projects/TRK/issues?parent=1", None).await;
    assert_eq!(numbers(&list), [2]);
    let (_, list) = api
        .call(
            "GET",
            "/projects/TRK/issues?state_type=started&include_archived=true",
            None,
        )
        .await;
    assert_eq!(numbers(&list), [3]);
    let (_, list) = api
        .call("GET", "/projects/TRK/issues?limit=1&offset=1", None)
        .await;
    assert_eq!((numbers(&list), list["total"].clone()), (vec![4], json!(3)));
    for bad in [
        "state_type=sideways",
        "parent=abc",
        "limit=many",
        "offset=-1",
        "colour=red",
        "include_archived=maybe",
    ] {
        let (status, body) = api
            .call("GET", &format!("/projects/TRK/issues?{bad}"), None)
            .await;
        assert_eq!(status, 422, "{bad}: {body}");
        assert!(body["error"].is_string());
    }
    let (status, _) = api.call("GET", "/projects/NOPE/issues", None).await;
    assert_eq!(status, 404);

    // Every change wrote its event.
    let kinds: Vec<String> = store
        .read(|c| {
            let mut stmt =
                c.prepare("SELECT kind FROM event WHERE entity_kind = 'issue' ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    for expected in [
        "issue.created",
        "issue.updated",
        "issue.moved",
        "issue.archived",
        "issue.unarchived",
    ] {
        assert!(kinds.iter().any(|k| k == expected), "no {expected} event");
    }
    assert_eq!(kinds.iter().filter(|k| *k == "issue.created").count(), 4);

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
