//! Comments over HTTP: add, list, edit, delete, replies, mentions and refusals.

use aurora_store::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

const MEMBER_KEY: &str = "aur_comments_member_key_tests_only";

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

fn ids(list: &[Value]) -> Vec<i64> {
    list.iter().map(|c| c["id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn comments_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = Api {
        addr: listener.local_addr().unwrap(),
        key: common::owner_key(&store).await,
    };
    // A member (nothing in the API creates one) with a write key.
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO user (id, name, kind, role) VALUES (2, 'ann', 'human', 'member')",
                [],
            )?;
            tx.execute(
                "INSERT INTO api_key (user_id, name, prefix, key_hash, scope, created)
                 VALUES (2, 'ann', 'aur_comm', ?1, 'write', 1)",
                [format!("{:x}", Sha256::digest(MEMBER_KEY.as_bytes()))],
            )?;
            Ok(())
        })
        .await
        .unwrap();
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
        for title in ["one", "two"] {
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

    // An agent user with its own key, and a read-only key for the owner.
    let (status, agent) = api
        .call("POST", "/users", Some(json!({"name": "robot"})))
        .await;
    assert_eq!(status, 201);
    let (_, agent_key) = api
        .call(
            "POST",
            "/keys",
            Some(json!({"name": "robot", "user_id": agent["id"]})),
        )
        .await;
    let agent_key = agent_key["key"].as_str().unwrap().to_string();
    let (_, read_key) = api
        .call(
            "POST",
            "/keys",
            Some(json!({"name": "ro", "scope": "read"})),
        )
        .await;
    let read_key = read_key["key"].as_str().unwrap().to_string();

    // Create: 201 and the comment, with the mention resolved.
    let (status, top) = api
        .call_as(
            Some(MEMBER_KEY),
            "POST",
            "/projects/TRK/issues/1/comments",
            Some(json!({"body": " Hello @robot and @nobody <b>x</b> "})),
        )
        .await;
    assert_eq!(status, 201, "{top}");
    assert_eq!(top["body"], "Hello @robot and @nobody <b>x</b>");
    assert_eq!(
        top["author"],
        json!({"id": 2, "name": "ann", "kind": "human"})
    );
    assert_eq!(top["issue"], json!({"number": 1, "identifier": "TRK-1"}));
    assert_eq!(
        top["mentions"],
        json!([{"user_id": agent["id"], "name": "robot"}])
    );
    assert!(top["parent"].is_null() && top["edited"].is_null() && top["deleted"].is_null());
    let top_id = top["id"].as_i64().unwrap();

    // The mention is an event the runner can read, with the agent kind.
    let (_, events) = api.call("GET", "/events?kind_prefix=mention.", None).await;
    let events = events["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["payload"]["user_kind"], "agent");
    assert_eq!(events[0]["payload"]["user_name"], "robot");
    assert_eq!(events[0]["payload"]["comment_id"], top_id);
    assert_eq!(events[0]["actor"], 2);

    // Replies nest one level.
    let (status, reply) = api
        .call_as(
            Some(&agent_key),
            "POST",
            "/projects/TRK/issues/1/comments",
            Some(json!({"body": "on it", "parent": top_id})),
        )
        .await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!(reply["parent"], top_id);
    assert_eq!(reply["author"]["kind"], "agent");
    let reply_id = reply["id"].as_i64().unwrap();
    for (parent, why) in [
        (json!(reply_id), "a reply to a reply"),
        (json!(999_999), "a missing parent"),
    ] {
        let (status, err) = api
            .call(
                "POST",
                "/projects/TRK/issues/1/comments",
                Some(json!({"body": "x", "parent": parent})),
            )
            .await;
        assert_eq!(status, 422, "{why}: {err}");
        assert!(err["error"].is_string());
    }
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/2/comments",
            Some(json!({"body": "x", "parent": top_id})),
        )
        .await;
    assert_eq!(status, 422, "a parent on another issue");
    let (_, other) = api
        .call(
            "POST",
            "/projects/OTH/issues/1/comments",
            Some(json!({"body": "other project"})),
        )
        .await;
    let other_id = other["id"].as_i64().unwrap();
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/1/comments",
            Some(json!({"body": "x", "parent": other_id})),
        )
        .await;
    assert_eq!(status, 422, "a parent in another project");

    // Bad bodies and bodies of the wrong shape: 422 with the error body.
    for body in [
        json!({"body": "   "}),
        json!({"body": "x".repeat(20_001)}),
        json!({}),
        json!({"body": 5}),
        json!({"body": "x", "extra": 1}),
    ] {
        let (status, err) = api
            .call(
                "POST",
                "/projects/TRK/issues/1/comments",
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, 422, "{body}: {err}");
        assert!(err["error"].is_string());
    }

    // List: threaded, in creation order.
    let (_, second) = api
        .call(
            "POST",
            "/projects/TRK/issues/1/comments",
            Some(json!({"body": "second top-level"})),
        )
        .await;
    let (status, list) = api
        .call("GET", "/projects/TRK/issues/1/comments", None)
        .await;
    assert_eq!(status, 200);
    let list = list.as_array().unwrap();
    assert_eq!(ids(list), [top_id, second["id"].as_i64().unwrap()]);
    assert_eq!(ids(list[0]["replies"].as_array().unwrap()), [reply_id]);
    assert_eq!(list[1]["replies"], json!([]));
    let (_, empty) = api
        .call("GET", "/projects/TRK/issues/2/comments", None)
        .await;
    assert_eq!(empty, json!([]));

    // Edit: the author and the owner; not another member or agent.
    let path = format!("/projects/TRK/comments/{top_id}");
    let (status, edited) = api
        .call_as(
            Some(MEMBER_KEY),
            "PATCH",
            &path,
            Some(json!({"body": "Hello @robot, edited"})),
        )
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["body"], "Hello @robot, edited");
    assert!(edited["edited"].is_i64());
    let (status, err) = api
        .call_as(
            Some(&agent_key),
            "PATCH",
            &path,
            Some(json!({"body": "hijack"})),
        )
        .await;
    assert_eq!(status, 403, "{err}");
    assert!(err["error"].is_string());
    let (status, _) = api
        .call("PATCH", &path, Some(json!({"body": "by the owner"})))
        .await;
    assert_eq!(status, 200);
    for body in [json!({"body": ""}), json!({"text": "x"})] {
        let (status, _) = api.call("PATCH", &path, Some(body)).await;
        assert_eq!(status, 422);
    }

    // Not found: a comment of another project, an unknown id, an unknown issue.
    let other_path = format!("/projects/TRK/comments/{other_id}");
    let (status, _) = api
        .call("PATCH", &other_path, Some(json!({"body": "x"})))
        .await;
    assert_eq!(status, 404);
    let (status, _) = api.call("DELETE", &other_path, None).await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call(
            "PATCH",
            "/projects/TRK/comments/999999",
            Some(json!({"body": "x"})),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call("GET", "/projects/TRK/issues/99/comments", None)
        .await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call("GET", "/projects/NOPE/issues/1/comments", None)
        .await;
    assert_eq!(status, 404);
    let (status, _) = api
        .call(
            "POST",
            "/projects/TRK/issues/99/comments",
            Some(json!({"body": "x"})),
        )
        .await;
    assert_eq!(status, 404);

    // Authentication and scope: no key 401, read-only key reads but cannot write.
    let (status, _) = api
        .call_as(None, "GET", "/projects/TRK/issues/1/comments", None)
        .await;
    assert_eq!(status, 401);
    let (status, _) = api
        .call_as(
            None,
            "POST",
            "/projects/TRK/issues/1/comments",
            Some(json!({"body": "x"})),
        )
        .await;
    assert_eq!(status, 401);
    let (status, _) = api
        .call_as(
            Some(&read_key),
            "GET",
            "/projects/TRK/issues/1/comments",
            None,
        )
        .await;
    assert_eq!(status, 200);
    for (method, target, body) in [
        (
            "POST",
            "/projects/TRK/issues/1/comments".to_string(),
            Some(json!({"body": "x"})),
        ),
        ("PATCH", path.clone(), Some(json!({"body": "x"}))),
        ("DELETE", path.clone(), None),
    ] {
        let (status, err) = api.call_as(Some(&read_key), method, &target, body).await;
        assert_eq!(status, 403, "{method}: {err}");
    }

    // Delete: 403 for a stranger, 204 for the author; the thread keeps its reply.
    let (status, _) = api.call_as(Some(&agent_key), "DELETE", &path, None).await;
    assert_eq!(status, 403);
    let (status, body) = api.call_as(Some(MEMBER_KEY), "DELETE", &path, None).await;
    assert_eq!((status, body), (204, Value::Null));
    let (_, list) = api
        .call("GET", "/projects/TRK/issues/1/comments", None)
        .await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], top_id);
    assert_eq!(list[0]["body"], "");
    assert!(list[0]["deleted"].is_i64());
    assert_eq!(list[0]["mentions"], json!([]));
    assert_eq!(ids(list[0]["replies"].as_array().unwrap()), [reply_id]);
    let (status, _) = api.call("DELETE", &path, None).await;
    assert_eq!(status, 404, "already deleted");
    let (status, _) = api.call("PATCH", &path, Some(json!({"body": "x"}))).await;
    assert_eq!(status, 404);

    // The reply goes too, then the placeholder disappears from the list.
    let (status, _) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/comments/{reply_id}"),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (_, list) = api
        .call("GET", "/projects/TRK/issues/1/comments", None)
        .await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // The log tells the story; none of it holds a full body.
    let (_, events) = api
        .call("GET", "/events?kind_prefix=comment.&limit=100", None)
        .await;
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"comment.created"));
    assert!(kinds.contains(&"comment.edited"));
    assert!(kinds.contains(&"comment.deleted"));
    assert!(!events.to_string().contains("second top-level"));

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
