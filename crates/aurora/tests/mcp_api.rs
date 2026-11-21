//! The MCP endpoint over HTTP: protocol, authentication, permissions, tools.

use aurora_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

const DOCS: &str = include_str!("../../../docs/mcp.md");

struct Raw {
    status: u16,
    head: String,
    body: String,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_str(self.body.trim())
            .unwrap_or_else(|_| panic!("not JSON: {} {}", self.status, self.body))
    }

    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }
}

struct Server {
    addr: std::net::SocketAddr,
    owner: String,
    _dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Server {
    async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        let owner = common::owner_key(&store).await;
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
            owner,
            _dir: dir,
            stop: Some(stop),
        }
    }

    async fn http(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        extra: &[(&str, &str)],
        body: &str,
    ) -> Raw {
        let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            head.push_str("Host: localhost\r\n");
        }
        if let Some(key) = key {
            head.push_str(&format!("Authorization: Bearer {key}\r\n"));
        }
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        ));
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        Raw {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            head: head.to_string(),
            body: body.to_string(),
        }
    }

    /// A REST call with the owner's key; the body must be a success.
    async fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let text = body.map(|b| b.to_string()).unwrap_or_default();
        let raw = self
            .http(
                method,
                &format!("/api/v1{path}"),
                Some(&self.owner),
                &[],
                &text,
            )
            .await;
        assert!(
            (200..300).contains(&raw.status),
            "{method} {path}: {} {}",
            raw.status,
            raw.body
        );
        serde_json::from_str(raw.body.trim()).unwrap_or(Value::Null)
    }

    async fn mcp_raw(&self, key: Option<&str>, extra: &[(&str, &str)], body: &str) -> Raw {
        self.http("POST", "/mcp", key, extra, body).await
    }

    /// Posts a JSON-RPC message with the owner's key.
    async fn rpc(&self, message: Value) -> Raw {
        self.mcp_raw(Some(&self.owner), &[], &message.to_string())
            .await
    }

    async fn call_as(&self, key: &str, tool: &str, arguments: Value) -> Value {
        let raw = self
            .mcp_raw(
                Some(key),
                &[],
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": tool, "arguments": arguments}})
                .to_string(),
            )
            .await;
        assert_eq!(raw.status, 200, "{tool}: {}", raw.body);
        let reply = raw.json();
        assert!(reply.get("error").is_none(), "{tool}: {reply}");
        reply["result"].clone()
    }

    /// A tool call that must succeed; returns its structured content.
    async fn ok_as(&self, key: &str, tool: &str, arguments: Value) -> Value {
        let result = self.call_as(key, tool, arguments).await;
        assert_eq!(result["isError"], false, "{tool}: {result}");
        result["structuredContent"].clone()
    }

    /// A tool call that must fail; returns the explanation.
    async fn err_as(&self, key: &str, tool: &str, arguments: Value) -> String {
        let result = self.call_as(key, tool, arguments).await;
        assert_eq!(result["isError"], true, "{tool}: {result}");
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    async fn new_key(&self, user_id: i64, scope: &str) -> (i64, String) {
        let made = self
            .rest(
                "POST",
                "/keys",
                Some(json!({"name": format!("k{user_id}{scope}"), "scope": scope, "user_id": user_id})),
            )
            .await;
        (
            made["id"].as_i64().unwrap(),
            made["key"].as_str().unwrap().to_string(),
        )
    }

    async fn agent(&self, name: &str) -> i64 {
        self.rest("POST", "/users", Some(json!({"name": name})))
            .await["id"]
            .as_i64()
            .unwrap()
    }

    /// Project TRK with the default columns and one issue type.
    async fn project(&self) {
        self.rest(
            "POST",
            "/projects",
            Some(json!({"key": "TRK", "name": "Tracker"})),
        )
        .await;
        self.rest(
            "POST",
            "/projects/TRK/issue-types",
            Some(json!({"name": "Task"})),
        )
        .await;
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn initialize_negotiates_the_protocol_revision() {
    let server = Server::start().await;
    for (asked, answered) in [
        ("2025-03-26", "2025-03-26"),
        ("2024-11-05", "2024-11-05"),
        ("2025-06-18", "2025-06-18"),
        ("2025-04-30", "2025-03-26"),
        ("2030-01-01", "2025-06-18"),
        ("2023-01-01", "2025-06-18"),
    ] {
        let raw = server
            .rpc(request(
                1,
                "initialize",
                json!({"protocolVersion": asked, "capabilities": {},
                       "clientInfo": {"name": "t", "version": "1"}}),
            ))
            .await;
        assert_eq!(raw.status, 200);
        assert_eq!(
            raw.header("content-type").as_deref(),
            Some("application/json")
        );
        let reply = raw.json();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 1);
        assert_eq!(
            reply["result"]["protocolVersion"], answered,
            "asked {asked}"
        );
        assert_eq!(
            reply["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(reply["result"]["serverInfo"]["name"], "aurora");
        assert_eq!(
            reply["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(reply["result"]["instructions"].as_str().unwrap().len() > 20);
    }
    // The server never issues a session id.
    let raw = server
        .rpc(request(
            2,
            "initialize",
            json!({"protocolVersion": "2025-03-26"}),
        ))
        .await;
    assert!(raw.header("mcp-session-id").is_none());
}

#[tokio::test]
async fn notifications_and_responses_are_accepted_without_a_body() {
    let server = Server::start().await;
    for message in [
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}),
        json!({"jsonrpc": "2.0", "id": 7, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 8, "error": {"code": -1, "message": "x"}}),
    ] {
        let raw = server.rpc(message.clone()).await;
        assert_eq!(raw.status, 202, "{message}");
        assert!(raw.body.is_empty(), "{message}: {}", raw.body);
    }
    let ping = server.rpc(request(5, "ping", json!({}))).await;
    assert_eq!(ping.status, 200);
    assert_eq!(
        ping.json(),
        json!({"jsonrpc": "2.0", "id": 5, "result": {}})
    );
}

#[tokio::test]
async fn tools_list_describes_every_tool() {
    let server = Server::start().await;
    let list = server.rpc(request(1, "tools/list", json!({}))).await.json();
    let names = tool_names(&list);
    for expected in [
        "list_projects",
        "get_project",
        "list_issues",
        "get_issue",
        "create_issue",
        "update_issue",
        "move_issue",
        "archive_issue",
        "unarchive_issue",
        "add_comment",
        "list_comments",
        "edit_comment",
        "delete_comment",
        "link_issues",
        "unlink_issues",
        "search",
        "list_documents",
        "get_document",
        "create_document",
        "update_document",
        "list_initiatives",
        "get_current_user",
    ] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
    assert!(
        !names
            .iter()
            .any(|n| n.contains("user") && n != "get_current_user")
    );
    for tool in list["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert!(tool["inputSchema"]["properties"].is_object(), "{name}");
        assert!(tool["description"].as_str().unwrap().len() > 20, "{name}");
        assert!(tool["annotations"]["readOnlyHint"].is_boolean(), "{name}");
    }
    let hint = |name: &str| {
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["annotations"]["readOnlyHint"]
            .clone()
    };
    assert_eq!(hint("get_issue"), true);
    assert_eq!(hint("search"), true);
    assert_eq!(hint("create_issue"), false);
    assert_eq!(hint("move_issue"), false);
}

#[tokio::test]
async fn every_tool_is_documented_in_docs_mcp_md() {
    let server = Server::start().await;
    let list = server.rpc(request(1, "tools/list", json!({}))).await.json();
    let names = tool_names(&list);
    assert!(names.len() >= 22);
    for name in &names {
        let heading = format!("### `{name}`");
        assert!(
            DOCS.lines().any(|line| line.trim() == heading),
            "docs/mcp.md has no heading {heading}"
        );
    }
    for line in DOCS.lines() {
        if let Some(name) = line
            .strip_prefix("### `")
            .and_then(|rest| rest.strip_suffix('`'))
        {
            assert!(
                names.iter().any(|n| n == name),
                "docs/mcp.md documents {name}, which is not a tool"
            );
        }
    }
}

#[tokio::test]
async fn batches_are_answered_as_batches() {
    let server = Server::start().await;
    let raw = server
        .rpc(json!([
            request(1, "ping", json!({})),
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            request(2, "tools/list", json!({})),
            request(3, "nope", json!({})),
        ]))
        .await;
    assert_eq!(raw.status, 200);
    let answers = raw.json();
    let answers = answers.as_array().unwrap();
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0]["id"], 1);
    assert!(answers[1]["result"]["tools"].is_array());
    assert_eq!(answers[2]["error"]["code"], -32601);

    let only_notifications = server
        .rpc(json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]))
        .await;
    assert_eq!(only_notifications.status, 202);

    let empty = server.rpc(json!([])).await;
    assert_eq!(empty.status, 400);
    assert_eq!(empty.json()["error"]["code"], -32600);
}

