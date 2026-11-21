//! Prompt templates over HTTP: the override routes, the preview, the variable
//! catalogue and the project rules, with the status codes the docs promise.

use aurora::routes::api_endpoints;
use aurora_store::Store;
use aurora_store::prompt::VARIABLES;
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

    fn replayed(&self) -> bool {
        self.head
            .to_ascii_lowercase()
            .contains("idempotent-replay: true")
    }
}

struct Server {
    addr: std::net::SocketAddr,
    owner: String,
    store: Store,
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
    /// An owner (id 1) and a member (id 2), each with a key; `public_url` is
    /// what the server was told to use for absolute links.
    async fn start(public_url: Option<&str>) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let options = aurora_store::OpenOptions {
            public_url: public_url.map(String::from),
            ..aurora_store::OpenOptions::default()
        };
        let store = Store::open_with(dir.path().join("test.db"), &options).unwrap();
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
            store.clone(),
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
        ));
        Server {
            addr,
            owner,
            store,
            _dir: dir,
            stop: Some(stop),
        }
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: &str,
        idem: Option<&str>,
    ) -> Reply {
        let auth = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
        let idem = idem.map_or(String::new(), |i| format!("Idempotency-Key: {i}\r\n"));
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}{idem}\
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
        self.raw(method, &format!("/api/v1{path}"), key, &body, None)
            .await
    }

    async fn as_owner(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.api(method, path, Some(&self.owner), body).await
    }

    async fn as_member(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        self.api(method, path, Some(MEMBER_KEY), body).await
    }

    /// An owner call that must succeed.
    async fn owner_ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let reply = self.as_owner(method, path, body).await;
        assert!(
            (200..300).contains(&reply.status),
            "{method} {path}: {} {}",
            reply.status,
            reply.body
        );
        serde_json::from_str(reply.body.trim()).unwrap_or(Value::Null)
    }

    /// Project AA with a Task and a Bug type and issue AA-1 (a Task); an agent
    /// `bot` with a key; returns (agent id, agent key, task type, bug type).
    async fn world(&self) -> (i64, String, i64, i64) {
        self.owner_ok(
            "POST",
            "/projects",
            Some(json!({"key": "AA", "name": "Alpha"})),
        )
        .await;
        let task = self
            .owner_ok(
                "POST",
                "/projects/AA/issue-types",
                Some(json!({"name": "Task"})),
            )
            .await["id"]
            .as_i64()
            .unwrap();
        let bug = self
            .owner_ok(
                "POST",
                "/projects/AA/issue-types",
                Some(json!({"name": "Bug"})),
            )
            .await["id"]
            .as_i64()
            .unwrap();
        self.owner_ok(
            "POST",
            "/projects/AA/issues",
            Some(json!({"title": "First issue", "type_id": task,
                        "description": "{{issue.title}} stays literal"})),
        )
        .await;
        let made = self
            .owner_ok(
                "POST",
                "/agents",
                Some(
                    json!({"name": "bot", "command": "claude", "projects": ["AA"],
                            "create_key": true}),
                ),
            )
            .await;
        (
            made["agent"]["id"].as_i64().unwrap(),
            made["key"].as_str().unwrap().to_string(),
            task,
            bug,
        )
    }
}

fn templates_path(agent: i64) -> String {
    format!("/agents/{agent}/templates")
}

fn preview_path(agent: i64) -> String {
    format!("/agents/{agent}/prompt-preview")
}

