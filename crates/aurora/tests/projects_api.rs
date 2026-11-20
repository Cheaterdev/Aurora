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

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect()
}

fn ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn projects_columns_issue_types_and_labels_over_http() {
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

    // Projects and their default columns.
    let (status, project) = api
        .call(
            "POST",
            "/projects",
            Some(json!({"key": "TRK", "name": " Tracker "})),
        )
        .await;
    assert_eq!(status, 201, "{project}");
    assert_eq!(project["key"], "TRK");
    assert_eq!(project["name"], "Tracker");
    assert_eq!(
        names(&project["columns"]),
        ["Backlog", "Todo", "In Progress", "Done", "Canceled"]
    );
    let (status, listed) = api.call("GET", "/projects", None).await;
    assert_eq!(status, 200);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let (status, one) = api.call("GET", "/projects/TRK", None).await;
    assert_eq!((status, one["name"].as_str()), (200, Some("Tracker")));
    let (status, renamed) = api
        .call("PATCH", "/projects/TRK", Some(json!({"name": "Aurora"})))
        .await;
    assert_eq!((status, renamed["key"].as_str()), (200, Some("TRK")));
    let (status, body) = api
        .call("PATCH", "/projects/TRK", Some(json!({"key": "NEW"})))
        .await;
    assert_eq!(status, 422, "the key cannot be changed: {body}");

    let (status, body) = api
        .call(
            "POST",
            "/projects",
            Some(json!({"key": "TRK", "name": "Again"})),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].is_string());
    let (status, _) = api
        .call(
            "POST",
            "/projects",
            Some(json!({"key": "bad", "name": "x"})),
        )
        .await;
    assert_eq!(status, 422);
    let (status, _) = api
        .call(
            "POST",
            "/projects",
            Some(json!({"key": "ZZ", "name": "x", "columns": []})),
        )
        .await;
    assert_eq!(status, 422);
    let (status, body) = api
        .call("POST", "/projects", Some(json!({"key": "ZZ"})))
        .await;
    assert_eq!(status, 422, "missing field: {body}");
    let (status, body) = api.call("GET", "/projects/NOPE", None).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"], "project not found");

    // A project with its own columns.
    let (status, own) = api
        .call(
            "POST",
            "/projects",
            Some(json!({"key": "OWN", "name": "Own", "columns": [
                {"name": "Open", "state_type": "unstarted"},
                {"name": "Shipped", "state_type": "completed"}]})),
        )
        .await;
    assert_eq!(status, 201, "{own}");
    assert_eq!(names(&own["columns"]), ["Open", "Shipped"]);

    // Columns: rename, create, reorder, filter.
    let columns = project["columns"].clone();
    let todo = columns[1]["id"].as_i64().unwrap();
    let (status, col) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/columns/{todo}"),
            Some(json!({"name": "To Do"})),
        )
        .await;
    assert_eq!((status, col["name"].as_str()), (200, Some("To Do")));
    assert_eq!(col["state_type"], "unstarted");
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/columns/{todo}"),
            Some(json!({"state_type": "started"})),
        )
        .await;
    assert_eq!(status, 422, "the state type is fixed");
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/columns/{todo}"),
            Some(json!({"name": "Done"})),
        )
        .await;
    assert_eq!(status, 409);
    let (status, _) = api
        .call(
            "PATCH",
            "/projects/TRK/columns/99999",
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(status, 404);

    let (status, review) = api
        .call(
            "POST",
            "/projects/TRK/columns",
            Some(json!({"name": "Review", "state_type": "started"})),
        )
        .await;
    assert_eq!(status, 201, "{review}");
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/columns",
            Some(json!({"name": "Odd", "state_type": "sideways"})),
        )
        .await;
    assert_eq!(status, 422);

    let (_, started) = api
        .call("GET", "/projects/TRK/columns?state_type=started", None)
        .await;
    assert_eq!(names(&started), ["In Progress", "Review"]);
    let (status, _) = api
        .call("GET", "/projects/TRK/columns?state_type=nope", None)
        .await;
    assert_eq!(status, 422);

    let (_, all) = api.call("GET", "/projects/TRK/columns", None).await;
    let mut order = ids(&all);
    order.reverse();
    let (status, reordered) = api
        .call(
            "PUT",
            "/projects/TRK/columns/order",
            Some(json!({"ids": order})),
        )
        .await;
    assert_eq!(status, 200, "{reordered}");
    assert_eq!(ids(&reordered), order);
    let (_, again) = api.call("GET", "/projects/TRK/columns", None).await;
    assert_eq!(ids(&again), order);
    let (status, _) = api
        .call(
            "PUT",
            "/projects/TRK/columns/order",
            Some(json!({"ids": [order[0]]})),
        )
        .await;
    assert_eq!(status, 422, "an incomplete list is refused");

    // Deleting a column that still has an issue is refused (409).
    let (status, bug) = api
        .call(
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Bug"})),
        )
        .await;
    assert_eq!(status, 201, "{bug}");
    let project_id = project["id"].as_i64().unwrap();
    let bug_id = bug["id"].as_i64().unwrap();
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO issue (project_id, number, type_id, column_id, title, position, created, updated)
                 VALUES (?1, 1, ?2, ?3, 'T', 1.0, 1, 1)",
                [project_id, bug_id, todo],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (status, body) = api
        .call("DELETE", &format!("/projects/TRK/columns/{todo}"), None)
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, body) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/issue-types/{bug_id}"),
            None,
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let review_id = review["id"].as_i64().unwrap();
    let (status, _) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/columns/{review_id}"),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/columns/{review_id}"),
            None,
        )
        .await;
    assert_eq!(status, 404);

    // Issue types.
    let (status, task) = api
        .call(
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await;
    assert_eq!(status, 201);
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await;
    assert_eq!(status, 409);
    let task_id = task["id"].as_i64().unwrap();
    let (status, renamed) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/issue-types/{task_id}"),
            Some(json!({"name": "Chore"})),
        )
        .await;
    assert_eq!((status, renamed["name"].as_str()), (200, Some("Chore")));
    let (_, types) = api.call("GET", "/projects/TRK/issue-types", None).await;
    assert_eq!(names(&types), ["Bug", "Chore"]);
    let (status, _) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/issue-types/{task_id}"),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = api.call("GET", "/projects/NOPE/issue-types", None).await;
    assert_eq!(status, 404);

    // Labels.
    let (status, label) = api
        .call(
            "POST",
            "/projects/TRK/labels",
            Some(json!({"name": "urgent", "color": "#FF0000"})),
        )
        .await;
    assert_eq!(status, 201, "{label}");
    assert_eq!(label["color"], "#ff0000");
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/labels",
            Some(json!({"name": "x", "color": "red"})),
        )
        .await;
    assert_eq!(status, 422);
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/labels",
            Some(json!({"name": "urgent"})),
        )
        .await;
    assert_eq!(status, 409);
    let label_id = label["id"].as_i64().unwrap();
    let (status, updated) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/labels/{label_id}"),
            Some(json!({"name": "p0", "color": "#00ff00"})),
        )
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(
        (updated["name"].as_str(), updated["color"].as_str()),
        (Some("p0"), Some("#00ff00"))
    );
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/labels/{label_id}"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 422);
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/labels/{label_id}"),
            Some(json!({"name": "renamed", "color": "bad"})),
        )
        .await;
    assert_eq!(status, 422);
    let (_, labels) = api.call("GET", "/projects/TRK/labels", None).await;
    assert_eq!(names(&labels), ["p0"], "the failed patch changed nothing");
    let (status, _) = api
        .call("DELETE", &format!("/projects/TRK/labels/{label_id}"), None)
        .await;
    assert_eq!(status, 204);
    let (_, labels) = api.call("GET", "/projects/TRK/labels", None).await;
    assert!(labels.as_array().unwrap().is_empty());

    // Malformed bodies get the JSON error body too.
    let (status, body) = api.call("POST", "/projects", None).await;
    assert_eq!(status, 422, "{body}");
    assert!(body["error"].is_string());

    // Every change above wrote its event.
    let kinds: Vec<String> = store
        .read(|c| {
            let mut stmt = c.prepare("SELECT kind FROM event ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(kinds.iter().filter(|k| *k == "project.created").count(), 2);
    for expected in [
        "project.updated",
        "column.created",
        "column.renamed",
        "column.reordered",
        "column.deleted",
        "issue_type.created",
        "issue_type.renamed",
        "issue_type.deleted",
        "label.created",
        "label.renamed",
        "label.recolored",
        "label.deleted",
    ] {
        assert!(kinds.iter().any(|k| k == expected), "no {expected} event");
    }

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
