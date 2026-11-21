//! Request ids (TRK-43) over HTTP and MCP: every write that can take an id is
//! applied once however often it is sent.

use std::collections::BTreeSet;

use aurora::idem;
use aurora::routes::api_endpoints;
use aurora_store::Store;
use axum::http::Method;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

const MISMATCH: &str = "request id was already used for a different request";

struct Resp {
    status: u16,
    headers: String,
    body: String,
}

impl Resp {
    fn replayed(&self) -> bool {
        self.headers.contains("idempotent-replay: true")
    }

    fn json(&self) -> Value {
        serde_json::from_str(self.body.trim())
            .unwrap_or_else(|_| panic!("not JSON ({}): {}", self.status, self.body))
    }
}

struct Server {
    addr: std::net::SocketAddr,
    store: Store,
    key: String,
    other_key: String,
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
            store.clone(),
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
        ));
        let mut server = Server {
            addr,
            store,
            key,
            other_key: String::new(),
            _dir: dir,
            stop: Some(stop),
        };
        let user = server
            .ok("POST", "/api/v1/users", Some(json!({"name": "other"})))
            .await;
        let key = server
            .ok(
                "POST",
                "/api/v1/keys",
                Some(json!({"name": "other key", "user_id": user["id"]})),
            )
            .await;
        server.other_key = key["key"].as_str().unwrap().to_string();
        server
    }

    async fn send(
        &self,
        bearer: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
        idem: Option<&str>,
    ) -> Resp {
        let body = body.map(ToString::to_string).unwrap_or_default();
        let idem = idem
            .map(|id| format!("Idempotency-Key: {id}\r\n"))
            .unwrap_or_default();
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {bearer}\r\n{idem}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let (head, text) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        Resp {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            headers: head.to_ascii_lowercase(),
            body: text.to_string(),
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        idem: Option<&str>,
    ) -> Resp {
        self.send(&self.key, method, path, body, idem).await
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let resp = self.call(method, path, body.as_ref(), None).await;
        assert!(
            (200..300).contains(&resp.status),
            "{method} {path}: {} {}",
            resp.status,
            resp.body
        );
        serde_json::from_str(resp.body.trim()).unwrap_or(Value::Null)
    }

    /// Every row of every table a write can change, as text; the request log
    /// and the credentials' bookkeeping are left out.
    async fn dump(&self) -> String {
        self.store
            .read(|c| {
                let tables: Vec<String> = c
                    .prepare(
                        "SELECT name FROM sqlite_master WHERE type = 'table'
                         AND name NOT LIKE 'sqlite_%'
                         AND name NOT IN ('request_log', 'schema_version', 'session', 'api_key')
                         ORDER BY name",
                    )?
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                let mut out = String::new();
                let mut dump_query = |sql: String| -> Result<(), aurora_store::StoreError> {
                    let mut stmt = c.prepare(&sql)?;
                    let columns = stmt.column_count();
                    let mut rows = stmt.query([])?;
                    while let Some(row) = rows.next()? {
                        for i in 0..columns {
                            out.push_str(&format!("{:?}|", row.get_ref(i)?));
                        }
                        out.push('\n');
                    }
                    Ok(())
                };
                for table in tables {
                    dump_query(format!("SELECT * FROM \"{table}\""))?;
                }
                dump_query(
                    "SELECT id, user_id, name, prefix, key_hash, scope, created, revoked
                     FROM api_key ORDER BY id"
                        .into(),
                )?;
                Ok(out)
            })
            .await
            .unwrap()
    }

    async fn request_log_rows(&self) -> i64 {
        self.store
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM request_log", [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    async fn issue_count(&self, project: &str) -> i64 {
        let list = self
            .ok(
                "GET",
                &format!("/api/v1/projects/{project}/issues?limit=1"),
                None,
            )
            .await;
        list["total"].as_i64().unwrap()
    }

    async fn mcp(&self, tool: &str, arguments: Value) -> Value {
        let message = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        let resp = self.call("POST", "/mcp", Some(&message), None).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        resp.json()["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn id(value: &Value) -> i64 {
    value["id"].as_i64().unwrap()
}

/// The ids of the entities the samples act on.
struct World {
    cols: Vec<i64>,
    col_order: Vec<i64>,
    col_temp: i64,
    task: i64,
    spare1: i64,
    spare2: i64,
    label_a: i64,
    label_b: i64,
    link: i64,
    comment_edit: i64,
    comment_delete: i64,
    init_patch: i64,
    init_delete: i64,
    docs: Vec<i64>,
    doc_link: i64,
    last_event: i64,
    agent_patch: i64,
    agent_delete: i64,
    agent_off: i64,
    agent_on: i64,
    key_rotate: i64,
    key_revoke: i64,
}

async fn build_world(s: &Server) -> World {
    let project = s
        .ok(
            "POST",
            "/api/v1/projects",
            Some(json!({"key": "TRK", "name": "Tracker"})),
        )
        .await;
    let cols: Vec<i64> = project["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(id)
        .collect();
    let task = id(&s
        .ok(
            "POST",
            "/api/v1/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await);
    let mut spare = Vec::new();
    for name in ["Spare one", "Spare two"] {
        let made = s
            .ok(
                "POST",
                "/api/v1/projects/TRK/issue-types",
                Some(json!({"name": name})),
            )
            .await;
        spare.push(id(&made));
    }
    let mut labels = Vec::new();
    for name in ["alpha", "beta"] {
        let made = s
            .ok(
                "POST",
                "/api/v1/projects/TRK/labels",
                Some(json!({"name": name})),
            )
            .await;
        labels.push(id(&made));
    }
    for n in 1..=10 {
        s.ok(
            "POST",
            "/api/v1/projects/TRK/issues",
            Some(json!({"title": format!("Issue {n}"), "type_id": task})),
        )
        .await;
    }
    s.ok("POST", "/api/v1/projects/TRK/issues/4/archive", None)
        .await;
    let link = s
        .ok(
            "POST",
            "/api/v1/projects/TRK/issues/7/links",
            Some(json!({"kind": "blocks", "to": 8})),
        )
        .await;
    let mut comments = Vec::new();
    for body in ["one", "two"] {
        let made = s
            .ok(
                "POST",
                "/api/v1/projects/TRK/issues/9/comments",
                Some(json!({"body": body})),
            )
            .await;
        comments.push(id(&made));
    }
    let mut inits = Vec::new();
    for name in ["Patch me", "Delete me"] {
        let made = s
            .ok(
                "POST",
                "/api/v1/projects/TRK/initiatives",
                Some(json!({"name": name})),
            )
            .await;
        inits.push(id(&made));
    }
    let mut docs = Vec::new();
    for n in 1..=7 {
        let made = s
            .ok(
                "POST",
                "/api/v1/projects/TRK/documents",
                Some(json!({"title": format!("Doc {n}"), "body": "v1"})),
            )
            .await;
        docs.push(id(&made));
    }
    s.ok(
        "PATCH",
        &format!("/api/v1/projects/TRK/documents/{}", docs[4]),
        Some(json!({"body": "v2"})),
    )
    .await;
    s.ok(
        "POST",
        &format!("/api/v1/projects/TRK/documents/{}/archive", docs[3]),
        None,
    )
    .await;
    let doc_link = s
        .ok(
            "POST",
            &format!("/api/v1/projects/TRK/documents/{}/links", docs[6]),
            Some(json!({"issue": 1})),
        )
        .await;
    for name in ["reader", "gone"] {
        s.ok(
            "PUT",
            &format!("/api/v1/consumers/{name}"),
            Some(json!({"start": "beginning"})),
        )
        .await;
    }
    let events = s.ok("GET", "/api/v1/events?limit=500", None).await;
    let last_event = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(id)
        .max()
        .unwrap();

    s.ok(
        "POST",
        "/api/v1/projects",
        Some(json!({"key": "COL", "name": "Columns"})),
    )
    .await;
    let temp = s
        .ok(
            "POST",
            "/api/v1/projects/COL/columns",
            Some(json!({"name": "Temp", "state_type": "backlog"})),
        )
        .await;
    let columns = s.ok("GET", "/api/v1/projects/COL/columns", None).await;
    let col_order: Vec<i64> = columns.as_array().unwrap().iter().map(id).collect();

    let mut agents = Vec::new();
    for (name, enabled) in [
        ("bot-patch", true),
        ("bot-delete", true),
        ("bot-off", false),
        ("bot-on", true),
    ] {
        let made = s
            .ok(
                "POST",
                "/api/v1/agents",
                Some(json!({"name": name, "command": "echo", "enabled": enabled})),
            )
            .await;
        agents.push(id(&made));
    }
    let mut keys = Vec::new();
    for name in ["rotate me", "revoke me"] {
        let made = s
            .ok("POST", "/api/v1/keys", Some(json!({"name": name})))
            .await;
        keys.push(id(&made));
    }
    World {
        cols,
        col_order,
        col_temp: id(&temp),
        task,
        spare1: spare[0],
        spare2: spare[1],
        label_a: labels[0],
        label_b: labels[1],
        link: id(&link),
        comment_edit: comments[0],
        comment_delete: comments[1],
        init_patch: inits[0],
        init_delete: inits[1],
        docs,
        doc_link: id(&doc_link),
        last_event,
        agent_patch: agents[0],
        agent_delete: agents[1],
        agent_off: agents[2],
        agent_on: agents[3],
        key_rotate: keys[0],
        key_revoke: keys[1],
    }
}

struct Sample {
    method: &'static str,
    template: &'static str,
    path: String,
    body: Option<Value>,
    /// Another valid body, for the "same id, different request" check.
    alt: Option<Value>,
    status: u16,
    /// The response carries a one-time key.
    secret: bool,
}

fn sample(method: &'static str, template: &'static str, path: &str, status: u16) -> Sample {
    Sample {
        method,
        template,
        path: format!("/api/v1{path}"),
        body: None,
        alt: None,
        status,
        secret: false,
    }
}

impl Sample {
    fn body(mut self, body: Value, alt: Value) -> Sample {
        self.body = Some(body);
        self.alt = Some(alt);
        self
    }

    fn secret(mut self) -> Sample {
        self.secret = true;
        self
    }
}

/// One valid request per mutating endpoint that takes a request id.
fn samples(w: &World) -> Vec<Sample> {
    let p = "/projects/TRK";
    let d = |n: usize| w.docs[n];
    let mut reversed = w.col_order.clone();
    reversed.reverse();
    let mut rotated = w.col_order.clone();
    rotated.rotate_left(1);
    vec![
        sample("POST", "/keys", "/keys", 201)
            .body(json!({"name": "new key"}), json!({"name": "other new key"}))
            .secret(),
        sample(
            "DELETE",
            "/keys/{id}",
            &format!("/keys/{}", w.key_revoke),
            204,
        ),
        sample(
            "POST",
            "/keys/{id}/rotate",
            &format!("/keys/{}/rotate", w.key_rotate),
            200,
        )
        .secret(),
        sample("POST", "/users", "/users", 201)
            .body(json!({"name": "user-new"}), json!({"name": "user-other"})),
        sample("POST", "/agents", "/agents", 201).body(
            json!({"name": "bot-new", "command": "echo"}),
            json!({"name": "bot-other", "command": "echo"}),
        ),
        sample("POST", "/agents", "/agents", 201)
            .body(
                json!({"name": "bot-key", "command": "echo", "create_key": true}),
                json!({"name": "bot-key-other", "command": "echo", "create_key": true}),
            )
            .secret(),
        sample(
            "PATCH",
            "/agents/{id}",
            &format!("/agents/{}", w.agent_patch),
            200,
        )
        .body(json!({"description": "d"}), json!({"description": "e"})),
        sample(
            "DELETE",
            "/agents/{id}",
            &format!("/agents/{}", w.agent_delete),
            204,
        ),
        sample(
            "POST",
            "/agents/{id}/enable",
            &format!("/agents/{}/enable", w.agent_off),
            200,
        ),
        sample(
            "POST",
            "/agents/{id}/disable",
            &format!("/agents/{}/disable", w.agent_on),
            200,
        ),
        sample("POST", "/projects", "/projects", 201).body(
            json!({"key": "NEW", "name": "New"}),
            json!({"key": "NEW2", "name": "New"}),
        ),
        sample("PATCH", "/projects/{key}", p, 200)
            .body(json!({"name": "Renamed"}), json!({"name": "Renamed again"})),
        // Before the column is added: the order must list every column.
        sample(
            "PUT",
            "/projects/{key}/columns/order",
            "/projects/COL/columns/order",
            200,
        )
        .body(json!({"ids": reversed}), json!({"ids": rotated})),
        sample(
            "POST",
            "/projects/{key}/columns",
            "/projects/COL/columns",
            201,
        )
        .body(
            json!({"name": "Review", "state_type": "started"}),
            json!({"name": "Review two", "state_type": "started"}),
        ),
        sample(
            "PATCH",
            "/projects/{key}/columns/{id}",
            &format!("/projects/COL/columns/{}", w.col_order[0]),
            200,
        )
        .body(json!({"name": "Renamed col"}), json!({"name": "Other col"})),
        sample(
            "DELETE",
            "/projects/{key}/columns/{id}",
            &format!("/projects/COL/columns/{}", w.col_temp),
            204,
        ),
        sample(
            "POST",
            "/projects/{key}/issue-types",
            &format!("{p}/issue-types"),
            201,
        )
        .body(json!({"name": "Bug"}), json!({"name": "Chore"})),
        sample(
            "PATCH",
            "/projects/{key}/issue-types/{id}",
            &format!("{p}/issue-types/{}", w.spare1),
            200,
        )
        .body(
            json!({"name": "Spare renamed"}),
            json!({"name": "Spare other"}),
        ),
        sample(
            "DELETE",
            "/projects/{key}/issue-types/{id}",
            &format!("{p}/issue-types/{}", w.spare2),
            204,
        ),
        sample(
            "POST",
            "/projects/{key}/labels",
            &format!("{p}/labels"),
            201,
        )
        .body(
            json!({"name": "new-label", "color": "#112233"}),
            json!({"name": "other-label"}),
        ),
        sample(
            "PATCH",
            "/projects/{key}/labels/{id}",
            &format!("{p}/labels/{}", w.label_a),
            200,
        )
        .body(json!({"name": "alpha two"}), json!({"color": "#445566"})),
        sample(
            "DELETE",
            "/projects/{key}/labels/{id}",
            &format!("{p}/labels/{}", w.label_b),
            204,
        ),
        sample(
            "POST",
            "/projects/{key}/issues",
            &format!("{p}/issues"),
            201,
        )
        .body(
            json!({"title": "A new issue", "type_id": w.task}),
            json!({"title": "Another issue", "type_id": w.task}),
        ),
        sample(
            "PATCH",
            "/projects/{key}/issues/{number}",
            &format!("{p}/issues/1"),
            200,
        )
        .body(
            json!({"title": "Changed"}),
            json!({"title": "Changed again"}),
        ),
        sample(
            "POST",
            "/projects/{key}/issues/{number}/move",
            &format!("{p}/issues/2/move"),
            200,
        )
        .body(
            json!({"column_id": w.cols[1]}),
            json!({"column_id": w.cols[2]}),
        ),
        sample(
            "POST",
            "/projects/{key}/issues/{number}/archive",
            &format!("{p}/issues/3/archive"),
            200,
        ),
        sample(
            "POST",
            "/projects/{key}/issues/{number}/unarchive",
            &format!("{p}/issues/4/unarchive"),
            200,
        ),
        sample(
            "POST",
            "/projects/{key}/issues/{number}/links",
            &format!("{p}/issues/5/links"),
            201,
        )
        .body(
            json!({"kind": "relates", "to": 6}),
            json!({"kind": "blocks", "to": 6}),
        ),
        sample(
            "DELETE",
            "/projects/{key}/links/{id}",
            &format!("{p}/links/{}", w.link),
            204,
        ),
        sample(
            "POST",
            "/projects/{key}/issues/{number}/comments",
            &format!("{p}/issues/9/comments"),
            201,
        )
        .body(json!({"body": "hello"}), json!({"body": "hello again"})),
        sample(
            "PATCH",
            "/projects/{key}/comments/{id}",
            &format!("{p}/comments/{}", w.comment_edit),
            200,
        )
        .body(json!({"body": "edited"}), json!({"body": "edited again"})),
        sample(
            "DELETE",
            "/projects/{key}/comments/{id}",
            &format!("{p}/comments/{}", w.comment_delete),
            204,
        ),
        sample(
            "POST",
            "/projects/{key}/initiatives",
            &format!("{p}/initiatives"),
            201,
        )
        .body(json!({"name": "Init"}), json!({"name": "Init two"})),
        sample(
            "PATCH",
            "/projects/{key}/initiatives/{id}",
            &format!("{p}/initiatives/{}", w.init_patch),
            200,
        )
        .body(
            json!({"name": "Renamed init"}),
            json!({"name": "Other init"}),
        ),
        sample(
            "DELETE",
            "/projects/{key}/initiatives/{id}",
            &format!("{p}/initiatives/{}", w.init_delete),
            204,
        ),
        sample(
            "POST",
            "/projects/{key}/documents",
            &format!("{p}/documents"),
            201,
        )
        .body(json!({"title": "New doc"}), json!({"title": "Other doc"})),
        sample(
            "PATCH",
            "/projects/{key}/documents/{id}",
            &format!("{p}/documents/{}", d(0)),
            200,
        )
        .body(json!({"title": "Doc one"}), json!({"title": "Doc uno"})),
        sample(
            "POST",
            "/projects/{key}/documents/{id}/move",
            &format!("{p}/documents/{}/move", d(1)),
            200,
        )
        .body(json!({"parent": d(0)}), json!({"parent": null})),
        sample(
            "POST",
            "/projects/{key}/documents/{id}/archive",
            &format!("{p}/documents/{}/archive", d(2)),
            200,
        ),
        sample(
            "POST",
            "/projects/{key}/documents/{id}/unarchive",
            &format!("{p}/documents/{}/unarchive", d(3)),
            200,
        ),
        sample(
            "POST",
            "/projects/{key}/documents/{id}/restore",
            &format!("{p}/documents/{}/restore", d(4)),
            200,
        )
        .body(json!({"revision": 1}), json!({"revision": 2})),
        sample(
            "POST",
            "/projects/{key}/documents/{id}/links",
            &format!("{p}/documents/{}/links", d(5)),
            201,
        )
        .body(json!({"issue": 1}), json!({"issue": 2})),
        sample(
            "DELETE",
            "/projects/{key}/documents/{id}/links/{link_id}",
            &format!("{p}/documents/{}/links/{}", d(6), w.doc_link),
            204,
        ),
        sample("PUT", "/consumers/{name}", "/consumers/fresh", 201)
            .body(json!({"start": "beginning"}), json!({"start": "latest"})),
        sample("DELETE", "/consumers/{name}", "/consumers/gone", 204),
        sample(
            "POST",
            "/consumers/{name}/ack",
            "/consumers/reader/ack",
            200,
        )
        .body(json!({"up_to": w.last_event}), json!({"up_to": 1})),
    ]
}

fn mutating(method: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
}

/// A concrete path for a route template, for the exclusion check.
fn concrete(template: &str) -> String {
    format!(
        "/api/v1{}",
        template
            .replace("{key}", "TRK")
            .replace('{', "1")
            .replace('}', "")
    )
}

fn strip_secret(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for field in ["key", "key_replayed", "message", "prefix"] {
                map.remove(field);
            }
            map.values_mut().for_each(strip_secret);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_secret),
        _ => {}
    }
}

#[tokio::test]
async fn every_mutating_endpoint_applies_a_repeated_request_id_once() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let all = samples(&w);

    // A route added to the table without a sample, or an exclusion nobody
    // documented, fails here.
    let table: BTreeSet<(String, String)> = api_endpoints()
        .into_iter()
        .filter(|e| mutating(e.method) && !e.public)
        .filter(|e| !idem::excluded(&Method::POST, &concrete(e.path)) || e.method != "POST")
        .map(|e| (e.method.to_string(), e.path.to_string()))
        .collect();
    let covered: BTreeSet<(String, String)> = all
        .iter()
        .map(|x| (x.method.to_string(), x.template.to_string()))
        .collect();
    assert_eq!(
        table, covered,
        "every mutating route needs a sample here (or an entry in idem::excluded)"
    );

    for (n, x) in all.iter().enumerate() {
        let what = format!("{} {}", x.method, x.path);
        let id = format!("request-{n:04}");
        let before = s.dump().await;

        let first = s.call(x.method, &x.path, x.body.as_ref(), Some(&id)).await;
        assert_eq!(first.status, x.status, "{what}: {}", first.body);
        assert!(!first.replayed(), "{what}: the first call is not a replay");
        let applied = s.dump().await;
        assert_ne!(applied, before, "{what}: the sample changes something");

        let second = s.call(x.method, &x.path, x.body.as_ref(), Some(&id)).await;
        assert_eq!(second.status, first.status, "{what}");
        assert!(second.replayed(), "{what}: the second call is a replay");
        assert_eq!(
            s.dump().await,
            applied,
            "{what}: a replay changes nothing and writes no event"
        );
        if x.secret {
            let mut original = first.json();
            let mut replay = second.json();
            let secret = original["key"].as_str().unwrap_or_default().to_string();
            assert!(secret.starts_with("aur_"), "{what}: {original}");
            assert_eq!(replay["key"], Value::Null, "{what}");
            assert_eq!(replay["key_replayed"], true, "{what}");
            assert!(replay["message"].is_string(), "{what}");
            assert!(!second.body.contains(&secret), "{what}");
            strip_secret(&mut original);
            strip_secret(&mut replay);
            assert_eq!(original, replay, "{what}");
        } else {
            assert_eq!(second.body, first.body, "{what}");
        }

        let other_body = x.alt.clone().unwrap_or_else(|| json!({"alt": true}));
        let different = s
            .call(x.method, &x.path, Some(&other_body), Some(&id))
            .await;
        assert_eq!(different.status, 422, "{what}: {}", different.body);
        assert_eq!(different.json()["error"], MISMATCH, "{what}");
        assert!(!different.replayed(), "{what}");
        assert_eq!(
            s.dump().await,
            applied,
            "{what}: the refusal changes nothing"
        );

        // The same id from another user is another request.
        let other = s
            .send(&s.other_key, x.method, &x.path, x.body.as_ref(), Some(&id))
            .await;
        assert!(!other.replayed(), "{what}: ids are per user");
        assert_ne!(
            other.json_or_null()["error"],
            MISMATCH,
            "{what}: ids are per user"
        );
    }

    let rows = s.request_log_rows().await;
    assert!(rows >= all.len() as i64, "one row per sample, got {rows}");
    let leaked: i64 = s
        .store
        .read(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM request_log
                 WHERE response_body LIKE '%aur_%' OR fingerprint LIKE '%aur_%'
                    OR request_id LIKE '%aur_%'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(leaked, 0, "no API key text is stored in the request log");
}

impl Resp {
    fn json_or_null(&self) -> Value {
        serde_json::from_str(self.body.trim()).unwrap_or(Value::Null)
    }
}

#[tokio::test]
async fn the_same_id_by_two_users_makes_two_issues_and_each_user_replays_alone() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let path = "/api/v1/projects/TRK/issues";
    let body = json!({"title": "Same", "type_id": w.task});
    let before = s.issue_count("TRK").await;

    let a1 = s
        .call("POST", path, Some(&body), Some("shared-id-0001"))
        .await;
    let b1 = s
        .send(
            &s.other_key,
            "POST",
            path,
            Some(&body),
            Some("shared-id-0001"),
        )
        .await;
    assert_eq!((a1.status, b1.status), (201, 201));
    assert!(!a1.replayed() && !b1.replayed());
    assert_ne!(a1.json()["number"], b1.json()["number"]);
    assert_eq!(s.issue_count("TRK").await, before + 2);

    let a2 = s
        .call("POST", path, Some(&body), Some("shared-id-0001"))
        .await;
    let b2 = s
        .send(
            &s.other_key,
            "POST",
            path,
            Some(&body),
            Some("shared-id-0001"),
        )
        .await;
    assert!(a2.replayed() && b2.replayed());
    assert_eq!(a2.body, a1.body);
    assert_eq!(b2.body, b1.body);
    assert_eq!(s.issue_count("TRK").await, before + 2);
}

#[tokio::test]
async fn ids_are_optional_and_have_a_format() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let path = "/api/v1/projects/TRK/issues";
    let body = json!({"title": "No id", "type_id": w.task});
    let before = s.issue_count("TRK").await;

    // No id: exactly as before, every call applies.
    for _ in 0..2 {
        let resp = s.call("POST", path, Some(&body), None).await;
        assert_eq!(resp.status, 201);
        assert!(!resp.replayed());
    }
    assert_eq!(s.issue_count("TRK").await, before + 2);
    assert_eq!(s.request_log_rows().await, 0);

    for bad in [
        "short",
        "has space in it",
        "bad/slash-0001",
        &"x".repeat(129),
    ] {
        let resp = s.call("POST", path, Some(&body), Some(bad)).await;
        assert_eq!(resp.status, 422, "{bad}: {}", resp.body);
        assert!(
            resp.json()["error"]
                .as_str()
                .unwrap()
                .contains("request id")
        );
    }
    assert_eq!(
        s.issue_count("TRK").await,
        before + 2,
        "nothing was applied"
    );

    let longest = "x".repeat(128);
    for good in ["abcdefgh", "a.b_c:d-e", longest.as_str()] {
        let resp = s.call("POST", path, Some(&body), Some(good)).await;
        assert_eq!(resp.status, 201, "{good}: {}", resp.body);
    }

    // A read ignores the header.
    let list = s.call("GET", path, None, Some("read-id-0001")).await;
    assert_eq!(list.status, 200);
    assert!(!list.replayed());
}

#[tokio::test]
async fn the_excluded_writes_ignore_the_header() {
    let s = Server::start().await;
    build_world(&s).await;
    let rows = s.request_log_rows().await;
    for id in ["read-id-0001", "bad"] {
        for _ in 0..2 {
            let preview = s
                .call(
                    "POST",
                    "/api/v1/projects/TRK/documents/preview",
                    Some(&json!({"body": "# hi"})),
                    Some(id),
                )
                .await;
            assert_eq!(preview.status, 200, "{}", preview.body);
            assert!(!preview.replayed());
            let out = s
                .call("POST", "/api/v1/auth/sign-out", None, Some(id))
                .await;
            assert_eq!(out.status, 204);
            assert!(!out.replayed());
        }
    }
    assert_eq!(s.request_log_rows().await, rows);
}

#[tokio::test]
async fn a_failed_attempt_is_not_remembered_so_the_retry_runs() {
    let s = Server::start().await;
    let path = "/api/v1/projects/LATE/issues";
    let body = json!({"title": "Late"});

    let failed = s
        .call("POST", path, Some(&body), Some("late-id-0001"))
        .await;
    assert_eq!(failed.status, 404, "{}", failed.body);
    assert_eq!(s.request_log_rows().await, 0);

    s.ok(
        "POST",
        "/api/v1/projects",
        Some(json!({"key": "LATE", "name": "Late"})),
    )
    .await;
    s.ok(
        "POST",
        "/api/v1/projects/LATE/issue-types",
        Some(json!({"name": "Task"})),
    )
    .await;
    let retry = s
        .call("POST", path, Some(&body), Some("late-id-0001"))
        .await;
    assert_eq!(retry.status, 201, "{}", retry.body);
    assert!(!retry.replayed());
    let again = s
        .call("POST", path, Some(&body), Some("late-id-0001"))
        .await;
    assert!(again.replayed());
    assert_eq!(s.issue_count("LATE").await, 1);
}

#[tokio::test]
async fn json_key_order_and_spacing_do_not_make_a_different_request() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let path = "/api/v1/projects/TRK/issues";
    let one = json!({"title": "Ordered", "type_id": w.task});
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let reordered = format!("{{ \"type_id\": {},   \"title\": \"Ordered\" }}", w.task);
    let first = s
        .call("POST", path, Some(&one), Some("order-id-0001"))
        .await;
    stream
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                 Authorization: Bearer {}\r\nIdempotency-Key: order-id-0001\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{reordered}",
                s.key,
                reordered.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let raw = raw.to_ascii_lowercase();
    assert!(raw.starts_with("http/1.1 201"), "{raw}");
    assert!(raw.contains("idempotent-replay: true"), "{raw}");
    assert_eq!(first.status, 201);
}