#[tokio::test]
async fn the_routes_are_in_the_table() {
    let table: Vec<(String, String)> = api_endpoints()
        .into_iter()
        .map(|e| (e.method.to_string(), e.path.to_string()))
        .collect();
    for (method, path) in [
        ("GET", "/agents/{id}/templates"),
        ("PUT", "/agents/{id}/templates"),
        ("DELETE", "/agents/{id}/templates/{template_id}"),
        ("POST", "/agents/{id}/prompt-preview"),
        ("GET", "/prompt/variables"),
    ] {
        assert!(
            table.contains(&(method.to_string(), path.to_string())),
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn every_route_answers_401_403_404_409_and_422_as_documented() {
    let s = Server::start(None).await;
    let (agent, agent_key, task, bug) = s.world().await;

    // 401: no key at all.
    for (method, path) in [
        ("GET", templates_path(agent)),
        ("PUT", templates_path(agent)),
        ("DELETE", format!("{}/1", templates_path(agent))),
        ("POST", preview_path(agent)),
        ("GET", "/prompt/variables".to_string()),
    ] {
        let body = (method != "GET").then(|| json!({}));
        assert_eq!(
            s.api(method, &path, None, body).await.status,
            401,
            "{method} {path}"
        );
    }

    // 403: a member cannot read, change or delete templates; an agent key
    // cannot either, and it cannot preview.
    let saved = s
        .as_owner(
            "PUT",
            &templates_path(agent),
            Some(json!({"project": "AA", "body": "Secret instructions"})),
        )
        .await;
    assert_eq!(saved.status, 201, "{}", saved.body);
    let template = saved.json()["id"].as_i64().unwrap();
    for key in [MEMBER_KEY, agent_key.as_str()] {
        for (method, path, body) in [
            ("GET", templates_path(agent), None),
            (
                "PUT",
                templates_path(agent),
                Some(json!({"project": "AA", "body": "x"})),
            ),
            (
                "DELETE",
                format!("{}/{template}", templates_path(agent)),
                None,
            ),
        ] {
            let reply = s.api(method, &path, Some(key), body).await;
            assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
            assert!(
                !reply.body.contains("Secret instructions"),
                "the text must not leak into a refusal"
            );
        }
    }
    let refused = s
        .api(
            "POST",
            &preview_path(agent),
            Some(&agent_key),
            Some(json!({"project": "AA", "number": 1})),
        )
        .await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(refused.error().contains("agents cannot preview"));

    // 404.
    for (method, path, body) in [
        ("GET", templates_path(9999), None),
        (
            "PUT",
            templates_path(9999),
            Some(json!({"project": "AA", "body": "x"})),
        ),
        (
            "DELETE",
            format!("{}/{template}", templates_path(9999)),
            None,
        ),
        ("DELETE", format!("{}/9999", templates_path(agent)), None),
        (
            "POST",
            preview_path(9999),
            Some(json!({"project": "AA", "number": 1})),
        ),
        (
            "POST",
            preview_path(agent),
            Some(json!({"project": "AA", "number": 99})),
        ),
        (
            "POST",
            preview_path(agent),
            Some(json!({"project": "NOPE", "number": 1})),
        ),
    ] {
        let reply = s.as_owner(method, &path, body).await;
        assert_eq!(reply.status, 404, "{method} {path}: {}", reply.body);
    }

    // 409: an issue type that overrides point at stays.
    s.owner_ok(
        "PUT",
        &templates_path(agent),
        Some(json!({"project": "AA", "issue_type_id": bug, "body": "Bug text"})),
    )
    .await;
    let blocked = s
        .as_owner("DELETE", &format!("/projects/AA/issue-types/{bug}"), None)
        .await;
    assert_eq!(blocked.status, 409, "{}", blocked.body);
    assert!(blocked.error().contains("prompt template"));

    // 422.
    for (what, body) in [
        (
            "an unknown variable",
            json!({"project": "AA", "body": "Hi {{issue.titel}}"}),
        ),
        (
            "an unclosed block",
            json!({"project": "AA", "body": "{{#if issue.title}}"}),
        ),
        ("an empty text", json!({"project": "AA", "body": "  "})),
        ("no project", json!({"body": "x"})),
        (
            "an unknown project",
            json!({"project": "NOPE", "body": "x"}),
        ),
        (
            "an unknown field",
            json!({"project": "AA", "body": "x", "extra": 1}),
        ),
        (
            "a type of another project",
            json!({"project": "AA", "issue_type_id": 9999, "body": "x"}),
        ),
        (
            "too long",
            json!({"project": "AA", "body": "x".repeat(20_001)}),
        ),
    ] {
        let reply = s.as_owner("PUT", &templates_path(agent), Some(body)).await;
        assert_eq!(reply.status, 422, "{what}: {}", reply.body);
    }
    let bad = s
        .as_owner(
            "PUT",
            &templates_path(agent),
            Some(json!({"project": "AA", "body": "line one\nHi {{issue.titel}}"})),
        )
        .await;
    assert!(
        bad.error().contains("unknown variable `issue.titel`")
            && bad.error().contains("(line 2, column 4)"),
        "{}",
        bad.error()
    );
    let _ = task;
    let notes: Vec<Value> = s.owner_ok("GET", &templates_path(agent), None).await["templates"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(notes.len(), 2, "no refused request stored anything");
}

#[tokio::test]
async fn saving_listing_replacing_and_deleting_overrides() {
    let s = Server::start(None).await;
    let (agent, _, task, _) = s.world().await;

    let empty = s.owner_ok("GET", &templates_path(agent), None).await;
    assert_eq!(empty["templates"], json!([]));
    assert_eq!(empty["default_source"], "builtin");

    let created = s
        .as_owner(
            "PUT",
            &templates_path(agent),
            Some(json!({"project": "AA", "body": "For {{issue.key}}"})),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let first = created.json();
    assert_eq!(first["project"], "AA");
    assert_eq!(first["issue_type_id"], Value::Null);
    assert_eq!(first["body"], "For {{issue.key}}");
    assert_eq!(first["agent_id"], agent);

    // The same scope again replaces: 200, same id.
    let replaced = s
        .as_owner(
            "PUT",
            &templates_path(agent),
            Some(json!({"project": "AA", "body": "For {{issue.key}}, v2"})),
        )
        .await;
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    assert_eq!(replaced.json()["id"], first["id"]);
    assert_eq!(replaced.json()["body"], "For {{issue.key}}, v2");

    let typed = s
        .as_owner(
            "PUT",
            &templates_path(agent),
            Some(json!({"project": "AA", "issue_type_id": task, "body": "Task text"})),
        )
        .await;
    assert_eq!(typed.status, 201);
    assert_eq!(typed.json()["issue_type"], "Task");

    let listed = s.owner_ok("GET", &templates_path(agent), None).await;
    let bodies: Vec<&str> = listed["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["For {{issue.key}}, v2", "Task text"]);

    // The agent's own instructions show as the default source.
    s.owner_ok(
        "PATCH",
        &format!("/agents/{agent}"),
        Some(json!({"instructions": "Be brief."})),
    )
    .await;
    assert_eq!(
        s.owner_ok("GET", &templates_path(agent), None).await["default_source"],
        "agent"
    );
    let bad_instructions = s
        .as_owner(
            "PATCH",
            &format!("/agents/{agent}"),
            Some(json!({"instructions": "{{nope}}"})),
        )
        .await;
    assert_eq!(bad_instructions.status, 422);
    assert!(
        bad_instructions
            .error()
            .starts_with("instructions: unknown variable")
    );
    let bad_create = s
        .as_owner(
            "POST",
            "/agents",
            Some(json!({"name": "broken", "command": "c", "instructions": "{{/if}}"})),
        )
        .await;
    assert_eq!(bad_create.status, 422, "{}", bad_create.body);

    let id = typed.json()["id"].as_i64().unwrap();
    let gone = s
        .as_owner("DELETE", &format!("{}/{id}", templates_path(agent)), None)
        .await;
    assert_eq!(gone.status, 204);
    let again = s
        .as_owner("DELETE", &format!("{}/{id}", templates_path(agent)), None)
        .await;
    assert_eq!(again.status, 404);
    let left = s.owner_ok("GET", &templates_path(agent), None).await;
    assert_eq!(left["templates"].as_array().unwrap().len(), 1);

    // The log says that something changed, never what the text was.
    let events = s
        .owner_ok("GET", "/events?kind_prefix=prompt_template.", None)
        .await;
    let text = events.to_string();
    assert!(text.contains("prompt_template.saved"), "{text}");
    assert!(
        !text.contains("Task text") && !text.contains("v2"),
        "{text}"
    );
}

#[tokio::test]
async fn deleting_an_agent_removes_its_overrides() {
    let s = Server::start(None).await;
    let (agent, _, _, _) = s.world().await;
    s.owner_ok(
        "PUT",
        &templates_path(agent),
        Some(json!({"project": "AA", "body": "x"})),
    )
    .await;
    assert_eq!(
        s.as_owner("DELETE", &format!("/agents/{agent}"), None)
            .await
            .status,
        204
    );
    let left: i64 = s
        .store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM prompt_template", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn a_repeated_request_id_saves_once_and_the_preview_ignores_ids() {
    let s = Server::start(None).await;
    let (agent, _, _, _) = s.world().await;
    let body = json!({"project": "AA", "body": "Once"}).to_string();
    let path = format!("/api/v1{}", templates_path(agent));
    let first = s
        .raw("PUT", &path, Some(&s.owner), &body, Some("template-0001"))
        .await;
    assert_eq!(first.status, 201);
    assert!(!first.replayed());
    let second = s
        .raw("PUT", &path, Some(&s.owner), &body, Some("template-0001"))
        .await;
    assert_eq!(second.status, 201, "the replay answers like the first call");
    assert!(second.replayed());
    assert_eq!(second.body, first.body);
    let events = s
        .owner_ok("GET", "/events?kind_prefix=prompt_template.", None)
        .await;
    assert_eq!(
        events.to_string().matches("prompt_template.saved").count(),
        1
    );

    let rows_before: i64 = s
        .store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM request_log", [], |r| r.get(0))?))
        .await
        .unwrap();
    let preview_body = json!({"project": "AA", "number": 1}).to_string();
    for _ in 0..2 {
        let reply = s
            .raw(
                "POST",
                &format!("/api/v1{}", preview_path(agent)),
                Some(&s.owner),
                &preview_body,
                Some("preview-0001"),
            )
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert!(!reply.replayed());
    }
    let rows_after: i64 = s
        .store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM request_log", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(rows_before, rows_after, "a preview is not remembered");
}

#[tokio::test]
async fn the_preview_shows_the_exact_message_for_owners_and_members() {
    let s = Server::start(Some("http://aurora.example:8787/")).await;
    let (agent, _, task, _) = s.world().await;

    // The built-in default, with the issue's text taken literally.
    let built_in = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1})),
        )
        .await;
    assert_eq!(built_in.status, 200, "{}", built_in.body);
    let shown = built_in.json();
    assert_eq!(shown["template_source"], "builtin");
    assert_eq!(shown["template_id"], Value::Null);
    assert_eq!(shown["truncated"], false);
    let text = shown["text"].as_str().unwrap();
    assert!(text.starts_with("You are bot. Your task is issue AA-1 \"First issue\""));
    assert!(text.contains("{{issue.title}} stays literal"), "no re-scan");
    assert!(text.contains("http://aurora.example:8787/p/AA/1"), "{text}");
    assert!(text.contains("(http://aurora.example:8787/mcp)"));
    assert_eq!(shown["chars"], text.chars().count());
    assert!(
        shown["variables_used"]
            .as_array()
            .unwrap()
            .contains(&json!("issue.key"))
    );

    // A stored override, seen by a member too.
    s.owner_ok(
        "PUT",
        &templates_path(agent),
        Some(json!({"project": "AA", "issue_type_id": task, "body": "Task: {{issue.title}}"})),
    )
    .await;
    let by_member = s
        .as_member(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "trigger": "assigned"})),
        )
        .await;
    assert_eq!(by_member.status, 200, "{}", by_member.body);
    assert_eq!(by_member.json()["text"], "Task: First issue");
    assert_eq!(by_member.json()["template_source"], "project_type");
    assert!(by_member.json()["template_id"].is_i64());

    // Text that is not saved yet, and a broken one.
    let inline = s
        .as_member(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1,
                        "template": "{{agent.name}} on {{issue.key}}: {{trigger}}",
                        "trigger": "schedule"})),
        )
        .await;
    assert_eq!(inline.json()["text"], "bot on AA-1: schedule");
    assert_eq!(inline.json()["template_source"], "inline");
    let broken = s
        .as_member(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "template": "ok\n{{issue.nope}}"})),
        )
        .await;
    assert_eq!(broken.status, 422);
    assert!(
        broken.error().contains("(line 2, column 1)"),
        "{}",
        broken.error()
    );

    // A mention carries the comment.
    let comment = s
        .owner_ok(
            "POST",
            "/projects/AA/issues/1/comments",
            Some(json!({"body": "@bot please look"})),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let mentioned = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "trigger": "mentioned",
                        "comment_id": comment,
                        "template": "{{trigger.actor}} said: {{trigger.comment}}"})),
        )
        .await;
    assert_eq!(mentioned.json()["text"], "owner said: @bot please look");

    // Bad requests.
    for (what, body) in [
        (
            "an unknown trigger",
            json!({"project": "AA", "number": 1, "trigger": "sometimes"}),
        ),
        ("a missing number", json!({"project": "AA"})),
        (
            "an unknown field",
            json!({"project": "AA", "number": 1, "x": 1}),
        ),
    ] {
        let reply = s.as_owner("POST", &preview_path(agent), Some(body)).await;
        assert_eq!(reply.status, 422, "{what}: {}", reply.body);
    }
    let stranger = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "trigger": "mentioned", "comment_id": 9999})),
        )
        .await;
    assert_eq!(stranger.status, 422, "a comment that is not on the issue");

    // A preview changes nothing.
    let events = s
        .owner_ok("GET", "/events?kind_prefix=prompt_template.", None)
        .await;
    assert_eq!(
        events.to_string().matches("prompt_template.saved").count(),
        1
    );
}