#[tokio::test]
async fn malformed_messages_get_json_rpc_errors() {
    let server = Server::start().await;
    let parse = server.mcp_raw(Some(&server.owner), &[], "{not json").await;
    assert_eq!(parse.status, 400);
    assert_eq!(parse.json()["error"]["code"], -32700);
    assert_eq!(parse.json()["id"], Value::Null);

    let empty = server.mcp_raw(Some(&server.owner), &[], "").await;
    assert_eq!(empty.status, 400);
    assert_eq!(empty.json()["error"]["code"], -32700);

    for bad in [
        json!(5),
        json!("text"),
        json!({"foo": 1}),
        json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": 7}),
        json!({"jsonrpc": "2.0", "id": [1], "method": "ping"}),
    ] {
        let raw = server.rpc(bad.clone()).await;
        assert_eq!(raw.status, 400, "{bad}");
        assert_eq!(raw.json()["error"]["code"], -32600, "{bad}");
    }

    let unknown = server.rpc(request(9, "resources/list", json!({}))).await;
    assert_eq!(unknown.status, 200);
    assert_eq!(unknown.json()["error"]["code"], -32601);
    assert_eq!(unknown.json()["id"], 9);

    for params in [
        request(1, "tools/call", json!({})),
        request(1, "tools/call", json!({"name": 5})),
        request(
            1,
            "tools/call",
            json!({"name": "list_projects", "arguments": [1]}),
        ),
        request(1, "tools/call", json!({"name": "no_such_tool"})),
        request(1, "initialize", json!({})),
    ] {
        let raw = server.rpc(params.clone()).await;
        assert_eq!(raw.json()["error"]["code"], -32602, "{params}");
    }
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let server = Server::start().await;
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
        "x".repeat(1_100_000)
    );
    let raw = server.mcp_raw(Some(&server.owner), &[], &big).await;
    assert_eq!(raw.status, 413, "{}", raw.body);
    assert_eq!(raw.json()["error"]["code"], -32600);
}

