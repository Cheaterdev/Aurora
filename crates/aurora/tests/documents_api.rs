//! Documents over HTTP: routes, status codes, revisions, links and the preview.

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

#[tokio::test]
async fn documents_over_http() {
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

    for key in ["TRK", "OTH"] {
        api.call("POST", "/projects", Some(json!({"key": key, "name": key})))
            .await;
        api.call(
            "POST",
            &format!("/projects/{key}/issue-types"),
            Some(json!({"name": "Task"})),
        )
        .await;
        let (status, _) = api
            .call(
                "POST",
                &format!("/projects/{key}/issues"),
                Some(json!({"title": "an issue"})),
            )
            .await;
        assert_eq!(status, 201);
    }
    let (_, read_key) = api
        .call(
            "POST",
            "/keys",
            Some(json!({"name": "ro", "scope": "read"})),
        )
        .await;
    let read_key = read_key["key"].as_str().unwrap().to_string();
    let (status, initiative) = api
        .call(
            "POST",
            "/projects/TRK/initiatives",
            Some(json!({"name": "Launch"})),
        )
        .await;
    assert_eq!(status, 201, "{initiative}");
    let initiative_id = initiative["id"].as_i64().unwrap();

    // Authentication and scope.
    let (status, _) = api
        .call_as(None, "GET", "/projects/TRK/documents", None)
        .await;
    assert_eq!(status, 401);
    let (status, _) = api
        .call_as(
            Some(&read_key),
            "POST",
            "/projects/TRK/documents",
            Some(json!({"title": "no"})),
        )
        .await;
    assert_eq!(status, 403);
    let (status, list) = api
        .call_as(Some(&read_key), "GET", "/projects/TRK/documents", None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(list, json!([]));

    // Create: 201, revision 1, markdown body kept as written.
    let (status, a) = api
        .call(
            "POST",
            "/projects/TRK/documents",
            Some(json!({"title": "Design", "body": "# Design\n\nfirst"})),
        )
        .await;
    assert_eq!(status, 201, "{a}");
    let a_id = a["id"].as_i64().unwrap();
    assert_eq!(a["revision"], 1);
    assert_eq!(a["body"], "# Design\n\nfirst");
    assert_eq!(a["archived"], false);
    assert!(a["parent"].is_null());

    let (status, b) = api
        .call(
            "POST",
            "/projects/TRK/documents",
            Some(json!({"title": "Detail", "parent": a_id})),
        )
        .await;
    assert_eq!(status, 201, "{b}");
    let b_id = b["id"].as_i64().unwrap();
    assert_eq!(b["parent"], a_id);

    // Invalid and unknown input.
    for bad in [
        json!({"title": "  "}),
        json!({"title": "x", "parent": 9999}),
    ] {
        let (status, _) = api.call("POST", "/projects/TRK/documents", Some(bad)).await;
        assert_eq!(status, 422);
    }
    let (status, _) = api.call("GET", "/projects/TRK/documents/9999", None).await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call("GET", &format!("/projects/OTH/documents/{a_id}"), None)
        .await;
    assert_eq!(status, 404, "a document of another project is not found");
    let (status, _) = api
        .call(
            "POST",
            "/projects/OTH/documents",
            Some(json!({"title": "x", "parent": a_id})),
        )
        .await;
    assert_eq!(status, 422, "a parent from another project is refused");

    // List and detail.
    let (status, list) = api.call("GET", "/projects/TRK/documents", None).await;
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (status, detail) = api
        .call("GET", &format!("/projects/TRK/documents/{b_id}"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(detail["ancestors"][0]["id"], a_id);
    let (_, detail) = api
        .call("GET", &format!("/projects/TRK/documents/{a_id}"), None)
        .await;
    assert_eq!(detail["children"][0]["id"], b_id);

    // Edits make revisions; base_revision guards them; a no-op writes nothing.
    let path = format!("/projects/TRK/documents/{a_id}");
    let (status, edited) = api
        .call("PATCH", &path, Some(json!({"body": "second"})))
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["revision"], 2);
    let (status, stale) = api
        .call(
            "PATCH",
            &path,
            Some(json!({"body": "late", "base_revision": 1})),
        )
        .await;
    assert_eq!(status, 409, "{stale}");
    let (status, edited) = api
        .call(
            "PATCH",
            &path,
            Some(json!({"title": "Design v2", "base_revision": 2})),
        )
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["revision"], 3);
    assert_eq!(edited["title"], "Design v2");
    let (status, same) = api
        .call("PATCH", &path, Some(json!({"body": "second"})))
        .await;
    assert_eq!(status, 200);
    assert_eq!(same["revision"], 3, "a no-op edit writes no revision");

    // Revisions: newest first, one by number, 404 for an unknown number.
    let (status, revisions) = api.call("GET", &format!("{path}/revisions"), None).await;
    assert_eq!(status, 200);
    let numbers: Vec<i64> = revisions
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["number"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [3, 2, 1]);
    let (status, first) = api.call("GET", &format!("{path}/revisions/1"), None).await;
    assert_eq!(status, 200);
    assert_eq!(first["body"], "# Design\n\nfirst");
    let (status, _) = api.call("GET", &format!("{path}/revisions/99"), None).await;
    assert_eq!(status, 404);

    // Restore appends a new revision; a stale base is a conflict.
    let (status, _) = api
        .call(
            "POST",
            &format!("{path}/restore"),
            Some(json!({"revision": 1, "base_revision": 2})),
        )
        .await;
    assert_eq!(status, 409);
    let (status, _) = api
        .call(
            "POST",
            &format!("{path}/restore"),
            Some(json!({"revision": 99})),
        )
        .await;
    assert_eq!(status, 404);
    let (status, restored) = api
        .call(
            "POST",
            &format!("{path}/restore"),
            Some(json!({"revision": 1, "base_revision": 3})),
        )
        .await;
    assert_eq!(status, 200, "{restored}");
    assert_eq!(restored["revision"], 4);
    assert_eq!(restored["body"], "# Design\n\nfirst");
    let (_, revisions) = api.call("GET", &format!("{path}/revisions"), None).await;
    assert_eq!(revisions[0]["note"], "restored from 1");

    // Parent rules: a cycle is a conflict; moving under another document works.
    let (status, _) = api
        .call("PATCH", &path, Some(json!({"parent": b_id})))
        .await;
    assert_eq!(status, 409, "a cycle");
    let (status, c) = api
        .call(
            "POST",
            "/projects/TRK/documents",
            Some(json!({"title": "Other"})),
        )
        .await;
    assert_eq!(status, 201);
    let c_id = c["id"].as_i64().unwrap();
    let (status, moved) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/documents/{b_id}"),
            Some(json!({"parent": c_id})),
        )
        .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["parent"], c_id);
    let (status, moved) = api
        .call(
            "POST",
            &format!("/projects/TRK/documents/{b_id}/move"),
            Some(json!({"parent": null, "before": a_id})),
        )
        .await;
    assert_eq!(status, 200, "{moved}");
    assert!(moved["parent"].is_null());

    // Depth: a chain deeper than the limit is refused with 422.
    let mut parent = None;
    let mut made = 0;
    let refused = loop {
        let body = match parent {
            Some(id) => json!({"title": "level", "parent": id}),
            None => json!({"title": "level"}),
        };
        let (status, doc) = api
            .call("POST", "/projects/TRK/documents", Some(body))
            .await;
        if status != 201 {
            break status;
        }
        parent = doc["id"].as_i64();
        made += 1;
        assert!(made < 20, "the depth limit never applied");
    };
    assert_eq!(refused, 422);
    assert!((7..=9).contains(&made), "made {made} levels");

    // Archive: refused while live children exist; then fine, and reversible.
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/documents/{b_id}"),
            Some(json!({"parent": c_id})),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = api
        .call(
            "POST",
            &format!("/projects/TRK/documents/{c_id}/archive"),
            None,
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, archived) = api
        .call(
            "POST",
            &format!("/projects/TRK/documents/{b_id}/archive"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{archived}");
    assert_eq!(archived["archived"], true);
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/documents/{b_id}"),
            Some(json!({"body": "edit while archived"})),
        )
        .await;
    assert_eq!(status, 409, "an archived document is read-only");
    let (_, list) = api.call("GET", "/projects/TRK/documents", None).await;
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|d| d["id"] != json!(b_id))
    );
    let (_, list) = api
        .call("GET", "/projects/TRK/documents?include_archived=true", None)
        .await;
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == json!(b_id))
    );
    let (status, back) = api
        .call(
            "POST",
            &format!("/projects/TRK/documents/{b_id}/unarchive"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{back}");
    assert_eq!(back["archived"], false);

    // Links, visible from both sides.
    let links = format!("{path}/links");
    let (status, link) = api.call("POST", &links, Some(json!({"issue": 1}))).await;
    assert_eq!(status, 201, "{link}");
    let issue_link = link["id"].as_i64().unwrap();
    let (status, _) = api.call("POST", &links, Some(json!({"issue": 1}))).await;
    assert_eq!(status, 409, "a duplicate link");
    let (status, _) = api
        .call("POST", &links, Some(json!({"issue": 1, "initiative": 1})))
        .await;
    assert_eq!(status, 422);
    let (status, _) = api.call("POST", &links, Some(json!({}))).await;
    assert_eq!(status, 422);
    let (status, _) = api.call("POST", &links, Some(json!({"issue": 99}))).await;
    assert_eq!(status, 422, "an unknown issue");
    let (status, _) = api
        .call("POST", &links, Some(json!({"initiative": 9999})))
        .await;
    assert_eq!(status, 422, "an unknown initiative");
    let (status, initiative_link) = api
        .call("POST", &links, Some(json!({"initiative": initiative_id})))
        .await;
    assert_eq!(status, 201, "{initiative_link}");

    let (_, issue) = api.call("GET", "/projects/TRK/issues/1", None).await;
    assert_eq!(issue["documents"][0]["id"], a_id);
    assert_eq!(issue["documents"][0]["title"], "Design");
    let (_, initiative) = api
        .call(
            "GET",
            &format!("/projects/TRK/initiatives/{initiative_id}"),
            None,
        )
        .await;
    assert_eq!(initiative["documents"][0]["id"], a_id);
    let (_, detail) = api.call("GET", &path, None).await;
    assert_eq!(detail["links"]["issues"][0]["number"], 1);
    assert_eq!(detail["links"]["initiatives"][0]["id"], initiative_id);

    let (status, _) = api
        .call("DELETE", &format!("{links}/{issue_link}"), None)
        .await;
    assert_eq!(status, 204);
    let (status, _) = api
        .call("DELETE", &format!("{links}/{issue_link}"), None)
        .await;
    assert_eq!(status, 404);
    let (_, issue) = api.call("GET", "/projects/TRK/issues/1", None).await;
    assert_eq!(issue["documents"], json!([]));

    // Deleting the initiative takes the link with it.
    let (status, _) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/initiatives/{initiative_id}"),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (_, detail) = api.call("GET", &path, None).await;
    assert_eq!(detail["links"]["initiatives"], json!([]));

    // Preview: rendered by the same renderer, nothing executable, write scope.
    let (status, preview) = api
        .call(
            "POST",
            "/projects/TRK/documents/preview",
            Some(json!({"body": "**bold** <script>alert(1)</script> [x](javascript:alert(1))"})),
        )
        .await;
    assert_eq!(status, 200, "{preview}");
    let html = preview["html"].as_str().unwrap();
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("javascript:"), "{html}");
    let (status, _) = api
        .call_as(
            Some(&read_key),
            "POST",
            "/projects/TRK/documents/preview",
            Some(json!({"body": "x"})),
        )
        .await;
    assert_eq!(status, 403);
    let (status, _) = api
        .call(
            "POST",
            "/projects/NOPE/documents/preview",
            Some(json!({"body": "x"})),
        )
        .await;
    assert_eq!(status, 404);

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