#[tokio::test]
async fn the_variable_catalogue_is_open_to_every_signed_in_user() {
    let s = Server::start(None).await;
    let (_, agent_key, _, _) = s.world().await;
    for key in [s.owner.as_str(), MEMBER_KEY, agent_key.as_str()] {
        let reply = s.api("GET", "/prompt/variables", Some(key), None).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let body = reply.json();
        let names: Vec<&str> = body["variables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = VARIABLES.iter().map(|v| v.name).collect();
        assert_eq!(names, expected);
        assert_eq!(body["limits"]["max_template_chars"], 20_000);
        assert_eq!(body["limits"]["max_depth"], 4);
        assert_eq!(body["limits"]["max_output_chars"], 200_000);
        assert_eq!(body["limits"]["max_comments"], 30);
        assert!(
            body["default_template"]
                .as_str()
                .unwrap()
                .contains("## How to work")
        );
        let comments = body["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "comments")
            .unwrap();
        assert_eq!(comments["type"], "list");
        assert_eq!(comments["item"], "record");
        assert!(comments["fields"].as_array().unwrap().len() >= 4);
    }
}

// ---- the agent page ----

async fn page(s: &Server, key: Option<&str>, path: &str) -> Reply {
    s.raw("GET", path, key, "", None).await
}

#[tokio::test]
async fn the_owner_sees_the_prompt_section_and_nobody_else_sees_any_template_text() {
    let s = Server::start(None).await;
    let (agent, agent_key, task, _) = s.world().await;
    let hostile = "<script>alert(1)</script> \"quoted\" & 'single' {{issue.key}}";
    s.owner_ok(
        "PUT",
        &templates_path(agent),
        Some(json!({"project": "AA", "issue_type_id": task, "body": hostile})),
    )
    .await;
    s.owner_ok(
        "PATCH",
        &format!("/agents/{agent}"),
        Some(json!({"instructions": "SECRET-INSTRUCTIONS <b>bold</b> {{issue.title}}"})),
    )
    .await;

    let owner = page(&s, Some(&s.owner), &format!("/agents/{agent}")).await;
    assert_eq!(owner.status, 200);
    let html = &owner.body;
    for needle in [
        "id=\"prompt\"",
        "data-form=\"save-template\"",
        "data-action=\"preview-prompt\"",
        "data-action=\"edit-template\"",
        "data-action=\"delete-template\"",
        "data-action=\"insert-variable\"",
        "the instructions of this agent",
        "id=\"pv-text\"",
        "id=\"t-body\"",
        "AA: Task",
    ] {
        assert!(html.contains(needle), "the agent page lacks {needle}");
    }
    // Every catalogue variable can be inserted with a click.
    for variable in VARIABLES {
        assert!(
            html.contains(&format!("<code>{}</code>", variable.name)),
            "{} is not offered",
            variable.name
        );
    }
    assert!(html.contains("data-text=\"{{issue.key}}\""));
    assert!(html.contains("data-text=\"{{#each comments}}"));
    assert!(html.contains("data-text=\"{{this.author}}\""));
    // Template text is escaped everywhere it appears.
    assert!(!html.contains("<script>alert(1)"), "{html}");
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<b>bold</b>"));
    // No inline script or style, and the only script is the page script.
    assert!(!html.contains("style=\""), "no inline styles");
    for piece in html.split("<script").skip(1) {
        assert!(
            piece.starts_with(" src=\"/assets/"),
            "inline script: {piece:.60}"
        );
    }
    assert!(!html.contains("onclick") && !html.contains("onchange"));

    // A member, an agent's key and a signed-out visitor see no template text
    // and no controls.
    for key in [Some(MEMBER_KEY), Some(agent_key.as_str())] {
        let seen = page(&s, key, &format!("/agents/{agent}")).await;
        if seen.status != 200 {
            assert!(!seen.body.contains("SECRET-INSTRUCTIONS"));
            continue;
        }
        for needle in [
            "SECRET-INSTRUCTIONS",
            "alert(1)",
            "save-template",
            "preview-prompt",
            "id=\"prompt\"",
            "Overrides",
            "insert-variable",
            "t-body",
        ] {
            assert!(!seen.body.contains(needle), "a non-owner sees {needle}");
        }
    }
    let anonymous = page(&s, None, &format!("/agents/{agent}")).await;
    assert!(!anonymous.body.contains("SECRET-INSTRUCTIONS"));
    assert!(!anonymous.body.contains("alert(1)"));
}

#[tokio::test]
async fn the_page_offers_the_default_source_and_every_project_and_type() {
    let s = Server::start(None).await;
    let (agent, _, _, _) = s.world().await;
    let fresh = page(&s, Some(&s.owner), &format!("/agents/{agent}"))
        .await
        .body;
    assert!(fresh.contains("the built-in default"), "{fresh}");
    assert!(fresh.contains("No overrides."));
    for option in ["value=\"AA\"", "AA: Task", "AA: Bug", "Any issue type"] {
        assert!(fresh.contains(option), "{option}");
    }
    s.owner_ok(
        "PATCH",
        &format!("/agents/{agent}"),
        Some(json!({"instructions": "Own words."})),
    )
    .await;
    let own = page(&s, Some(&s.owner), &format!("/agents/{agent}"))
        .await
        .body;
    assert!(own.contains("the instructions of this agent"));
    s.owner_ok(
        "PUT",
        &templates_path(agent),
        Some(json!({"project": "AA", "body": "Project level"})),
    )
    .await;
    let listed = page(&s, Some(&s.owner), &format!("/agents/{agent}"))
        .await
        .body;
    assert!(listed.contains("AA, any issue type"));
    assert!(listed.contains("13 characters"));
    assert!(!listed.contains("No overrides."));
}

#[tokio::test]
async fn the_script_and_styles_carry_the_prompt_screen_and_never_read_messages_as_markup() {
    let s = Server::start(None).await;
    let (agent, _, _, _) = s.world().await;
    let html = page(&s, Some(&s.owner), &format!("/agents/{agent}"))
        .await
        .body;
    let script = html
        .split("src=\"")
        .find(|piece| piece.starts_with("/assets/app"))
        .and_then(|piece| piece.split('"').next())
        .unwrap()
        .to_string();
    let js = s.raw("GET", &script, None, "", None).await.body;
    for needle in [
        "/prompt-preview",
        "'/templates'",
        "insert-variable",
        "edit-template",
        "delete-template",
        "preview-prompt",
        "save-template",
        "setSelectionRange",
    ] {
        assert!(js.contains(needle), "app.js lacks {needle}");
    }
    let preview = js.split("function previewPrompt").nth(1).unwrap();
    let preview = preview.split("var formHandlers").next().unwrap();
    assert!(preview.contains("shown.textContent = made.text"));
    assert!(!preview.contains("innerHTML"), "a message is never markup");

    let css_href = html
        .split("href=\"")
        .find(|piece| piece.starts_with("/assets/") && piece.contains(".css"))
        .and_then(|piece| piece.split('"').next())
        .unwrap()
        .to_string();
    let css = s.raw("GET", &css_href, None, "", None).await.body;
    assert!(css.contains("--target: 44px"));
    for rule in [
        ".template-list li",
        ".var-list li",
        ".prompt-text",
        ".prompt-form select",
    ] {
        assert!(css.contains(rule), "app.css lacks {rule}");
    }
    let rows = css.split(".template-list li,").nth(1).unwrap();
    assert!(
        rows.split('}')
            .next()
            .unwrap()
            .contains("min-height: var(--target)"),
        "list rows keep the touch target"
    );
}

#[tokio::test]
async fn the_page_preview_request_works_end_to_end() {
    let s = Server::start(None).await;
    let (agent, _, task, _) = s.world().await;
    // What the page sends for "Preview this text": the textarea's text, the
    // chosen issue and trigger; the answer is shown in an escaped block.
    let text = "<b>{{issue.title}}</b> {{trigger}} {{project.key}}-{{issue.number}}";
    let reply = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "trigger": "assigned", "template": text})),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let shown = reply.json();
    assert_eq!(shown["text"], "<b>First issue</b> assigned AA-1");
    assert_eq!(shown["chars"], 32);
    assert_eq!(shown["truncated"], false);
    assert_eq!(shown["template_source"], "inline");
    // Saving it and previewing "as saved" gives the same text.
    s.owner_ok(
        "PUT",
        &templates_path(agent),
        Some(json!({"project": "AA", "issue_type_id": task, "body": text})),
    )
    .await;
    let saved = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "trigger": "assigned"})),
        )
        .await
        .json();
    assert_eq!(saved["text"], shown["text"]);
    assert_eq!(saved["template_source"], "project_type");
    // A mistake comes back as a message the page shows inline.
    let mistake = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1, "template": "{{#each comments}}"})),
        )
        .await;
    assert_eq!(mistake.status, 422);
    assert!(
        mistake.error().contains("never closed"),
        "{}",
        mistake.error()
    );
}