#[tokio::test]
async fn only_post_is_served_and_the_protocol_header_is_checked() {
    let server = Server::start().await;
    for method in ["GET", "DELETE", "PUT"] {
        let raw = server
            .http(method, "/mcp", Some(&server.owner), &[], "")
            .await;
        assert_eq!(raw.status, 405, "{method}");
    }
    let bad = server
        .mcp_raw(
            Some(&server.owner),
            &[("MCP-Protocol-Version", "1999-01-01")],
            &request(1, "ping", json!({})).to_string(),
        )
        .await;
    assert_eq!(bad.status, 400);
    let good = server
        .mcp_raw(
            Some(&server.owner),
            &[("MCP-Protocol-Version", "2025-06-18")],
            &request(1, "ping", json!({})).to_string(),
        )
        .await;
    assert_eq!(good.status, 200);
}

#[tokio::test]
async fn keys_are_required_and_cookies_do_not_count() {
    let server = Server::start().await;
    let ping = request(1, "ping", json!({})).to_string();
    let user = server.agent("bot").await;
    let (id, key) = server.new_key(user, "write").await;
    assert_eq!(server.mcp_raw(Some(&key), &[], &ping).await.status, 200);

    let none = server.mcp_raw(None, &[], &ping).await;
    let bad = server.mcp_raw(Some("aur_nonsense"), &[], &ping).await;
    let cookie = server
        .mcp_raw(None, &[("Cookie", "aurora_session=abc")], &ping)
        .await;
    for raw in [&none, &bad, &cookie] {
        assert_eq!(raw.status, 401, "{}", raw.body);
        assert_eq!(raw.header("www-authenticate").as_deref(), Some("Bearer"));
        assert!(raw.json()["error"].is_string());
    }

    server.rest("DELETE", &format!("/keys/{id}"), None).await;
    let revoked = server.mcp_raw(Some(&key), &[], &ping).await;
    assert_eq!(revoked.status, 401);
}

