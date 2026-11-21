//! The agent registry over HTTP, the pages that edit it, and what it
//! enforces: a registered agent's key reaches only its allowed projects.

use std::collections::BTreeSet;

use aurora::routes::api_endpoints;
use aurora_store::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

mod common;

const MEMBER_KEY: &str = "aur_member_key_for_tests_only_not_a_secret";

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(self.body.trim())
            .unwrap_or_else(|_| panic!("not JSON: {} {}", self.status, self.body))
    }

    fn error(&self) -> String {
        self.json()["error"]
            .as_str()
            .unwrap_or_default()
            .to_string()
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
    /// An owner (id 1) and a member (id 2, a human), each with a key.
    async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        let owner = common::owner_key(&store).await;
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO user (id, name, kind, role) VALUES (2, 'member', 'human', 'member')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO api_key (user_id, name, prefix, key_hash, scope, created)
                     VALUES (2, 'tests', 'aur_memb', ?1, 'write', 1)",
                    [format!("{:x}", Sha256::digest(MEMBER_KEY.as_bytes()))],
                )?;
                Ok(())
            })
            .await
            .unwrap();
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

    async fn raw(&self, method: &str, path: &str, key: Option<&str>, body: &str) -> Reply {
        let auth = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).await.unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        Reply {
            status: head.split(' ').nth(1).unwrap().parse().unwrap(),
            head: head.to_string(),
            body: body.to_string(),
        }
    }

    async fn api(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> Reply {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        self.raw(method, &format!("/api/v1{path}"), key, &body)
            .await
    }

    /// An owner call that must succeed.
    async fn owner(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let reply = self.api(method, path, Some(&self.owner), body).await;
        assert!(
            (200..300).contains(&reply.status),
            "{method} {path}: {} {}",
            reply.status,
            reply.body
        );
        serde_json::from_str(reply.body.trim()).unwrap_or(Value::Null)
    }

    async fn mcp(&self, key: &str, tool: &str, arguments: Value) -> Value {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": tool, "arguments": arguments}});
        let reply = self
            .raw("POST", "/mcp", Some(key), &message.to_string())
            .await;
        assert_eq!(reply.status, 200, "{tool}: {}", reply.body);
        reply.json()["result"].clone()
    }

    /// Projects AA and BB, each with a task type and one issue titled "needle".
    async fn two_projects(&self) {
        for (key, name) in [("AA", "Alpha"), ("BB", "Beta")] {
            self.owner("POST", "/projects", Some(json!({"key": key, "name": name})))
                .await;
            self.owner(
                "POST",
                &format!("/projects/{key}/issue-types"),
                Some(json!({"name": "Task"})),
            )
            .await;
            self.owner(
                "POST",
                &format!("/projects/{key}/issues"),
                Some(json!({"title": format!("needle in {key}")})),
            )
            .await;
        }
    }

    /// A registered agent with a key, allowed on AA only.
    async fn agent_on_aa(&self) -> (Value, String) {
        let made = self
            .owner(
                "POST",
                "/agents",
                Some(
                    json!({"name": "bot", "command": "claude", "projects": ["AA"],
                            "create_key": true}),
                ),
            )
            .await;
        let key = made["key"].as_str().unwrap().to_string();
        (made["agent"].clone(), key)
    }
}

fn agent_body(name: &str) -> Value {
    json!({"name": name, "command": "claude"})
}

// ---- the routes ----