#[tokio::test]
async fn parallel_identical_requests_apply_once_and_all_get_the_same_answer() {
    let s = std::sync::Arc::new(Server::start().await);
    let w = build_world(&s).await;
    let before = s.issue_count("TRK").await;
    let body = json!({"title": "Parallel", "type_id": w.task});

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        let body = body.clone();
        tasks.push(tokio::spawn(async move {
            s.call(
                "POST",
                "/api/v1/projects/TRK/issues",
                Some(&body),
                Some("parallel-id-0001"),
            )
            .await
        }));
    }
    let mut answers = Vec::new();
    for task in tasks {
        answers.push(task.await.unwrap());
    }
    assert!(answers.iter().all(|a| a.status == 201));
    assert_eq!(
        answers.iter().filter(|a| !a.replayed()).count(),
        1,
        "exactly one request did the work"
    );
    assert!(answers.iter().all(|a| a.body == answers[0].body));
    assert_eq!(s.issue_count("TRK").await, before + 1);
}

#[tokio::test]
async fn an_id_that_expired_is_a_new_request_and_the_purge_removes_it() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let path = "/api/v1/projects/TRK/issues";
    let body = json!({"title": "Old", "type_id": w.task});
    let before = s.issue_count("TRK").await;

    let first = s
        .call("POST", path, Some(&body), Some("expiry-id-0001"))
        .await;
    assert_eq!(first.status, 201);
    // Age the row past the retention (the wall clock is not injectable here).
    s.store
        .write(|tx| {
            Ok(tx.execute(
                "UPDATE request_log SET created = created - ?1",
                [s_week_millis() + 1_000],
            )?)
        })
        .await
        .unwrap();

    let second = s
        .call("POST", path, Some(&body), Some("expiry-id-0001"))
        .await;
    assert_eq!(second.status, 201);
    assert!(!second.replayed(), "after the retention the id is free");
    assert_eq!(s.issue_count("TRK").await, before + 2);
    let third = s
        .call("POST", path, Some(&body), Some("expiry-id-0001"))
        .await;
    assert!(third.replayed(), "and it is remembered again");

    s.store
        .write(|tx| {
            Ok(tx.execute(
                "UPDATE request_log SET created = created - ?1",
                [s_week_millis() + 1_000],
            )?)
        })
        .await
        .unwrap();
    let rows = s.request_log_rows().await;
    assert!(rows > 0);
    let removed = s.store.purge_request_log(aurora_store::now_millis()).await;
    assert_eq!(removed.unwrap() as i64, rows);
    assert_eq!(s.request_log_rows().await, 0);
}