#[tokio::test]
async fn the_origin_must_be_this_server_or_localhost() {
    let server = Server::start().await;
    let ping = request(1, "ping", json!({})).to_string();
    let check = |origin: &'static str, host: &'static str| {
        let ping = ping.clone();
        let server = &server;
        async move {
            server
                .mcp_raw(
                    Some(&server.owner),
                    &[("Origin", origin), ("Host", host)],
                    &ping,
                )
                .await
                .status
        }
    };
    assert_eq!(check("https://evil.example", "127.0.0.1:8787").await, 403);
    assert_eq!(check("null", "127.0.0.1:8787").await, 403);
    assert_eq!(check("http://localhost:3000", "127.0.0.1:8787").await, 200);
    assert_eq!(check("http://127.0.0.1:8787", "127.0.0.1:8787").await, 200);
    assert_eq!(
        check("https://tracker.example", "tracker.example").await,
        200
    );
    // The Origin is checked before the key: a hostile page learns nothing.
    let raw = server
        .mcp_raw(None, &[("Origin", "https://evil.example")], &ping)
        .await;
    assert_eq!(raw.status, 403);
}

#[tokio::test]
async fn a_read_key_reads_and_cannot_write() {
    let server = Server::start().await;
    server.project().await;
    let user = server.agent("viewer").await;
    let (_, read) = server.new_key(user, "read").await;

    let projects = server.ok_as(&read, "list_projects", json!({})).await;
    assert_eq!(projects["projects"][0]["key"], "TRK");
    let me = server.ok_as(&read, "get_current_user", json!({})).await;
    assert_eq!(me["scope"], "read");
    assert_eq!(me["user"]["id"], user);

    let text = server
        .err_as(
            &read,
            "create_issue",
            json!({"project": "TRK", "title": "x"}),
        )
        .await;
    assert!(text.contains("read-only"), "{text}");
    for tool in [
        "update_issue",
        "move_issue",
        "add_comment",
        "link_issues",
        "create_document",
    ] {
        let text = server.err_as(&read, tool, json!({})).await;
        assert!(text.contains("read-only"), "{tool}: {text}");
    }
    let issues = server
        .ok_as(&read, "list_issues", json!({"project": "TRK"}))
        .await;
    assert_eq!(issues["total"], 0);
}