#[tokio::test]
async fn every_route_answers_401_403_404_409_and_422_as_documented() {
    let server = Server::start().await;
    let some = Some(server.owner.as_str());

    for (method, path) in [
        ("GET", "/agents"),
        ("POST", "/agents"),
        ("GET", "/agents/1"),
        ("PATCH", "/agents/1"),
        ("DELETE", "/agents/1"),
        ("POST", "/agents/1/enable"),
        ("POST", "/agents/1/disable"),
    ] {
        let body = (method != "GET").then(|| json!({}));
        assert_eq!(
            server.api(method, path, None, body).await.status,
            401,
            "{method} {path} without a key"
        );
    }

    let made = server
        .api("POST", "/agents", some, Some(agent_body("bot")))
        .await;
    assert_eq!(made.status, 201, "{}", made.body);
    let agent = made.json();
    let id = agent["id"].as_i64().unwrap();
    assert_eq!(agent["projects"], json!([]), "no projects unless given");
    assert_eq!(agent["command"], "claude");
    assert_eq!(agent["on_assign"], true);
    assert_eq!(agent["enabled"], true);
    assert!(agent.get("key").is_none(), "no key unless asked");

    let one = server
        .api("GET", &format!("/agents/{id}"), some, None)
        .await;
    assert_eq!((one.status, one.json()), (200, agent.clone()));
    assert_eq!(
        server.api("GET", "/agents", some, None).await.json(),
        json!([agent])
    );

    // Members read, but redacted, and cannot write.
    let member = Some(MEMBER_KEY);
    let seen = server
        .api("GET", &format!("/agents/{id}"), member, None)
        .await;
    assert_eq!(seen.status, 200);
    for secret in ["command", "args", "model", "instructions", "work_dir"] {
        assert!(
            seen.json().get(secret).is_none(),
            "{secret} leaked to a member"
        );
    }
    assert_eq!(seen.json()["name"], "bot");
    let listed = server.api("GET", "/agents", member, None).await.json();
    assert!(listed[0].get("command").is_none());
    for (method, path, body) in [
        ("POST", "/agents".to_string(), agent_body("other")),
        ("PATCH", format!("/agents/{id}"), json!({"enabled": false})),
        ("DELETE", format!("/agents/{id}"), json!(null)),
        ("POST", format!("/agents/{id}/enable"), json!(null)),
        ("POST", format!("/agents/{id}/disable"), json!(null)),
        // The permission check comes before the lookup: nothing leaks.
        (
            "PATCH",
            "/agents/999".to_string(),
            json!({"enabled": false}),
        ),
    ] {
        let body = (!body.is_null()).then_some(body);
        let reply = server.api(method, &path, member, body).await;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
    }

    // 404
    for (method, path) in [
        ("GET", "/agents/999"),
        ("PATCH", "/agents/999"),
        ("DELETE", "/agents/999"),
        ("POST", "/agents/999/enable"),
        ("POST", "/agents/999/disable"),
    ] {
        let body = (method == "PATCH").then(|| json!({"enabled": true}));
        assert_eq!(
            server.api(method, path, some, body).await.status,
            404,
            "{method} {path}"
        );
    }
    let unknown_project = json!({"name": "x1", "command": "c", "projects": ["NOPE"]});
    assert_eq!(
        server
            .api("POST", "/agents", some, Some(unknown_project))
            .await
            .status,
        422,
        "an unknown project in the list is an invalid field"
    );
    let unknown_user = json!({"user_id": 999, "command": "c"});
    assert_eq!(
        server
            .api("POST", "/agents", some, Some(unknown_user))
            .await
            .status,
        404
    );

    // 409
    let again = server
        .api("POST", "/agents", some, Some(agent_body("bot")))
        .await;
    assert_eq!(again.status, 409, "{}", again.body);
    let user_id = agent["user_id"].as_i64().unwrap();
    let twice = server
        .api(
            "POST",
            "/agents",
            some,
            Some(json!({"user_id": user_id, "command": "c"})),
        )
        .await;
    assert_eq!(twice.status, 409, "{}", twice.body);

    // 422
    for (label, body) in [
        ("no command", json!({"name": "n1"})),
        ("no name or user", json!({"command": "c"})),
        (
            "name and user",
            json!({"name": "n2", "user_id": user_id, "command": "c"}),
        ),
        ("empty command", json!({"name": "n3", "command": ""})),
        (
            "unknown field",
            json!({"name": "n4", "command": "c", "secret": "x"}),
        ),
        (
            "zero concurrency",
            json!({"name": "n5", "command": "c", "max_concurrent": 0}),
        ),
        (
            "short timeout",
            json!({"name": "n6", "command": "c", "timeout_seconds": 5}),
        ),
        (
            "create_key not a flag",
            json!({"name": "n7", "command": "c", "create_key": "yes"}),
        ),
        ("a human", json!({"user_id": 2, "command": "c"})),
    ] {
        let reply = server.api("POST", "/agents", some, Some(body)).await;
        assert_eq!(reply.status, 422, "{label}: {}", reply.body);
        assert!(!reply.error().is_empty(), "{label}");
    }
    let bad_patch = server
        .api(
            "PATCH",
            &format!("/agents/{id}"),
            some,
            Some(json!({"max_concurrent": 99})),
        )
        .await;
    assert_eq!(bad_patch.status, 422);
    let unknown_patch = server
        .api(
            "PATCH",
            &format!("/agents/{id}"),
            some,
            Some(json!({"name": "renamed"})),
        )
        .await;
    assert_eq!(unknown_patch.status, 422, "name is not patchable");
    // None of the refused creates left an agent user behind.
    let users = server.api("GET", "/users", some, None).await.json();
    assert_eq!(
        users.as_array().unwrap().len(),
        3,
        "owner, member and bot only: {users}"
    );

    // PATCH, enable, disable, delete
    server
        .owner(
            "POST",
            "/projects",
            Some(json!({"key": "AA", "name": "Alpha"})),
        )
        .await;
    let patched = server
        .api(
            "PATCH",
            &format!("/agents/{id}"),
            some,
            Some(
                json!({"model": "opus", "on_mention": false, "args": ["-p", "x"],
                        "projects": ["AA"], "max_concurrent": 3}),
            ),
        )
        .await;
    assert_eq!(patched.status, 200, "{}", patched.body);
    let patched = patched.json();
    assert_eq!(patched["model"], "opus");
    assert_eq!(patched["on_mention"], false);
    assert_eq!(patched["on_assign"], true, "absent fields stay");
    assert_eq!(patched["projects"], json!(["AA"]));
    let cleared = server
        .api(
            "PATCH",
            &format!("/agents/{id}"),
            some,
            Some(json!({"model": null})),
        )
        .await
        .json();
    assert_eq!(cleared["model"], Value::Null);

    let off = server
        .api("POST", &format!("/agents/{id}/disable"), some, None)
        .await;
    assert_eq!(
        (off.status, off.json()["enabled"].clone()),
        (200, json!(false))
    );
    let off_again = server
        .api("POST", &format!("/agents/{id}/disable"), some, None)
        .await;
    assert_eq!(off_again.status, 200);
    let on = server
        .api("POST", &format!("/agents/{id}/enable"), some, None)
        .await;
    assert_eq!(
        (on.status, on.json()["enabled"].clone()),
        (200, json!(true))
    );

    let gone = server
        .api("DELETE", &format!("/agents/{id}"), some, None)
        .await;
    assert_eq!(gone.status, 204);
    assert_eq!(
        server
            .api("GET", &format!("/agents/{id}"), some, None)
            .await
            .status,
        404
    );
    assert_eq!(
        server
            .api("DELETE", &format!("/agents/{id}"), some, None)
            .await
            .status,
        404
    );

    // A read key reads the registry but cannot change it.
    let read_key = server
        .owner(
            "POST",
            "/keys",
            Some(json!({"name": "ro", "scope": "read"})),
        )
        .await["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        server
            .api("GET", "/agents", Some(&read_key), None)
            .await
            .status,
        200
    );
    let blocked = server
        .api("POST", "/agents", Some(&read_key), Some(agent_body("nope")))
        .await;
    assert_eq!(blocked.status, 403);
}