const DOCS: &str = include_str!("../../../docs/api.md");

#[test]
fn the_docs_list_every_variable_and_every_limit() {
    for variable in VARIABLES {
        assert!(
            DOCS.contains(&format!("| `{}` |", variable.name)),
            "docs/api.md has no table row for the variable {}",
            variable.name
        );
    }
    // The language's limits as the code has them.
    // Each phrase is what the docs say, with the number the code has.
    let limits: [(&str, usize); 6] = [
        ("20000 characters", aurora_store::prompt::MAX_TEMPLATE_CHARS),
        ("200000 characters", aurora_store::prompt::MAX_OUTPUT_CHARS),
        ("nest at most 4", aurora_store::prompt::MAX_DEPTH),
        (
            "last 30 comments",
            usize::try_from(aurora_store::prompt::MAX_COMMENTS).unwrap(),
        ),
        ("cut at 4000", aurora_store::prompt::MAX_COMMENT_CHARS),
        ("at most 10000", aurora_store::projects::MAX_RULES_CHARS),
    ];
    for (text, value) in limits {
        assert!(DOCS.contains(text), "docs/api.md should say {text:?}");
        let number: usize = text
            .split(|c: char| !c.is_ascii_digit())
            .find(|part| !part.is_empty())
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(number, value, "docs/api.md says {text:?}");
    }
    for kind in ["prompt_template.saved", "prompt_template.deleted"] {
        assert!(
            DOCS.contains(kind),
            "docs/api.md should name the event {kind}"
        );
    }
    assert!(DOCS.contains("`prompt_templates.valid`"));
    assert!(DOCS.contains("--public-url"));
}