#[tokio::test]
async fn an_agent_works_an_issue_through_tools_only() {
    let server = Server::start().await;
    server.project().await;
    let bot = server.agent("bot").await;
    let helper = server.agent("helper").await;
    let (_, key) = server.new_key(bot, "write").await;
    let before = server.rest("GET", "/events?limit=500", None).await["events"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    let me = server.ok_as(&key, "get_current_user", json!({})).await;
    assert_eq!(me["user"]["name"], "bot");
    assert_eq!(me["role"], "agent");

    let projects = server.ok_as(&key, "list_projects", json!({})).await;
    assert_eq!(projects["projects"][0]["key"], "TRK");
    let project = server
        .ok_as(&key, "get_project", json!({"project": "TRK"}))
        .await;
    let states: Vec<&str> = project["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["state_type"].as_str().unwrap())
        .collect();
    assert!(
        states.contains(&"started") && states.contains(&"completed"),
        "{states:?}"
    );
    assert_eq!(project["issue_types"][0]["name"], "Task");
    server
        .rest("POST", "/projects/TRK/labels", Some(json!({"name": "bug"})))
        .await;

    let created = server
        .ok_as(
            &key,
            "create_issue",
            json!({"project": "TRK", "title": "Ship the MCP endpoint",
                   "description": "needle", "priority": "high", "labels": ["bug"]}),
        )
        .await;
    assert_eq!(created["identifier"], "TRK-1");
    let second = server
        .ok_as(
            &key,
            "create_issue",
            json!({"project": "TRK", "title": "Docs", "type": "task"}),
        )
        .await;
    assert_eq!(second["number"], 2);

    let updated = server
        .ok_as(
            &key,
            "update_issue",
            json!({"issue": "TRK-1", "title": "Ship MCP", "priority": null, "assignee": "helper"}),
        )
        .await;
    assert_eq!(updated["title"], "Ship MCP");
    assert_eq!(updated["priority"], Value::Null);
    assert_eq!(updated["assignee_id"], helper);
    assert_eq!(updated["description"], "needle");

    let moved = server
        .ok_as(
            &key,
            "move_issue",
            json!({"issue": "TRK-1", "state_type": "started"}),
        )
        .await;
    let issue = server.rest("GET", "/projects/TRK/issues/1", None).await;
    assert_eq!(moved["column_id"], issue["column_id"]);
    let by_name = server
        .ok_as(
            &key,
            "move_issue",
            json!({"project": "TRK", "number": 1, "column": "done"}),
        )
        .await;
    assert_ne!(by_name["column_id"], moved["column_id"]);

    let comment = server
        .ok_as(
            &key,
            "add_comment",
            json!({"issue": "TRK-1", "body": "ping @helper please review"}),
        )
        .await;
    assert_eq!(comment["mentions"][0]["name"], "helper");
    assert_eq!(comment["author"]["id"], bot);

    let linked = server
        .ok_as(
            &key,
            "link_issues",
            json!({"issue": "TRK-1", "to": "TRK-2", "kind": "blocks"}),
        )
        .await;
    assert_eq!(linked["link"]["kind"], "blocks");

    let found = server.ok_as(&key, "search", json!({"q": "needle"})).await;
    assert_eq!(found["hits"][0]["issue"]["identifier"], "TRK-1");
    let direct = server.ok_as(&key, "search", json!({"q": "TRK-2"})).await;
    assert_eq!(direct["hits"][0]["exact"], true);

    let detail = server
        .ok_as(&key, "get_issue", json!({"issue": "TRK-1"}))
        .await;
    assert_eq!(detail["links"]["blocks"][0]["identifier"], "TRK-2");
    assert_eq!(detail["comments_count"], 1);
    assert_eq!(detail["label_names"][0], "bug");
    let blocked = server
        .ok_as(&key, "get_issue", json!({"issue": "TRK-2"}))
        .await;
    // TRK-1 is in a completed column, so it no longer blocks.
    assert_eq!(blocked["blocked"], false);
    assert_eq!(blocked["links"]["blocked_by"][0]["identifier"], "TRK-1");

    let comments = server
        .ok_as(&key, "list_comments", json!({"issue": "TRK-1"}))
        .await;
    let id = comments["comments"][0]["id"].as_i64().unwrap();
    server
        .ok_as(
            &key,
            "edit_comment",
            json!({"issue": "TRK-1", "comment_id": id, "body": "done"}),
        )
        .await;
    server
        .ok_as(
            &key,
            "delete_comment",
            json!({"project": "TRK", "comment_id": id}),
        )
        .await;
    server
        .ok_as(
            &key,
            "unlink_issues",
            json!({"issue": "TRK-1", "link_id": linked["link"]["id"]}),
        )
        .await;

    let listed = server
        .ok_as(
            &key,
            "list_issues",
            json!({"project": "TRK", "state_type": "completed"}),
        )
        .await;
    assert_eq!(listed["issues"][0]["identifier"], "TRK-1");
    let archived = server
        .ok_as(&key, "archive_issue", json!({"issue": "TRK-1"}))
        .await;
    assert_eq!(archived["archived"], true);
    server
        .ok_as(&key, "unarchive_issue", json!({"issue": "TRK-1"}))
        .await;
    server
        .ok_as(&key, "archive_issue", json!({"issue": "TRK-1"}))
        .await;

    let events = server
        .rest("GET", &format!("/events?after={before}&limit=500"), None)
        .await;
    let events = events["events"].as_array().unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    for expected in [
        "issue.created",
        "issue.updated",
        "issue.moved",
        "comment.created",
        "comment.edited",
        "comment.deleted",
        "mention.created",
        "link.added",
        "link.removed",
        "issue.archived",
    ] {
        assert!(kinds.contains(&expected), "no {expected} in {kinds:?}");
    }
    for event in events {
        if event["kind"].as_str().unwrap().starts_with("label.") {
            continue; // the label was made over REST by the owner
        }
        assert_eq!(event["actor"], bot, "{event}");
    }
}

#[tokio::test]
async fn documents_and_initiatives_are_reachable() {
    let server = Server::start().await;
    server.project().await;
    server
        .rest(
            "POST",
            "/projects/TRK/initiatives",
            Some(json!({"name": "Launch", "status": "active"})),
        )
        .await;
    let user = server.agent("bot").await;
    let (_, key) = server.new_key(user, "write").await;

    let initiatives = server
        .ok_as(
            &key,
            "list_initiatives",
            json!({"project": "TRK", "status": "active"}),
        )
        .await;
    assert_eq!(initiatives["initiatives"][0]["name"], "Launch");
    let initiative = initiatives["initiatives"][0]["id"].as_i64().unwrap();
    let issue = server
        .ok_as(
            &key,
            "create_issue",
            json!({"project": "TRK", "title": "In launch", "initiative": initiative}),
        )
        .await;
    assert_eq!(issue["initiative_id"], initiative);
    let project = server
        .ok_as(&key, "get_project", json!({"project": "TRK"}))
        .await;
    assert_eq!(project["initiatives"][0]["progress"]["total"], 1);

    let doc = server
        .ok_as(
            &key,
            "create_document",
            json!({"project": "TRK", "title": "Plan", "body": "# Plan\nzebra"}),
        )
        .await;
    let id = doc["id"].as_i64().unwrap();
    let listed = server
        .ok_as(&key, "list_documents", json!({"project": "TRK"}))
        .await;
    assert_eq!(listed["documents"][0]["title"], "Plan");
    assert!(listed["documents"][0].get("body").is_none());
    let read = server
        .ok_as(&key, "get_document", json!({"project": "TRK", "id": id}))
        .await;
    assert_eq!(read["revision"], 1);
    let updated = server
        .ok_as(
            &key,
            "update_document",
            json!({"project": "TRK", "id": id, "body": "# Plan v2", "base_revision": 1}),
        )
        .await;
    assert_eq!(updated["revision"], 2);
    let text = server
        .err_as(
            &key,
            "update_document",
            json!({"project": "TRK", "id": id, "body": "stale", "base_revision": 1}),
        )
        .await;
    assert!(text.contains("conflict"), "{text}");
    let cut = server
        .ok_as(
            &key,
            "get_document",
            json!({"project": "TRK", "id": id, "max_body_chars": 4}),
        )
        .await;
    assert_eq!(cut["body"], "# Pl");
    assert_eq!(cut["body_truncated"], true);
}

#[tokio::test]
async fn tool_failures_are_results_that_say_what_to_fix() {
    let server = Server::start().await;
    server.project().await;
    let user = server.agent("bot").await;
    let (_, key) = server.new_key(user, "write").await;
    for title in ["a", "b"] {
        server
            .ok_as(
                &key,
                "create_issue",
                json!({"project": "TRK", "title": title}),
            )
            .await;
    }

    let text = server
        .err_as(&key, "get_issue", json!({"issue": "TRK-99"}))
        .await;
    assert!(text.contains("not found"), "{text}");
    let text = server
        .err_as(&key, "get_issue", json!({"issue": "garbage"}))
        .await;
    assert!(text.contains("TRK-12"), "{text}");
    let text = server.err_as(&key, "get_issue", json!({})).await;
    assert!(text.contains("`issue`"), "{text}");
    let text = server
        .err_as(&key, "get_project", json!({"project": "NOPE"}))
        .await;
    assert!(text.contains("not found"), "{text}");

    let text = server
        .err_as(
            &key,
            "move_issue",
            json!({"issue": "TRK-1", "column": "Nowhere"}),
        )
        .await;
    assert!(
        text.contains("its columns are") && text.contains("Backlog"),
        "{text}"
    );
    let text = server
        .err_as(
            &key,
            "move_issue",
            json!({"issue": "TRK-1", "column_id": 999999}),
        )
        .await;
    assert!(text.contains("columns"), "{text}");
    let text = server
        .err_as(&key, "move_issue", json!({"issue": "TRK-1"}))
        .await;
    assert!(text.contains("column_id, column or state_type"), "{text}");

    server
        .ok_as(
            &key,
            "link_issues",
            json!({"issue": "TRK-1", "to": 2, "kind": "blocks"}),
        )
        .await;
    let text = server
        .err_as(
            &key,
            "link_issues",
            json!({"issue": "TRK-2", "to": 1, "kind": "blocks"}),
        )
        .await;
    assert!(text.contains("cycle"), "{text}");
    let text = server
        .err_as(
            &key,
            "link_issues",
            json!({"issue": "TRK-1", "to": 1, "kind": "relates"}),
        )
        .await;
    assert!(text.starts_with("invalid"), "{text}");
    let text = server
        .err_as(
            &key,
            "link_issues",
            json!({"issue": "TRK-1", "to": 2, "kind": "sideways"}),
        )
        .await;
    assert!(text.contains("blocks, blocked_by or relates"), "{text}");

    let text = server
        .err_as(&key, "update_issue", json!({"issue": "TRK-1"}))
        .await;
    assert!(text.contains("nothing to change"), "{text}");
    let text = server
        .err_as(
            &key,
            "update_issue",
            json!({"issue": "TRK-1", "colour": "red"}),
        )
        .await;
    assert!(
        text.contains("unknown argument") && text.contains("colour"),
        "{text}"
    );
    let text = server
        .err_as(
            &key,
            "update_issue",
            json!({"issue": "TRK-1", "priority": "huge"}),
        )
        .await;
    assert!(text.contains("urgent, high, medium or low"), "{text}");
    let text = server
        .err_as(
            &key,
            "create_issue",
            json!({"project": "TRK", "title": "x", "labels": ["nope"]}),
        )
        .await;
    assert!(text.contains("no label"), "{text}");
    let text = server
        .err_as(&key, "create_issue", json!({"project": "TRK", "title": 5}))
        .await;
    assert!(text.contains("`title`"), "{text}");
    let text = server
        .err_as(&key, "add_comment", json!({"issue": "TRK-1", "body": ""}))
        .await;
    assert!(text.starts_with("invalid"), "{text}");
}

#[tokio::test]
async fn lists_are_bounded_and_paged() {
    let server = Server::start().await;
    server.project().await;
    let user = server.agent("bot").await;
    let (_, key) = server.new_key(user, "write").await;
    let long = "y".repeat(2000);
    for n in 0..3 {
        server
            .ok_as(
                &key,
                "create_issue",
                json!({"project": "TRK", "title": format!("issue {n}"), "description": long}),
            )
            .await;
    }
    let page = server
        .ok_as(&key, "list_issues", json!({"project": "TRK", "limit": 2}))
        .await;
    assert_eq!(page["issues"].as_array().unwrap().len(), 2);
    assert_eq!(page["total"], 3);
    assert_eq!(page["next_offset"], 2);
    assert_eq!(page["issues"][0]["description_truncated"], true);
    assert_eq!(
        page["issues"][0]["description"].as_str().unwrap().len(),
        300
    );
    let rest = server
        .ok_as(
            &key,
            "list_issues",
            json!({"project": "TRK", "limit": 2, "offset": 2}),
        )
        .await;
    assert_eq!(rest["issues"].as_array().unwrap().len(), 1);
    assert!(rest.get("next_offset").is_none());
    let full = server
        .ok_as(&key, "get_issue", json!({"issue": "TRK-1"}))
        .await;
    assert_eq!(full["description"].as_str().unwrap().len(), 2000);

    let result = server
        .call_as(&key, "list_issues", json!({"project": "TRK"}))
        .await;
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("TRK-1")
    );
    assert!(
        result["content"][1]["text"]
            .as_str()
            .unwrap()
            .starts_with('{')
    );
}