#[tokio::test]
async fn create_key_returns_the_key_once_and_it_acts_only_as_the_agent() {
    let server = Server::start().await;
    server.two_projects().await;
    let (agent, key) = server.agent_on_aa().await;
    assert!(key.starts_with("aur_"));
    let user_id = agent["user_id"].as_i64().unwrap();

    let again = server
        .api(
            "GET",
            &format!("/agents/{}", agent["id"]),
            Some(&server.owner),
            None,
        )
        .await;
    assert!(!again.body.contains(&key), "the key is not shown again");
    let listing = server
        .api(
            "GET",
            &format!("/keys?user_id={user_id}"),
            Some(&server.owner),
            None,
        )
        .await;
    assert!(!listing.body.contains(&key));
    assert_eq!(listing.json().as_array().unwrap().len(), 1);
    assert_eq!(listing.json()[0]["scope"], "write");
    assert_eq!(listing.json()[0]["name"], "agent key");

    // The response that carries the key is not cacheable.
    let second = server
        .api(
            "POST",
            "/agents",
            Some(&server.owner),
            Some(json!({"name": "bot2", "command": "c", "create_key": true})),
        )
        .await;
    assert_eq!(second.status, 201);
    assert_eq!(second.header("cache-control").as_deref(), Some("no-store"));
    let second_json = second.json();
    assert!(second_json["key"].as_str().unwrap().starts_with("aur_"));
    assert_eq!(
        second_json["key_info"]["user_id"],
        second_json["agent"]["user_id"]
    );

    let me = server.api("GET", "/auth/me", Some(&key), None).await.json();
    assert_eq!(me["user"]["id"], user_id);
    assert_eq!(me["user"]["kind"], "agent");
    assert_eq!(me["agent"]["id"], agent["id"]);
    assert_eq!(me["agent"]["enabled"], true);
    assert_eq!(me["agent"]["projects"], json!(["AA"]));

    // It cannot create users, manage anyone else's keys, register agents.
    let denied = [
        (
            "POST",
            "/users".to_string(),
            Some(json!({"name": "sneaky"})),
        ),
        ("GET", "/users".to_string(), None),
        ("GET", "/keys?user_id=1".to_string(), None),
        (
            "POST",
            "/keys".to_string(),
            Some(json!({"name": "mine", "user_id": 1})),
        ),
        ("POST", "/agents".to_string(), Some(agent_body("copy"))),
        (
            "PATCH",
            format!("/agents/{}", agent["id"]),
            Some(json!({"projects": ["BB"]})),
        ),
        ("DELETE", format!("/agents/{}", agent["id"]), None),
        ("POST", format!("/agents/{}/disable", agent["id"]), None),
    ];
    for (method, path, body) in denied {
        let reply = server.api(method, &path, Some(&key), body).await;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
    }
    let owner_keys = server
        .api("GET", "/keys", Some(&server.owner), None)
        .await
        .json();
    let owner_key_id = owner_keys[0]["id"].as_i64().unwrap();
    for (method, path) in [
        ("DELETE", format!("/keys/{owner_key_id}")),
        ("POST", format!("/keys/{owner_key_id}/rotate")),
    ] {
        assert_eq!(
            server.api(method, &path, Some(&key), None).await.status,
            403,
            "{path}"
        );
    }
    assert_eq!(
        server
            .api("GET", "/auth/me", Some(&server.owner), None)
            .await
            .status,
        200
    );

    // Whatever it does is recorded as the agent, not as someone else.
    let made = server
        .api(
            "POST",
            "/projects/AA/issues",
            Some(&key),
            Some(json!({"title": "by the agent"})),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.body);
    let events = server
        .api(
            "GET",
            "/events?kind_prefix=issue.created&project=AA",
            Some(&server.owner),
            None,
        )
        .await
        .json();
    let last = events["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["actor"], user_id);
}

// ---- enforcement ----

fn concrete_for(path: &str, key: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .map_or(rest.len(), |e| start + e + 1);
        let name = &rest[start + 1..end - 1];
        out.push_str(if name == "key" { key } else { "1" });
        rest = &rest[end..];
    }
    out.push_str(rest);
    format!("/api/v1{out}")
}