fn s_week_millis() -> i64 {
    7 * 24 * 60 * 60 * 1000
}

#[tokio::test]
async fn a_response_too_large_to_keep_is_replayed_as_a_note() {
    let s = Server::start().await;
    build_world(&s).await;
    let path = "/api/v1/projects/TRK/documents";
    // Two bytes per character: over 256 KiB on the wire, under the 200000 character limit.
    let body = json!({"title": "Big", "body": "\u{e9}".repeat(150_000)});
    let before = s.dump().await;

    let first = s
        .call("POST", path, Some(&body), Some("big-id-00001"))
        .await;
    assert_eq!(
        first.status,
        201,
        "{}",
        &first.body[..first.body.len().min(200)]
    );
    assert!(first.body.len() > 256 * 1024);
    let applied = s.dump().await;
    assert_ne!(applied, before);

    let again = s
        .call("POST", path, Some(&body), Some("big-id-00001"))
        .await;
    assert_eq!(again.status, 201);
    assert!(again.replayed());
    assert_eq!(
        again.json(),
        json!({"replayed": true, "note": "original response too large to replay"})
    );
    assert_eq!(s.dump().await, applied);
}

#[tokio::test]
async fn replaying_a_key_creating_call_never_stores_or_shows_the_key() {
    let s = Server::start().await;
    let body = json!({"name": "ci key"});
    let first = s
        .call("POST", "/api/v1/keys", Some(&body), Some("keyed-id-0001"))
        .await;
    assert_eq!(first.status, 201);
    assert!(first.headers.contains("cache-control: no-store"));
    let secret = first.json()["key"].as_str().unwrap().to_string();
    assert!(secret.starts_with("aur_"));

    let again = s
        .call("POST", "/api/v1/keys", Some(&body), Some("keyed-id-0001"))
        .await;
    assert_eq!(again.status, 201);
    assert!(again.replayed());
    assert!(again.headers.contains("cache-control: no-store"));
    let replay = again.json();
    assert_eq!(replay["key"], Value::Null);
    assert_eq!(replay["key_replayed"], true);
    assert!(replay["message"].as_str().unwrap().contains("shown once"));
    assert_eq!(replay["id"], first.json()["id"]);
    assert!(!again.body.contains(&secret));

    // One key was made, not two.
    let keys = s.ok("GET", "/api/v1/keys", None).await;
    let named = keys
        .as_array()
        .unwrap()
        .iter()
        .filter(|k| k["name"] == "ci key")
        .count();
    assert_eq!(named, 1);

    let stored: Vec<String> = s
        .store
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT request_id || '|' || fingerprint || '|' || COALESCE(response_body, '')
                 FROM request_log",
            )?;
            Ok(stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].contains("aur_"), "{}", stored[0]);
    assert!(!stored[0].contains(&secret));
}