#[tokio::test]
async fn project_rules_are_set_with_patch_and_shown_to_agents() {
    let s = Server::start(None).await;
    let (agent, _, _, _) = s.world().await;
    let set = s
        .as_owner(
            "PATCH",
            "/projects/AA",
            Some(json!({"rules": "Always add a test."})),
        )
        .await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(set.json()["rules"], "Always add a test.");
    assert_eq!(set.json()["name"], "Alpha", "the name did not change");
    let both = s
        .as_member(
            "PATCH",
            "/projects/AA",
            Some(json!({"name": "Alpha 2", "rules": "Be kind."})),
        )
        .await;
    assert_eq!(both.status, 200, "{}", both.body);
    assert_eq!(both.json()["name"], "Alpha 2");
    assert_eq!(
        s.as_member("GET", "/projects/AA", None).await.json()["rules"],
        "Be kind."
    );
    let preview = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1})),
        )
        .await;
    assert!(
        preview.json()["text"]
            .as_str()
            .unwrap()
            .contains("## Project rules\nBe kind.")
    );

    // Clearing: null or blank.
    let cleared = s
        .as_owner("PATCH", "/projects/AA", Some(json!({"rules": null})))
        .await;
    assert_eq!(cleared.status, 200);
    assert_eq!(cleared.json()["rules"], "");
    let preview = s
        .as_owner(
            "POST",
            &preview_path(agent),
            Some(json!({"project": "AA", "number": 1})),
        )
        .await;
    assert!(
        !preview.json()["text"]
            .as_str()
            .unwrap()
            .contains("Project rules")
    );

    for (what, body) in [
        ("nothing to change", json!({})),
        ("rules too long", json!({"rules": "r".repeat(10_001)})),
        ("a key change", json!({"key": "ZZ"})),
        ("rules of the wrong type", json!({"rules": 5})),
    ] {
        let reply = s.as_owner("PATCH", "/projects/AA", Some(body)).await;
        assert_eq!(reply.status, 422, "{what}: {}", reply.body);
    }
    assert_eq!(
        s.as_owner("PATCH", "/projects/NOPE", Some(json!({"rules": "x"})))
            .await
            .status,
        404
    );
    assert_eq!(
        s.api("PATCH", "/projects/AA", None, Some(json!({"rules": "x"})))
            .await
            .status,
        401
    );
    let at_limit = s
        .as_owner(
            "PATCH",
            "/projects/AA",
            Some(json!({"rules": "r".repeat(10_000)})),
        )
        .await;
    assert_eq!(at_limit.status, 200);
}