#[tokio::test]
async fn every_project_route_in_the_table_is_closed_to_a_denied_project() {
    let server = Server::start().await;
    server.two_projects().await;
    let (_, key) = server.agent_on_aa().await;
    let mut checked = 0;
    let mut methods = BTreeSet::new();
    for endpoint in api_endpoints() {
        if !endpoint.path.contains("{key}") {
            continue;
        }
        // A project that exists but is not allowed, and one that does not exist.
        for project in ["BB", "ZZ"] {
            let path = concrete_for(endpoint.path, project);
            let body = if endpoint.method == "GET" || endpoint.method == "DELETE" {
                String::new()
            } else {
                "{}".to_string()
            };
            let reply = server.raw(endpoint.method, &path, Some(&key), &body).await;
            assert_eq!(
                reply.status, 403,
                "{} {} must be closed to a denied project: {}",
                endpoint.method, endpoint.path, reply.body
            );
            assert_eq!(
                reply.error(),
                format!("this agent may not access project {project}"),
                "{} {}",
                endpoint.method,
                endpoint.path
            );
            checked += 1;
        }
        methods.insert(endpoint.method);
    }
    assert!(checked > 80, "the table looks short: {checked}");
    for method in ["GET", "POST", "PATCH", "PUT", "DELETE"] {
        assert!(methods.contains(method), "no {method} route was checked");
    }
    // Percent-encoding does not get around the check.
    let sneaky = server
        .raw("GET", "/api/v1/projects/%42%42/issues", Some(&key), "")
        .await;
    assert_eq!(sneaky.status, 403, "{}", sneaky.body);

    // The allowed project works through the same routes.
    for path in [
        "/projects/AA",
        "/projects/AA/issues",
        "/projects/AA/columns",
    ] {
        let reply = server.api("GET", path, Some(&key), None).await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
    }
    // The owner is not restricted.
    assert_eq!(
        server
            .api("GET", "/projects/BB/issues", Some(&server.owner), None)
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn lists_search_events_and_project_creation_are_filtered_for_a_registered_agent() {
    let server = Server::start().await;
    server.two_projects().await;
    let (_, key) = server.agent_on_aa().await;

    let projects = server
        .api("GET", "/projects", Some(&key), None)
        .await
        .json();
    let keys: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["AA"]);
    let all = server
        .api("GET", "/projects", Some(&server.owner), None)
        .await
        .json();
    assert_eq!(all.as_array().unwrap().len(), 2);

    let hits = server
        .api("GET", "/search?q=needle", Some(&key), None)
        .await
        .json();
    let found: Vec<&str> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["project"].as_str().unwrap())
        .collect();
    assert_eq!(found, ["AA"], "{hits}");
    let owner_hits = server
        .api("GET", "/search?q=needle", Some(&server.owner), None)
        .await
        .json();
    assert_eq!(owner_hits["hits"].as_array().unwrap().len(), 2);
    let exact = server
        .api("GET", "/search?q=BB-1", Some(&key), None)
        .await
        .json();
    assert_eq!(
        exact["hits"].as_array().unwrap().len(),
        0,
        "an identifier does not leak: {exact}"
    );
    let denied = server
        .api("GET", "/search?q=needle&project=BB", Some(&key), None)
        .await;
    assert_eq!(denied.status, 403);
    assert_eq!(denied.error(), "this agent may not access project BB");

    let events = server
        .api("GET", "/events?limit=500", Some(&key), None)
        .await
        .json();
    let list = events["events"].as_array().unwrap();
    assert!(!list.is_empty());
    assert!(
        list.iter().all(|e| e["project"] == "AA"),
        "only AA events, none without a project: {events}"
    );
    let owner_events = server
        .api("GET", "/events?limit=500", Some(&server.owner), None)
        .await
        .json();
    assert!(
        owner_events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "agent.created")
    );
    assert_eq!(
        server
            .api("GET", "/events?project=BB", Some(&key), None)
            .await
            .status,
        403
    );

    let make = server
        .api(
            "POST",
            "/projects",
            Some(&key),
            Some(json!({"key": "CC", "name": "Gamma"})),
        )
        .await;
    assert_eq!(make.status, 403, "{}", make.body);
    for path in ["/consumers", "/consumers/x", "/consumers/x/events"] {
        assert_eq!(
            server.api("GET", path, Some(&key), None).await.status,
            403,
            "{path}"
        );
    }
    assert_eq!(
        server
            .api(
                "PUT",
                "/consumers/mine",
                Some(&key),
                Some(json!({"start": "latest"}))
            )
            .await
            .status,
        403
    );
    assert_eq!(
        server
            .raw("GET", "/events/stream", Some(&key), "")
            .await
            .status,
        403
    );
    assert_eq!(
        server.raw("GET", "/p/BB", Some(&key), "").await.status,
        403,
        "pages are closed to its key"
    );
}