// ---- MCP ----

fn tool_text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

fn replayed(result: &Value) -> bool {
    result["_meta"]["idempotent_replay"] == true
}

#[tokio::test]
async fn every_write_tool_applies_a_repeated_request_id_once() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let tools = s
        .call(
            "POST",
            "/mcp",
            Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})),
            None,
        )
        .await
        .json();
    let mut write_tools = BTreeSet::new();
    for tool in tools["result"]["tools"].as_array().unwrap() {
        if tool["annotations"]["readOnlyHint"] == false {
            let name = tool["name"].as_str().unwrap().to_string();
            assert!(
                tool["inputSchema"]["properties"]["request_id"].is_object(),
                "{name} must declare request_id"
            );
            write_tools.insert(name);
        } else {
            let name = tool["name"].as_str().unwrap();
            assert!(
                tool["inputSchema"]["properties"]["request_id"].is_null(),
                "{name} is a read tool"
            );
        }
    }

    let cases: Vec<(&str, Value, Value)> = vec![
        (
            "create_issue",
            json!({"project": "TRK", "title": "From MCP", "type": "Task"}),
            json!({"project": "TRK", "title": "From MCP, other", "type": "Task"}),
        ),
        (
            "update_issue",
            json!({"issue": "TRK-1", "title": "Updated by MCP"}),
            json!({"issue": "TRK-1", "title": "Updated by MCP, other"}),
        ),
        (
            "move_issue",
            json!({"issue": "TRK-2", "state_type": "started"}),
            json!({"issue": "TRK-2", "state_type": "completed"}),
        ),
        (
            "archive_issue",
            json!({"issue": "TRK-3"}),
            json!({"issue": "TRK-5"}),
        ),
        (
            "unarchive_issue",
            json!({"issue": "TRK-4"}),
            json!({"issue": "TRK-5"}),
        ),
        (
            "add_comment",
            json!({"issue": "TRK-9", "body": "from MCP"}),
            json!({"issue": "TRK-9", "body": "from MCP, other"}),
        ),
        (
            "edit_comment",
            json!({"project": "TRK", "comment_id": w.comment_edit, "body": "mcp edit"}),
            json!({"project": "TRK", "comment_id": w.comment_edit, "body": "mcp edit two"}),
        ),
        (
            "delete_comment",
            json!({"project": "TRK", "comment_id": w.comment_delete}),
            json!({"project": "TRK", "comment_id": w.comment_edit}),
        ),
        (
            "link_issues",
            json!({"issue": "TRK-5", "to": 6, "kind": "relates"}),
            json!({"issue": "TRK-5", "to": 6, "kind": "blocks"}),
        ),
        (
            "unlink_issues",
            json!({"project": "TRK", "link_id": w.link}),
            json!({"project": "TRK", "link_id": w.link + 1000}),
        ),
        (
            "create_document",
            json!({"project": "TRK", "title": "MCP doc"}),
            json!({"project": "TRK", "title": "MCP doc, other"}),
        ),
        (
            "update_document",
            json!({"project": "TRK", "id": w.docs[0], "title": "MCP retitled"}),
            json!({"project": "TRK", "id": w.docs[0], "title": "MCP retitled, other"}),
        ),
    ];
    let covered: BTreeSet<String> = cases.iter().map(|(name, _, _)| name.to_string()).collect();
    assert_eq!(write_tools, covered, "every write tool needs a case here");

    for (n, (tool, arguments, other)) in cases.into_iter().enumerate() {
        let id = format!("mcp-request-{n:04}");
        let mut with_id = arguments.clone();
        with_id["request_id"] = json!(id);
        let before = s.dump().await;

        let first = s.mcp(tool, with_id.clone()).await;
        assert_eq!(first["isError"], false, "{tool}: {first}");
        assert!(!replayed(&first), "{tool}");
        let applied = s.dump().await;
        assert_ne!(applied, before, "{tool}: the case changes something");

        let second = s.mcp(tool, with_id.clone()).await;
        assert_eq!(second["isError"], false, "{tool}: {second}");
        assert!(replayed(&second), "{tool}: the second call is a replay");
        assert_eq!(
            second["structuredContent"], first["structuredContent"],
            "{tool}"
        );
        assert_eq!(second["content"], first["content"], "{tool}");
        assert_eq!(s.dump().await, applied, "{tool}: a replay changes nothing");

        let mut changed = other;
        changed["request_id"] = json!(id);
        let different = s.mcp(tool, changed).await;
        assert_eq!(different["isError"], true, "{tool}");
        assert!(tool_text(&different).contains(MISMATCH), "{tool}");
        assert_eq!(s.dump().await, applied, "{tool}");
    }
}

