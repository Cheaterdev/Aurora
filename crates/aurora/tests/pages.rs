//! The server-rendered pages, the assets and the live event stream, over the real router.

use std::net::SocketAddr;
use std::time::Duration;

use aurora_store::Store;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const PASSWORD: &str = "Correct-Horse-Battery-9";

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn parse_head(head: &str) -> (u16, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    (status, headers)
}

fn request_text(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in headers {
        text.push_str(&format!("{k}: {v}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    text
}

async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Resp {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(request_text(method, path, headers, body).as_bytes())
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let (status, headers) = parse_head(head);
    Resp {
        status,
        headers,
        body: body.to_string(),
    }
}

async fn get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> Resp {
    match cookie {
        Some(cookie) => send(addr, "GET", path, &[("Cookie", cookie)], "").await,
        None => send(addr, "GET", path, &[], "").await,
    }
}

async fn form(addr: SocketAddr, path: &str, body: &str) -> Resp {
    send(
        addr,
        "POST",
        path,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body,
    )
    .await
}

fn session_cookie(resp: &Resp) -> String {
    let set = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("no Set-Cookie in {} {}", resp.status, resp.body));
    set.split(';').next().unwrap().to_string()
}

struct Server {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(aurora::serve(
            listener,
            store,
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
        ));
        Server {
            addr,
            stop: Some(stop),
            task: Some(task),
            _dir: dir,
        }
    }

    /// Runs first-run setup through the form and returns the session cookie.
    async fn owner_cookie(&self) -> String {
        let resp = form(
            self.addr,
            "/setup",
            &format!("name=owner&password={PASSWORD}"),
        )
        .await;
        assert_eq!(resp.status, 303, "{}", resp.body);
        assert_eq!(resp.header("location"), Some("/"));
        session_cookie(&resp)
    }

    async fn api(&self, cookie: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
        let text = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let resp = send(
            self.addr,
            method,
            &format!("/api/v1{path}"),
            &[
                ("Cookie", cookie),
                ("X-Requested-With", "aurora"),
                ("Content-Type", "application/json"),
            ],
            &text,
        )
        .await;
        let value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        (resp.status, value)
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("server stops")
                .unwrap()
                .unwrap();
        }
    }
}

/// A project WEB with a Task type and three issues in Backlog.
async fn seed(server: &Server, cookie: &str) -> Vec<i64> {
    let (status, _) = server
        .api(
            cookie,
            "POST",
            "/projects",
            json!({"key": "WEB", "name": "Website"}),
        )
        .await;
    assert_eq!(status, 201);
    let (status, _) = server
        .api(
            cookie,
            "POST",
            "/projects/WEB/issue-types",
            json!({"name": "Task"}),
        )
        .await;
    assert_eq!(status, 201);
    let mut numbers = Vec::new();
    for title in ["First", "Second", "Third"] {
        let (status, issue) = server
            .api(
                cookie,
                "POST",
                "/projects/WEB/issues",
                json!({"title": title}),
            )
            .await;
        assert_eq!(status, 201, "{issue}");
        numbers.push(issue["number"].as_i64().unwrap());
    }
    numbers
}

fn index_of(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in:\n{haystack}"))
}

#[tokio::test]
async fn pages_need_a_session_and_setup_comes_first() {
    let server = Server::start().await;

    // No owner yet: every page leads to setup.
    let resp = get(server.addr, "/", None).await;
    assert_eq!(resp.status, 303);
    assert_eq!(resp.header("location"), Some("/setup"));
    let resp = get(server.addr, "/p/WEB", None).await;
    assert_eq!(resp.header("location"), Some("/setup"));
    let resp = get(server.addr, "/setup", None).await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.contains("Create owner"));

    let cookie = server.owner_cookie().await;
    assert!(cookie.starts_with("aurora_session="));

    // Owner exists: setup is closed, pages need the cookie.
    let resp = get(server.addr, "/setup", None).await;
    assert_eq!(resp.header("location"), Some("/sign-in"));
    let resp = get(server.addr, "/p/WEB/12?x=1", None).await;
    assert_eq!(resp.status, 303);
    assert_eq!(
        resp.header("location"),
        Some("/sign-in?next=%2Fp%2FWEB%2F12%3Fx%3D1")
    );
    // A fetch from the script gets a 401, not a redirect.
    let resp = send(
        server.addr,
        "GET",
        "/p/WEB/board",
        &[("X-Requested-With", "aurora")],
        "",
    )
    .await;
    assert_eq!(resp.status, 401);
    // The stream is API-style: 401 JSON.
    let resp = get(server.addr, "/events/stream", None).await;
    assert_eq!(resp.status, 401);

    server.shutdown().await;
}