#[tokio::test]
async fn a_disabled_agent_may_only_ask_who_it_is() {
    let server = Server::start().await;
    server.two_projects().await;
    let (agent, key) = server.agent_on_aa().await;
    let id = agent["id"].as_i64().unwrap();
    assert_eq!(
        server
            .api("GET", "/projects/AA", Some(&key), None)
            .await
            .status,
        200
    );

    server
        .owner("POST", &format!("/agents/{id}/disable"), None)
        .await;
    for (method, path) in [
        ("GET", "/projects"),
        ("GET", "/projects/AA/issues"),
        ("GET", "/search?q=needle"),
        ("GET", "/events"),
        ("GET", "/agents"),
        ("GET", "/keys"),
        ("POST", "/projects/AA/issues"),
    ] {
        let body = (method == "POST").then(|| json!({"title": "x"}));
        let reply = server.api(method, path, Some(&key), body).await;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
        assert_eq!(reply.error(), "agent is disabled", "{method} {path}");
    }
    let me = server.api("GET", "/auth/me", Some(&key), None).await;
    assert_eq!(me.status, 200);
    assert_eq!(me.json()["agent"]["enabled"], false);

    let mcp = server
        .raw(
            "POST",
            "/mcp",
            Some(&key),
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
        )
        .await;
    assert_eq!(mcp.status, 403);

    server
        .owner("POST", &format!("/agents/{id}/enable"), None)
        .await;
    assert_eq!(
        server
            .api("GET", "/projects/AA", Some(&key), None)
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn humans_and_unregistered_agents_are_not_restricted_and_deleting_the_entry_lifts_it() {
    let server = Server::start().await;
    server.two_projects().await;

    let loose = server
        .owner("POST", "/users", Some(json!({"name": "loose"})))
        .await;
    let loose_key = server
        .owner(
            "POST",
            "/keys",
            Some(json!({"name": "k", "user_id": loose["id"]})),
        )
        .await["key"]
        .as_str()
        .unwrap()
        .to_string();
    for key in [loose_key.as_str(), MEMBER_KEY] {
        let projects = server.api("GET", "/projects", Some(key), None).await.json();
        assert_eq!(projects.as_array().unwrap().len(), 2);
        assert_eq!(
            server
                .api("GET", "/projects/BB/issues", Some(key), None)
                .await
                .status,
            200
        );
        let hits = server
            .api("GET", "/search?q=needle", Some(key), None)
            .await
            .json();
        assert_eq!(hits["hits"].as_array().unwrap().len(), 2);
    }
    let me = server
        .api("GET", "/auth/me", Some(&loose_key), None)
        .await
        .json();
    assert_eq!(me["agent"], Value::Null);

    // Registering the same user is what restricts it, and deleting lifts it.
    let made = server
        .owner(
            "POST",
            "/agents",
            Some(json!({"user_id": loose["id"], "command": "c"})),
        )
        .await;
    assert_eq!(
        server
            .api("GET", "/projects/BB/issues", Some(&loose_key), None)
            .await
            .status,
        403
    );
    server
        .owner("DELETE", &format!("/agents/{}", made["id"]), None)
        .await;
    assert_eq!(
        server
            .api("GET", "/projects/BB/issues", Some(&loose_key), None)
            .await
            .status,
        200
    );
}

// ---- MCP ----

#[tokio::test]
async fn mcp_enforces_the_allow_list_and_lists_agents_without_commands() {
    let server = Server::start().await;
    server.two_projects().await;
    let (_, key) = server.agent_on_aa().await;

    let projects = server.mcp(&key, "list_projects", json!({})).await;
    assert_eq!(projects["isError"], false);
    let keys: Vec<&str> = projects["structuredContent"]["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["AA"]);

    for (tool, arguments) in [
        ("get_project", json!({"project": "BB"})),
        ("list_issues", json!({"project": "BB"})),
        ("get_issue", json!({"issue": "BB-1"})),
        ("get_issue", json!({"project": "BB", "number": 1})),
        ("create_issue", json!({"project": "BB", "title": "nope"})),
        ("search", json!({"q": "needle", "project": "BB"})),
        ("list_documents", json!({"project": "BB"})),
        ("list_initiatives", json!({"project": "BB"})),
    ] {
        let result = server.mcp(&key, tool, arguments).await;
        assert_eq!(result["isError"], true, "{tool}: {result}");
        assert_eq!(
            result["content"][0]["text"], "this agent may not access project BB",
            "{tool}"
        );
    }

    let allowed = server
        .mcp(&key, "get_issue", json!({"issue": "AA-1"}))
        .await;
    assert_eq!(allowed["isError"], false, "{allowed}");
    let created = server
        .mcp(
            &key,
            "create_issue",
            json!({"project": "AA", "title": "from mcp"}),
        )
        .await;
    assert_eq!(created["isError"], false, "{created}");

    let found = server.mcp(&key, "search", json!({"q": "needle"})).await;
    assert_eq!(found["isError"], false);
    let text = found["structuredContent"].to_string();
    assert!(
        text.contains("AA") && !text.contains("needle in BB"),
        "{text}"
    );

    // list_agents: no command line, not even for the owner.
    for who in [key.as_str(), server.owner.as_str()] {
        let listed = server.mcp(who, "list_agents", json!({})).await;
        assert_eq!(listed["isError"], false, "{listed}");
        let agents = listed["structuredContent"]["agents"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["name"], "bot");
        assert!(agents[0].get("command").is_none(), "{listed}");
        assert!(!listed.to_string().contains("claude"), "{listed}");
    }

    let owner_sees = server
        .mcp(&server.owner, "get_issue", json!({"issue": "BB-1"}))
        .await;
    assert_eq!(owner_sees["isError"], false);
}

// ---- pages ----

#[tokio::test]
async fn the_agent_pages_list_create_edit_and_escape() {
    let server = Server::start().await;
    server.two_projects().await;
    let tricky = server
        .owner(
            "POST",
            "/agents",
            Some(json!({"name": "pager", "command": "run-<b>it</b>",
                        "description": "<script>alert(1)</script>",
                        "args": ["--flag=\"quoted\"", "second"],
                        "instructions": "</textarea><i>x</i>",
                        "projects": ["BB"], "create_key": true})),
        )
        .await;
    let secret = tricky["key"].as_str().unwrap().to_string();
    let id = tricky["agent"]["id"].as_i64().unwrap();
    let owner = Some(server.owner.as_str());

    // The nav link is on every page.
    let home = server.raw("GET", "/?list=1", owner, "").await;
    assert_eq!(home.status, 200);
    assert!(home.body.contains("href=\"/agents\""), "no Agents link");

    let list = server.raw("GET", "/agents", owner, "").await;
    assert_eq!(list.status, 200);
    assert_eq!(list.header("cache-control").as_deref(), Some("no-store"));
    assert!(list.body.contains("data-form=\"new-agent\""));
    assert!(list.body.contains("href=\"/agents/") && list.body.contains("pager"));
    assert!(list.body.contains("name=\"create_key\""));
    assert!(list.body.contains("id=\"key-box\"") && list.body.contains("hidden"));
    assert!(
        !list.body.contains(&secret),
        "the key must not be in the list page"
    );
    assert!(!list.body.contains("<script>alert"), "escaped");
    assert!(
        !list.body.contains("<script>") || list.body.matches("<script").count() == 1,
        "only the app script"
    );

    let edit = server.raw("GET", &format!("/agents/{id}"), owner, "").await;
    assert_eq!(edit.status, 200);
    assert!(edit.body.contains("data-form=\"edit-agent\""));
    assert!(edit.body.contains("data-action=\"disable-agent\""));
    assert!(edit.body.contains("data-action=\"delete-agent\""));
    assert!(
        edit.body.contains("run-&lt;b&gt;it&lt;/b&gt;"),
        "command escaped"
    );
    assert!(!edit.body.contains("<b>it</b>"));
    assert!(
        edit.body.contains("&lt;/textarea&gt;&lt;i&gt;x&lt;/i&gt;"),
        "instructions escaped"
    );
    assert!(edit.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(edit.body.contains("--flag=&quot;quoted&quot;"));
    assert!(
        edit.body.contains("value=\"BB\" checked"),
        "BB is ticked: {}",
        edit.body
    );
    assert!(!edit.body.contains("value=\"AA\" checked"));
    assert!(edit.body.contains("data-action=\"revoke-key\""));
    assert!(edit.body.contains(&secret[..8]), "the key prefix is listed");
    assert!(
        !edit.body.contains(&secret),
        "never the key: it was shown once"
    );
    assert_eq!(edit.header("cache-control").as_deref(), Some("no-store"));
    assert_eq!(
        server.raw("GET", "/agents/999", owner, "").await.status,
        404
    );

    // A member sees the agent, not the command or the controls.
    let seen = server
        .raw("GET", &format!("/agents/{id}"), Some(MEMBER_KEY), "")
        .await;
    assert_eq!(seen.status, 200);
    assert!(seen.body.contains("pager"));
    assert!(!seen.body.contains("run-"), "the command is redacted");
    assert!(!seen.body.contains("quoted"));
    assert!(!seen.body.contains("textarea"));
    assert!(!seen.body.contains("data-form=\"edit-agent\""));
    assert!(!seen.body.contains("data-action=\"delete-agent\""));
    let member_list = server.raw("GET", "/agents", Some(MEMBER_KEY), "").await;
    assert_eq!(member_list.status, 200);
    assert!(!member_list.body.contains("data-form=\"new-agent\""));

    // A registered agent's key gets none of the pages.
    assert_eq!(
        server.raw("GET", "/agents", Some(&secret), "").await.status,
        403
    );

    // Disabled agents show their state and offer to enable.
    server
        .owner("POST", &format!("/agents/{id}/disable"), None)
        .await;
    let off = server.raw("GET", &format!("/agents/{id}"), owner, "").await;
    assert!(off.body.contains("disabled") && off.body.contains("data-action=\"enable-agent\""));
}

#[tokio::test]
async fn the_page_script_and_styles_carry_the_agent_screens() {
    let server = Server::start().await;
    let owner = Some(server.owner.as_str());
    let page = server.raw("GET", "/agents", owner, "").await;
    let script = page
        .body
        .split("src=\"")
        .find(|piece| piece.starts_with("/assets/app"))
        .and_then(|piece| piece.split('"').next())
        .unwrap()
        .to_string();
    let js = server.raw("GET", &script, None, "").await;
    assert_eq!(js.status, 200);
    for needle in [
        "new-agent",
        "edit-agent",
        "claude mcp add",
        "revoke-key",
        "delete-agent",
    ] {
        assert!(js.body.contains(needle), "app.js lacks {needle}");
    }
    assert!(!js.body.contains("localStorage"), "the key is never stored");
    assert!(
        !js.body.contains("location.href = '/agents?"),
        "the key is never put in a URL"
    );
}