#[tokio::test]
async fn mcp_tool_errors_are_not_remembered_and_bad_ids_are_refused() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let call = json!({"issue": "TRK-11", "body": "late comment", "request_id": "mcp-late-0001"});

    let failed = s.mcp("add_comment", call.clone()).await;
    assert_eq!(failed["isError"], true, "{failed}");
    assert_eq!(s.request_log_rows().await, 0);

    s.mcp(
        "create_issue",
        json!({"project": "TRK", "title": "Eleven", "type_id": w.task}),
    )
    .await;
    let retry = s.mcp("add_comment", call.clone()).await;
    assert_eq!(retry["isError"], false, "{retry}");
    assert!(!replayed(&retry));
    let again = s.mcp("add_comment", call).await;
    assert!(replayed(&again));
    assert_eq!(s.request_log_rows().await, 1);

    let short = s
        .mcp(
            "add_comment",
            json!({"issue": "TRK-1", "body": "x", "request_id": "short"}),
        )
        .await;
    assert_eq!(short["isError"], true);
    assert!(tool_text(&short).contains("request id"), "{short}");
    let wrong_type = s
        .mcp(
            "add_comment",
            json!({"issue": "TRK-1", "body": "x", "request_id": 12345678}),
        )
        .await;
    assert_eq!(wrong_type["isError"], true);

    // A read tool does not take one.
    let read = s
        .mcp(
            "get_issue",
            json!({"issue": "TRK-1", "request_id": "read-id-0001"}),
        )
        .await;
    assert_eq!(read["isError"], true);
    assert!(tool_text(&read).contains("unknown argument"), "{read}");

    // No id: every call applies, as before.
    let before = s.dump().await;
    for _ in 0..2 {
        s.mcp("add_comment", json!({"issue": "TRK-1", "body": "plain"}))
            .await;
    }
    assert_ne!(s.dump().await, before);
    assert_eq!(s.request_log_rows().await, 1);
}

#[tokio::test]
async fn mcp_and_rest_share_ids_per_user_but_never_confuse_requests() {
    let s = Server::start().await;
    let w = build_world(&s).await;
    let id = "cross-id-00001";
    let rest = s
        .call(
            "POST",
            "/api/v1/projects/TRK/issues",
            Some(&json!({"title": "Via REST", "type_id": w.task})),
            Some(id),
        )
        .await;
    assert_eq!(rest.status, 201);
    let mcp = s
        .mcp(
            "create_issue",
            json!({"project": "TRK", "title": "Via REST", "type_id": w.task, "request_id": id}),
        )
        .await;
    assert_eq!(
        mcp["isError"], true,
        "a different request under the same id"
    );
    assert!(tool_text(&mcp).contains(MISMATCH));
}