#[tokio::test]
async fn sign_in_form_sets_the_cookie_and_only_follows_local_next() {
    let server = Server::start().await;
    server.owner_cookie().await;

    let resp = get(server.addr, "/sign-in?next=%2Fp%2FWEB", None).await;
    assert_eq!(resp.status, 200);
    assert!(
        resp.body.contains(r#"name="next" value="/p/WEB""#),
        "{}",
        resp.body
    );
    assert!(resp.body.contains(r#"type="password""#));
    assert!(resp.body.contains(r#"<label for="name">"#));

    let resp = form(
        server.addr,
        "/sign-in",
        &format!("name=owner&password={PASSWORD}&next=%2Fp%2FWEB"),
    )
    .await;
    assert_eq!(resp.status, 303);
    assert_eq!(resp.header("location"), Some("/p/WEB"));
    let cookie = session_cookie(&resp);
    assert!(
        resp.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("set-cookie")
                && v.contains("HttpOnly")
                && v.contains("SameSite=Lax"))
    );

    // The cookie works for the API and the pages.
    let (status, me) = server.api(&cookie, "GET", "/auth/me", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(me["user"]["name"], "owner");

    for evil in [
        "https%3A%2F%2Fevil.example%2F",
        "%2F%2Fevil.example",
        "%2F%5Cevil.example",
        "evil",
    ] {
        let resp = form(
            server.addr,
            "/sign-in",
            &format!("name=owner&password={PASSWORD}&next={evil}"),
        )
        .await;
        assert_eq!(resp.status, 303);
        assert_eq!(resp.header("location"), Some("/"), "next={evil}");
    }

    let resp = form(
        server.addr,
        "/sign-in",
        "name=owner&password=wrong-password-here",
    )
    .await;
    assert_eq!(resp.status, 401);
    assert!(resp.body.contains("form-error"));
    assert!(resp.header("set-cookie").is_none());

    // A post from another site is refused.
    let resp = send(
        server.addr,
        "POST",
        "/sign-in",
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Origin", "https://evil.example"),
        ],
        &format!("name=owner&password={PASSWORD}"),
    )
    .await;
    assert_eq!(resp.status, 403);

    // Signed in already: the sign-in page goes on to the target.
    let resp = get(server.addr, "/sign-in?next=%2Fp%2FWEB", Some(&cookie)).await;
    assert_eq!(resp.header("location"), Some("/p/WEB"));

    server.shutdown().await;
}

#[tokio::test]
async fn board_shows_columns_and_issues_in_order() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let numbers = seed(&server, &cookie).await;

    // Move the third issue before the first: Third, First, Second.
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            &format!("/projects/WEB/issues/{}/move", numbers[2]),
            json!({"column_id": 1, "before": numbers[0]}),
        )
        .await;
    assert_eq!(status, 200);
    // And the second into "Done".
    let (_, columns) = server
        .api(&cookie, "GET", "/projects/WEB/columns", Value::Null)
        .await;
    let done = columns
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "Done")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            &format!("/projects/WEB/issues/{}/move", numbers[1]),
            json!({"column_id": done}),
        )
        .await;
    assert_eq!(status, 200);

    // One project: the home page goes straight to it; ?list=1 shows the list and the form.
    let resp = get(server.addr, "/", Some(&cookie)).await;
    assert_eq!(resp.status, 303);
    assert_eq!(resp.header("location"), Some("/p/WEB"));
    let resp = get(server.addr, "/?list=1", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.contains(r#"href="/p/WEB""#));
    assert!(resp.body.contains(r#"data-form="new-project""#));

    let resp = get(server.addr, "/p/WEB", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    let body = &resp.body;

    let names = ["Backlog", "Todo", "In Progress", "Done", "Canceled"];
    let positions: Vec<usize> = names
        .iter()
        .map(|n| index_of(body, &format!(">{n}</h2>")))
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "columns out of order"
    );
    assert!(body.contains(r#"data-state="started""#));

    let third = index_of(body, ">Third<");
    let first = index_of(body, ">First<");
    let done_column = index_of(body, ">Done</h2>");
    let second = index_of(body, ">Second<");
    assert!(third < first, "Third was moved before First");
    assert!(
        first < done_column && done_column < second,
        "Second is in Done"
    );
    assert!(body.contains(r#"<span class="ident">WEB-1</span>"#));
    assert!(body.contains(r#"href="/p/WEB/1""#));
    assert!(body.contains(r#"class="move-select""#));
    assert!(body.contains(r#"draggable="true""#));
    assert!(body.contains(r#"<meta name="viewport""#));
    assert!(body.contains("data-last-event="));

    // The live-refresh fragment is the board alone.
    let resp = get(server.addr, "/p/WEB/board", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    assert!(
        resp.body.starts_with(r#"<div class="board" id="board""#),
        "{}",
        resp.body
    );
    assert!(!resp.body.contains("<html"));

    // Unknown project and issue: 404 pages.
    assert_eq!(get(server.addr, "/p/NOPE", Some(&cookie)).await.status, 404);
    assert_eq!(
        get(server.addr, "/p/WEB/99", Some(&cookie)).await.status,
        404
    );

    server.shutdown().await;
}

#[tokio::test]
async fn user_content_is_escaped_and_pages_have_no_inline_script_or_style() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let (_, _) = server
        .api(
            &cookie,
            "POST",
            "/projects",
            json!({"key": "XSS", "name": "<b>bold</b> project"}),
        )
        .await;
    server
        .api(
            &cookie,
            "POST",
            "/projects/XSS/issue-types",
            json!({"name": "T<i>"}),
        )
        .await;
    let (status, label) = server
        .api(
            &cookie,
            "POST",
            "/projects/XSS/labels",
            json!({"name": "\"><img src=x onerror=alert(1)>"}),
        )
        .await;
    assert_eq!(status, 201);
    let (status, issue) = server
        .api(
            &cookie,
            "POST",
            "/projects/XSS/issues",
            json!({
                "title": "<script>alert(1)</script>",
                "description": "</textarea><script>alert(2)</script>\nline two",
                "labels": [label["id"]],
            }),
        )
        .await;
    assert_eq!(status, 201, "{issue}");

    for path in [
        "/p/XSS",
        "/p/XSS/1",
        "/p/XSS/board",
        "/p/XSS/1/activity",
        "/?list=1",
    ] {
        let resp = get(server.addr, path, Some(&cookie)).await;
        assert_eq!(resp.status, 200, "{path}");
        let body = &resp.body;
        assert!(
            !body.contains("<script>alert"),
            "{path}: raw script\n{body}"
        );
        assert!(!body.contains("<img"), "{path}: raw img\n{body}");
        assert!(!body.contains("<b>bold"), "{path}: raw b\n{body}");
        assert!(!body.contains("<i>"), "{path}: raw i\n{body}");
        for (i, _) in body.match_indices("<script") {
            assert!(
                body[i..].starts_with("<script src=\"/assets/app.js?v="),
                "{path}: inline script"
            );
        }
        assert!(!body.contains(" style="), "{path}: inline style");
        assert!(!body.contains("<style"), "{path}: style element");
        assert!(!body.contains(" onclick="), "{path}: inline handler");
    }
    let board = get(server.addr, "/p/XSS", Some(&cookie)).await;
    assert!(board.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(board.body.contains("&lt;b&gt;bold&lt;/b&gt; project"));
    assert!(
        board
            .body
            .contains("&quot;&gt;&lt;img src=x onerror=alert(1)&gt;")
    );
    let page = get(server.addr, "/p/XSS/1", Some(&cookie)).await;
    assert!(
        page.body
            .contains("&lt;/textarea&gt;&lt;script&gt;alert(2)&lt;/script&gt;")
    );
    assert_eq!(page.body.matches("<script").count(), 1);

    server.shutdown().await;
}

#[tokio::test]
async fn security_headers_are_on_pages_and_assets() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;

    for (path, cookie) in [
        ("/sign-in", None),
        ("/p/WEB", Some(cookie.as_str())),
        ("/assets/app.js", None),
    ] {
        let resp = get(server.addr, path, cookie).await;
        let csp = resp
            .header("content-security-policy")
            .unwrap_or_else(|| panic!("{path}: no CSP"));
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains("style-src 'self'"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("unsafe-inline"));
        assert_eq!(
            resp.header("x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(
            resp.header("referrer-policy"),
            Some("same-origin"),
            "{path}"
        );
    }
    let page = get(server.addr, "/p/WEB", Some(&cookie)).await;
    assert_eq!(page.header("cache-control"), Some("no-store"));

    server.shutdown().await;
}

#[tokio::test]
async fn assets_have_content_types_and_cache_rules() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;

    let page = get(server.addr, "/p/WEB", Some(&cookie)).await;
    let find = |attr: &str| -> String {
        let at = index_of(&page.body, attr) + attr.len();
        page.body[at..].split('"').next().unwrap().to_string()
    };
    let css = find(r#"rel="stylesheet" href=""#);
    let js = find(r#"<script src=""#);
    assert!(css.starts_with("/assets/app.css?v="));
    assert!(js.starts_with("/assets/app.js?v="));

    let resp = get(server.addr, &css, None).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(
        resp.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert!(resp.body.contains("--accent"));
    let etag = resp.header("etag").unwrap().to_string();

    let resp = get(server.addr, &js, None).await;
    assert_eq!(
        resp.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert!(resp.body.contains("EventSource"));

    // Without the current hash a request must revalidate; the etag answers 304.
    let resp = get(server.addr, "/assets/app.css", None).await;
    assert_eq!(resp.header("cache-control"), Some("no-cache"));
    let resp = send(
        server.addr,
        "GET",
        "/assets/app.css",
        &[("If-None-Match", &etag)],
        "",
    )
    .await;
    assert_eq!(resp.status, 304);

    let resp = get(server.addr, "/assets/icon.svg", None).await;
    assert_eq!(resp.header("content-type"), Some("image/svg+xml"));
    assert_eq!(get(server.addr, "/assets/nope.js", None).await.status, 404);

    server.shutdown().await;
}

#[tokio::test]
async fn issue_page_shows_fields_children_and_activity() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;
    let (_, label) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/labels",
            json!({"name": "urgent-ish", "color": "#aa0000"}),
        )
        .await;
    let (_, child) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues",
            json!({"title": "Child work", "parent": 1}),
        )
        .await;
    assert_eq!(child["number"], 4);
    let (status, _) = server
        .api(
            &cookie,
            "PATCH",
            "/projects/WEB/issues/1",
            json!({"title": "First, renamed", "priority": "high", "labels": [label["id"]], "description": "Some text\nwith a break"}),
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/1/move",
            json!({"column_id": 3}),
        )
        .await;
    assert_eq!(status, 200);

    let resp = get(server.addr, "/p/WEB/1", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    let body = &resp.body;
    assert!(body.contains(r#"value="First, renamed""#));
    assert!(body.contains("Some text\nwith a break</textarea>"));
    assert!(body.contains(r#"data-field="priority""#));
    assert!(
        body.contains(r#"<option value="high" selected>high</option>"#),
        "{body}"
    );
    assert!(body.contains(r#"<option value="3" selected>In Progress (started)</option>"#));
    assert!(body.contains(r#"data-field="assignee_id""#));
    assert!(body.contains(r#"data-field="labels""#));
    assert!(body.contains("urgent-ish"));
    assert!(body.contains("Child work"));
    assert!(body.contains(r#"href="/p/WEB/4""#));
    assert!(body.contains(r#"data-action="archive""#));
    assert!(body.contains(r#"data-issue-id=""#));
    assert!(body.contains(r#"data-issue-number="1""#));

    // Activity: created, updated (changed fields), moved; actor and time.
    assert!(body.contains("created the issue"));
    assert!(body.contains("updated the issue"));
    assert!(
        body.contains("title: First \u{2192} First, renamed"),
        "{body}"
    );
    assert!(body.contains("priority: none \u{2192} high"));
    assert!(body.contains("labels: none \u{2192} urgent-ish"));
    assert!(body.contains("description edited"));
    assert!(body.contains("moved the issue from Backlog to In Progress"));
    assert!(body.contains("<strong>owner</strong>"));
    assert!(body.contains(r#"<time class="when" datetime=""#));
    assert!(body.contains(" UTC</time>"));
    assert!(
        body.find("moved the issue").unwrap() < body.find("created the issue").unwrap(),
        "newest first"
    );

    // The child links back to its parent; archived issues offer "Restore".
    let resp = get(server.addr, "/p/WEB/4", Some(&cookie)).await;
    assert!(resp.body.contains("Parent: "));
    assert!(resp.body.contains(r#"href="/p/WEB/1""#));
    server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/4/archive",
            Value::Null,
        )
        .await;
    let resp = get(server.addr, "/p/WEB/4", Some(&cookie)).await;
    assert!(resp.body.contains(r#"data-action="unarchive""#));
    assert!(resp.body.contains("archived the issue"));

    let resp = get(server.addr, "/p/WEB/1/activity", Some(&cookie)).await;
    assert!(
        resp.body
            .starts_with(r#"<section class="activity" id="activity""#)
    );

    server.shutdown().await;
}

// ---- the event stream ----

struct Sse {
    stream: TcpStream,
    seen: String,
}

impl Sse {
    async fn open(addr: SocketAddr, path: &str, cookie: &str, last_event_id: Option<i64>) -> Sse {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nAccept: text/event-stream\r\n"
        );
        if let Some(id) = last_event_id {
            request.push_str(&format!("Last-Event-ID: {id}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut sse = Sse {
            stream,
            seen: String::new(),
        };
        sse.read_until("\r\n\r\n").await;
        assert!(sse.seen.starts_with("HTTP/1.1 200"), "{}", sse.seen);
        assert!(
            sse.seen
                .to_ascii_lowercase()
                .contains("content-type: text/event-stream")
        );
        // Keep what arrived with the headers: replayed events can be in the same read.
        let body_at = index_of(&sse.seen, "\r\n\r\n") + 4;
        sse.seen = sse.seen[body_at..].to_string();
        sse
    }

    /// Reads until `needle` shows up in what has arrived; panics after 5 s.
    async fn read_until(&mut self, needle: &str) {
        let wait = async {
            while !self.seen.contains(needle) {
                let mut buf = [0_u8; 4096];
                let n = self.stream.read(&mut buf).await.unwrap();
                assert!(
                    n > 0,
                    "stream closed while waiting for {needle:?}; got {:?}",
                    self.seen
                );
                self.seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; got {:?}", self.seen));
    }

    fn events(&self) -> Vec<Value> {
        self.seen
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect()
    }
}

#[tokio::test]
async fn stream_delivers_new_events_and_resumes_after_last_event_id() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let numbers = seed(&server, &cookie).await;
    let (_, latest) = server
        .api(&cookie, "GET", "/events?limit=500", Value::Null)
        .await;
    let before = latest["events"].as_array().unwrap().last().unwrap()["id"]
        .as_i64()
        .unwrap();

    // Opened with no id: starts after the newest event, so only new ones arrive.
    let mut sse = Sse::open(server.addr, "/events/stream?project=WEB", &cookie, None).await;
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            &format!("/projects/WEB/issues/{}/move", numbers[0]),
            json!({"column_id": 3}),
        )
        .await;
    assert_eq!(status, 200);
    sse.read_until("issue.moved").await;
    let events = sse.events();
    assert_eq!(events.len(), 1, "{:?}", sse.seen);
    assert_eq!(events[0]["kind"], "issue.moved");
    assert_eq!(events[0]["project"], "WEB");
    assert_eq!(events[0]["entity_kind"], "issue");
    let moved_id = events[0]["id"].as_i64().unwrap();
    assert!(moved_id > before);
    assert!(sse.seen.contains(&format!("id: {moved_id}")));
    drop(sse);

    // Events from other projects are filtered out; the next WEB event still arrives.
    server
        .api(
            &cookie,
            "POST",
            "/projects",
            json!({"key": "OTH", "name": "Other"}),
        )
        .await;
    let (status, _) = server
        .api(
            &cookie,
            "PATCH",
            "/projects/WEB/issues/2",
            json!({"title": "Second, renamed"}),
        )
        .await;
    assert_eq!(status, 200);

    // Reconnect with Last-Event-ID: everything after it is replayed, in order, once.
    let mut sse = Sse::open(
        server.addr,
        "/events/stream?project=WEB",
        &cookie,
        Some(before),
    )
    .await;
    sse.read_until("issue.updated").await;
    let kinds: Vec<String> = sse
        .events()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["issue.moved", "issue.updated"]);
    let ids: Vec<i64> = sse
        .events()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect();
    assert!(ids[0] < ids[1]);

    // The header wins over ?after=, and ?after= alone works for the first connect.
    let mut sse = Sse::open(
        server.addr,
        &format!("/events/stream?project=WEB&after={}", ids[0]),
        &cookie,
        None,
    )
    .await;
    sse.read_until("issue.updated").await;
    assert_eq!(sse.events().len(), 1);

    assert_eq!(
        send(
            server.addr,
            "GET",
            "/events/stream?project=NOPE",
            &[("Cookie", &cookie)],
            ""
        )
        .await
        .status,
        404
    );
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_open_streams_without_waiting_for_the_drain() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let mut sse = Sse::open(server.addr, "/events/stream", &cookie, None).await;

    let started = std::time::Instant::now();
    let mut server = server;
    let _ = server.stop.take().unwrap().send(());
    let task = server.task.take().unwrap();
    // The drain limit is 10 s; a stream that ends on shutdown finishes far sooner.
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .expect("shutdown must not wait for the open stream")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));

    // The client sees the stream end (EOF), not a hang.
    let mut rest = Vec::new();
    let eof = tokio::time::timeout(Duration::from_secs(2), sse.stream.read_to_end(&mut rest)).await;
    assert!(eof.is_ok(), "the stream did not end");
}
