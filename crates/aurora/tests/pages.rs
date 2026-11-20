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

/// The board markup of the card with this identifier.
fn card_of<'a>(board: &'a str, ident: &str) -> &'a str {
    board
        .split("<li class=\"card")
        .find(|chunk| chunk.contains(&format!(">{ident}</span>")))
        .unwrap_or_else(|| panic!("no card for {ident}"))
}

#[tokio::test]
async fn board_marks_blocked_cards_until_their_blockers_are_done() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await; // WEB-1, WEB-2, WEB-3 in Backlog
    let (_, columns) = server
        .api(&cookie, "GET", "/projects/WEB/columns", Value::Null)
        .await;
    let done = columns[3]["id"].as_i64().unwrap();
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/1/links",
            json!({"kind": "blocks", "to": 2}),
        )
        .await;
    assert_eq!(status, 201);

    for path in ["/p/WEB", "/p/WEB/board"] {
        let body = get(server.addr, path, Some(&cookie)).await.body;
        let blocked = card_of(&body, "WEB-2");
        assert!(blocked.contains("chip-blocked"), "{path}: {blocked}");
        assert!(blocked.starts_with(" blocked\""), "{blocked}");
        assert!(blocked.contains(r#"title="Blocked by WEB-1""#), "{blocked}");
        assert!(blocked.contains(">Blocked<span class=\"sr-only\"> by WEB-1</span>"));
        // The blocker and the unrelated issue are not marked.
        for free in ["WEB-1", "WEB-3"] {
            let card = card_of(&body, free);
            assert!(!card.contains("chip-blocked"), "{path} {free}: {card}");
            assert!(!card.starts_with(" blocked"), "{free}");
        }
    }

    // The blocker reaches a completed column: nothing is blocked any more.
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/1/move",
            json!({"column_id": done}),
        )
        .await;
    assert_eq!(status, 200);
    let body = get(server.addr, "/p/WEB/board", Some(&cookie)).await.body;
    assert!(!body.contains("chip-blocked"), "{body}");

    // A link event is an event of the project, so the board's stream sees it.
    let mut sse = Sse::open(server.addr, "/events/stream?project=WEB", &cookie, None).await;
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/3/links",
            json!({"kind": "blocks", "to": 2}),
        )
        .await;
    assert_eq!(status, 201);
    sse.read_until("link.added").await;
    let events = sse.events();
    assert_eq!(events[0]["kind"], "link.added");
    assert_eq!(events[0]["project"], "WEB");
    drop(sse);

    server.shutdown().await;
}

#[tokio::test]
async fn issue_page_lists_links_with_a_form_and_escapes_titles() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;
    let (status, nasty) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues",
            json!({"title": "<b>bold</b> & \"quoted\""}),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(nasty["number"], 4);
    for (from, to, kind) in [(1, 2, "blocks"), (4, 2, "blocks"), (2, 3, "relates")] {
        let (status, body) = server
            .api(
                &cookie,
                "POST",
                &format!("/projects/WEB/issues/{from}/links"),
                json!({"kind": kind, "to": to}),
            )
            .await;
        assert_eq!(status, 201, "{body}");
    }

    let page = get(server.addr, "/p/WEB/2", Some(&cookie)).await;
    assert_eq!(page.status, 200);
    let body = &page.body;
    assert!(
        body.contains(r#"<h2 id="links-heading">Links</h2>"#),
        "{body}"
    );
    assert!(body.contains("<h3>Blocked by</h3>"));
    assert!(body.contains("<h3>Related</h3>"));
    assert!(!body.contains("<h3>Blocks</h3>"), "WEB-2 blocks nothing");
    assert!(body.contains(r#"href="/p/WEB/1""#));
    assert!(body.contains(r#"href="/p/WEB/3""#));
    assert!(body.contains(r#"data-form="add-link""#));
    assert!(body.contains(r#"<option value="blocked_by">Is blocked by</option>"#));
    assert!(body.contains(r#"name="to" type="number""#));
    assert_eq!(body.matches(r#"data-action="remove-link""#).count(), 3);
    assert!(body.contains("data-link-id="));
    assert!(body.contains("chip chip-blocked"), "header marker");
    assert!(body.contains("Blocked</strong> until its blockers are done"));
    // Titles are escaped in the links, in the form of the blocker's own page and in activity.
    assert!(body.contains("&lt;b&gt;bold&lt;/b&gt; &amp; &quot;quoted&quot;"));
    assert!(!body.contains("<b>bold"));
    assert_eq!(body.matches("<script").count(), 1, "only the app script");
    assert!(!body.contains(" style="));

    // An unblocked, unlinked issue shows the empty state and still has the form.
    let (_, _) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues",
            json!({"title": "lonely"}),
        )
        .await;
    let lonely = get(server.addr, "/p/WEB/5", Some(&cookie)).await.body;
    assert!(lonely.contains("No links."));
    assert!(lonely.contains(r#"data-form="add-link""#));
    assert!(!lonely.contains("chip-blocked"));

    // The blocker's page says what it blocks, and its activity tells about the link.
    let one = get(server.addr, "/p/WEB/1", Some(&cookie)).await.body;
    assert!(one.contains("<h3>Blocks</h3>"));
    assert!(one.contains("added the link WEB-1 blocks WEB-2"), "{one}");
    assert!(!one.contains("chip-blocked"));
    let activity = get(server.addr, "/p/WEB/1/activity", Some(&cookie))
        .await
        .body;
    assert!(activity.contains("added the link WEB-1 blocks WEB-2"));

    // The script posts to the API and reports errors inline.
    let js = get(server.addr, "/assets/app.js", None).await.body;
    assert!(js.contains("'add-link'") && js.contains("remove-link"));

    server.shutdown().await;
}

/// Posts a comment as the owner (cookie) and returns its id.
async fn comment(server: &Server, cookie: &str, body: &str, parent: Option<i64>) -> i64 {
    let (status, made) = server
        .api(
            cookie,
            "POST",
            "/projects/WEB/issues/1/comments",
            json!({"body": body, "parent": parent}),
        )
        .await;
    assert_eq!(status, 201, "{made}");
    made["id"].as_i64().unwrap()
}

#[tokio::test]
async fn issue_page_shows_threaded_comments_with_mentions_and_forms() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;
    let (status, agent) = server
        .api(&cookie, "POST", "/users", json!({"name": "robot"}))
        .await;
    assert_eq!(status, 201);
    // An agent whose name is markup: it must be shown as text.
    let (status, odd) = server
        .api(&cookie, "POST", "/users", json!({"name": "<i>odd</i>"}))
        .await;
    assert_eq!(status, 201);
    let (_, key) = server
        .api(
            &cookie,
            "POST",
            "/keys",
            json!({"name": "k", "user_id": odd["id"]}),
        )
        .await;
    let bearer = format!("Bearer {}", key["key"].as_str().unwrap());

    let empty = get(server.addr, "/p/WEB/1", Some(&cookie)).await.body;
    assert!(empty.contains(r#"id="comments""#) && empty.contains("No comments yet."));
    assert!(empty.contains(r#"data-form="add-comment""#));
    assert!(!empty.contains(r#"data-form="add-reply""#));

    let top = comment(
        &server,
        &cookie,
        "Hi @robot, <script>alert(1)</script>\nsecond line @nobody",
        None,
    )
    .await;
    let reply = comment(&server, &cookie, "me too @ROBOT", Some(top)).await;
    let resp = send(
        server.addr,
        "POST",
        "/api/v1/projects/WEB/issues/1/comments",
        &[
            ("Authorization", bearer.as_str()),
            ("Content-Type", "application/json"),
        ],
        &json!({"body": "from the odd one", "parent": top}).to_string(),
    )
    .await;
    assert_eq!(resp.status, 201, "{}", resp.body);
    let gone = comment(&server, &cookie, "to be removed", None).await;
    let (status, _) = server
        .api(
            &cookie,
            "DELETE",
            &format!("/projects/WEB/comments/{gone}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = server
        .api(
            &cookie,
            "PATCH",
            &format!("/projects/WEB/comments/{reply}"),
            json!({"body": "me too @ROBOT!"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(agent["name"], "robot");

    let page = get(server.addr, "/p/WEB/1", Some(&cookie)).await;
    assert_eq!(page.status, 200);
    let body = &page.body;
    // Escaped body and author names; mentions wrapped; nothing raw.
    assert!(
        body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{body}"
    );
    assert!(!body.contains("<script>alert(1)"));
    assert!(body.contains("&lt;i&gt;odd&lt;/i&gt;") && !body.contains("<i>odd</i>"));
    assert!(body.contains(r#"<span class="mention">@robot</span>"#));
    assert!(body.contains(r#"<span class="mention">@ROBOT</span>"#));
    assert!(!body.contains(r#"<span class="mention">@nobody"#));
    assert!(body.contains("second line @nobody"));
    assert!(body.contains("Hi <span"), "{body}");
    assert!(body.contains(r#"class="comment-text""#));
    assert!(body.contains(r#"class="chip">agent"#));

    // Threaded: the top comment, then its replies inside the reply list, in order;
    // the placeholder shows for no one else and the lone deleted comment is gone.
    let at = |needle: &str| index_of(body, needle);
    assert!(at(r#"id="comments""#) < at("Hi <span"));
    assert!(at("Hi <span") < at(r#"class="reply-list""#));
    assert!(at(r#"class="reply-list""#) < at("me too "));
    assert!(at("me too ") < at("from the odd one"));
    assert!(!body.contains("to be removed"));
    assert!(!body.contains("This comment was deleted."));
    assert!(
        body.contains(r#"class="comment-edited""#),
        "the edited reply is marked"
    );
    assert_eq!(
        body.matches(r#"data-action="show-reply""#).count(),
        1,
        "top level only"
    );

    // Forms: add, reply (per top-level comment), edit; delete and edit buttons for the owner.
    assert!(body.contains(r#"data-form="add-comment""#));
    assert!(body.contains(&format!(r#"data-form="add-reply" data-parent-id="{top}""#)));
    assert!(body.contains(&format!(
        r#"data-form="edit-comment" data-comment-id="{reply}""#
    )));
    assert!(body.contains(r#"data-action="delete-comment""#));
    assert!(body.matches("data-action=\"show-edit\"").count() >= 3);
    assert!(body.contains(r#"<time class="when""#) && body.contains("data-ts="));
    assert!(body.contains("maxlength=\"20000\""));

    // The edit form carries the stored text (escaped), so editing starts from it.
    assert!(body.contains("me too @ROBOT!</textarea>"));

    // A deleted top-level comment with replies stays as a placeholder.
    let (status, _) = server
        .api(
            &cookie,
            "DELETE",
            &format!("/projects/WEB/comments/{top}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, 204);
    let page = get(server.addr, "/p/WEB/1", Some(&cookie)).await.body;
    assert!(page.contains("This comment was deleted."));
    assert!(!page.contains("alert(1)"));
    assert!(page.contains("from the odd one"));

    // Activity says what happened and hides the mentions.
    assert!(page.contains("commented"));
    assert!(page.contains("replied to a comment"));
    assert!(page.contains("edited a comment"));
    assert!(page.contains("deleted a comment"));
    assert!(
        !page.contains("mention.created"),
        "mentions are not activity"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn comments_fragment_needs_a_session_and_renders_the_list_alone() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;
    comment(&server, &cookie, "fragment text", None).await;

    let resp = get(server.addr, "/p/WEB/1/comments", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.starts_with("<section"), "{}", resp.body);
    assert!(resp.body.contains(r#"id="comments""#));
    assert!(resp.body.contains("fragment text"));
    assert!(!resp.body.contains("<html"));
    assert_eq!(resp.header("cache-control"), Some("no-store"));

    // The script refreshes with the CSRF header: no cookie is a plain 401, no redirect.
    let resp = send(
        server.addr,
        "GET",
        "/p/WEB/1/comments",
        &[("X-Requested-With", "aurora")],
        "",
    )
    .await;
    assert_eq!(resp.status, 401);
    let resp = get(server.addr, "/p/WEB/1/comments", None).await;
    assert_eq!(resp.status, 303);
    assert_eq!(
        get(server.addr, "/p/WEB/99/comments", Some(&cookie))
            .await
            .status,
        404
    );
    assert_eq!(
        get(server.addr, "/p/NOPE/1/comments", Some(&cookie))
            .await
            .status,
        404
    );

    // The script knows the forms and refreshes the fragment on comment events.
    let js = get(server.addr, "/assets/app.js", None).await.body;
    for needle in [
        "'add-comment'",
        "'add-reply'",
        "'edit-comment'",
        "/comments'",
        "comment.",
    ] {
        assert!(js.contains(needle), "app.js lacks {needle}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn stream_reports_comment_events_for_the_issue() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    seed(&server, &cookie).await;
    let mut sse = Sse::open(server.addr, "/events/stream?project=WEB", &cookie, None).await;
    let id = comment(&server, &cookie, "live", None).await;
    sse.read_until("comment.created").await;
    let (status, _) = server
        .api(
            &cookie,
            "PATCH",
            &format!("/projects/WEB/comments/{id}"),
            json!({"body": "live, edited"}),
        )
        .await;
    assert_eq!(status, 200);
    sse.read_until("comment.edited").await;
    let events = sse.events();
    assert_eq!(events[0]["kind"], "comment.created");
    assert_eq!(
        events[0]["entity_kind"], "issue",
        "the page reacts to its own issue's events"
    );
    assert_eq!(events[0]["entity_id"], 1);
    assert!(
        !sse.seen.contains("live, edited"),
        "the stream carries no comment text"
    );
    server.shutdown().await;
}

// ---- initiatives ----

/// Creates an initiative in WEB and returns its id.
async fn initiative(server: &Server, cookie: &str, body: Value) -> i64 {
    let (status, made) = server
        .api(cookie, "POST", "/projects/WEB/initiatives", body)
        .await;
    assert_eq!(status, 201, "{made}");
    made["id"].as_i64().unwrap()
}

async fn assign(server: &Server, cookie: &str, number: i64, initiative: i64) {
    let (status, issue) = server
        .api(
            cookie,
            "PATCH",
            &format!("/projects/WEB/issues/{number}"),
            json!({ "initiative": initiative }),
        )
        .await;
    assert_eq!(status, 200, "{issue}");
}

async fn done_column(server: &Server, cookie: &str) -> i64 {
    let (_, columns) = server
        .api(cookie, "GET", "/projects/WEB/columns", Value::Null)
        .await;
    columns
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["state_type"] == "completed")
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn initiatives_page_lists_progress_health_and_a_create_form() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let numbers = seed(&server, &cookie).await;

    // Nothing yet: an empty state, the form and the nav link are there.
    let resp = get(server.addr, "/p/WEB/initiatives", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.contains("No initiatives yet"));
    assert!(resp.body.contains(r#"data-page="initiatives""#));
    assert!(resp.body.contains(r#"data-form="new-initiative""#));
    assert!(resp.body.contains(r#"name="target_date" type="date""#));
    assert!(resp.body.contains(r#"href="/p/WEB/initiatives""#));
    let board = get(server.addr, "/p/WEB", Some(&cookie)).await;
    assert!(
        board
            .body
            .contains(r#"<a href="/p/WEB/initiatives">Initiatives</a>"#),
        "the project header links to the initiatives"
    );

    let launch = initiative(
        &server,
        &cookie,
        json!({"name": "Launch", "status": "active", "target_date": "2999-01-01"}),
    )
    .await;
    let evil = initiative(&server, &cookie, json!({"name": "<b>Evil</b> \"one\""})).await;
    assign(&server, &cookie, numbers[0], launch).await;
    assign(&server, &cookie, numbers[1], launch).await;
    let done = done_column(&server, &cookie).await;
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            &format!("/projects/WEB/issues/{}/move", numbers[0]),
            json!({ "column_id": done }),
        )
        .await;
    assert_eq!(status, 200);

    let resp = get(server.addr, "/p/WEB/initiatives", Some(&cookie)).await;
    assert_eq!(resp.status, 200);
    let body = &resp.body;
    assert_eq!(resp.header("cache-control"), Some("no-store"));
    assert!(body.contains(&format!(r#"href="/p/WEB/initiatives/{launch}""#)));
    assert!(body.contains("1 of 2 done (50%)"), "{body}");
    assert!(body.contains(r#"<progress class="progress-bar" max="2" value="1""#));
    assert!(body.contains("0 of 0 done (0%)"));
    assert!(body.contains("Target 2999-01-01"));
    assert!(body.contains("No target date"));
    // Health is in words, not only a colour.
    assert!(body.contains("Health: On track"), "{body}");
    assert!(body.contains("Health: No health signal"));
    assert!(body.contains(r#"data-status="active""#));
    assert!(
        index_of(body, "Launch") < index_of(body, "Evil"),
        "creation order"
    );

    // Escaping, and no inline script or style.
    assert!(body.contains("&lt;b&gt;Evil&lt;/b&gt; &quot;one&quot;"));
    assert!(!body.contains("<b>Evil"));
    assert!(!body.contains(" style="));
    assert!(!body.contains("<style"));
    assert_eq!(body.matches("<script").count(), 1);
    let _ = evil;

    // The list fragment on its own, behind the session.
    let frag = get(server.addr, "/p/WEB/initiatives/list", Some(&cookie)).await;
    assert_eq!(frag.status, 200);
    assert!(frag.body.starts_with("<section"), "{}", frag.body);
    assert!(frag.body.contains(r#"id="initiative-list""#));
    assert!(frag.body.contains("1 of 2 done (50%)"));
    assert!(!frag.body.contains("<html"));
    assert_eq!(
        get(server.addr, "/p/WEB/initiatives/list", None)
            .await
            .status,
        303
    );
    let resp = send(
        server.addr,
        "GET",
        "/p/WEB/initiatives/list",
        &[("X-Requested-With", "aurora")],
        "",
    )
    .await;
    assert_eq!(resp.status, 401);
    assert_eq!(
        get(server.addr, "/p/WEB/initiatives", None).await.status,
        303
    );
    assert_eq!(
        get(server.addr, "/p/NOPE/initiatives", Some(&cookie))
            .await
            .status,
        404
    );
    server.shutdown().await;
}

#[tokio::test]
async fn initiative_page_has_editable_fields_progress_and_its_issues() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let numbers = seed(&server, &cookie).await;
    let id = initiative(
        &server,
        &cookie,
        json!({
            "name": "Ship <it>",
            "description": "</textarea><script>alert(2)</script>\nline two",
            "status": "paused",
            "target_date": "2031-02-03"
        }),
    )
    .await;
    assign(&server, &cookie, numbers[0], id).await;
    assign(&server, &cookie, numbers[2], id).await;
    assign(&server, &cookie, numbers[1], id).await;
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/2/archive",
            Value::Null,
        )
        .await;
    assert_eq!(status, 200);
    let done = done_column(&server, &cookie).await;
    server
        .api(
            &cookie,
            "POST",
            "/projects/WEB/issues/3/move",
            json!({ "column_id": done }),
        )
        .await;

    let resp = get(
        server.addr,
        &format!("/p/WEB/initiatives/{id}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(resp.status, 200);
    let body = &resp.body;
    assert!(body.contains(r#"data-page="initiative""#));
    assert!(body.contains(&format!(r#"data-initiative-id="{id}""#)));
    assert!(body.contains(r#"data-field="name""#));
    assert!(body.contains(r#"data-field="description""#));
    assert!(body.contains(r#"data-field="status""#));
    assert!(body.contains(r#"data-field="target_date""#));
    assert!(body.contains(r#"type="date" value="2031-02-03""#));
    assert!(
        body.contains(r#"<option value="paused" selected>paused</option>"#),
        "{body}"
    );
    assert!(body.contains(r#"<option value="active">active</option>"#));
    assert!(body.contains(r#"data-action="delete-initiative""#));
    assert!(body.contains(r#"data-action="save-description""#));

    // Progress counts the two live issues (one done); the archived one is out.
    assert!(body.contains("1 of 2 done (50%)"), "{body}");
    assert!(body.contains("Completed 1, canceled 0, in progress 0, not started 1."));
    assert!(body.contains(r#"href="/p/WEB/1""#));
    assert!(body.contains(r#"href="/p/WEB/3""#));
    assert!(
        !body.contains(r#"href="/p/WEB/2""#),
        "archived issues are not listed"
    );
    assert!(body.contains(r#"data-state="completed""#));
    assert!(body.contains(r#"href="/p/WEB/initiatives""#));

    // Escaping.
    assert!(body.contains("Ship &lt;it&gt;"));
    assert!(!body.contains("Ship <it>"));
    assert!(body.contains("&lt;/textarea&gt;&lt;script&gt;alert(2)&lt;/script&gt;"));
    assert_eq!(body.matches("<script").count(), 1);
    assert!(!body.contains(" style="));

    // The progress fragment on its own.
    let frag = get(
        server.addr,
        &format!("/p/WEB/initiatives/{id}/progress"),
        Some(&cookie),
    )
    .await;
    assert_eq!(frag.status, 200);
    assert!(frag.body.starts_with("<section"), "{}", frag.body);
    assert!(frag.body.contains(r#"id="initiative-progress""#));
    assert!(frag.body.contains("1 of 2 done (50%)"));
    assert!(!frag.body.contains("<html"));
    assert!(!frag.body.contains(r#"data-field="name""#));
    assert_eq!(
        get(
            server.addr,
            &format!("/p/WEB/initiatives/{id}/progress"),
            None
        )
        .await
        .status,
        303
    );

    // An empty initiative says so and offers no broken bar.
    let empty = initiative(&server, &cookie, json!({"name": "Empty"})).await;
    let resp = get(
        server.addr,
        &format!("/p/WEB/initiatives/{empty}"),
        Some(&cookie),
    )
    .await;
    assert!(resp.body.contains("0 of 0 done (0%)"));
    assert!(resp.body.contains("No issues yet"));
    assert!(resp.body.contains(r#"max="1" value="0""#));

    // Unknown ids, and ids of other projects, are 404 pages.
    for path in [
        format!("/p/WEB/initiatives/{}", id + 100),
        format!("/p/NOPE/initiatives/{id}"),
        format!("/p/WEB/initiatives/{}/progress", id + 100),
    ] {
        assert_eq!(
            get(server.addr, &path, Some(&cookie)).await.status,
            404,
            "{path}"
        );
    }
    let (status, _) = server
        .api(
            &cookie,
            "POST",
            "/projects",
            json!({"key": "OTH", "name": "Other"}),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(
        get(
            server.addr,
            &format!("/p/OTH/initiatives/{id}"),
            Some(&cookie)
        )
        .await
        .status,
        404
    );
    server.shutdown().await;
}

#[tokio::test]
async fn issue_page_picks_the_initiative_and_the_board_and_activity_show_it() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let numbers = seed(&server, &cookie).await;
    let id = initiative(&server, &cookie, json!({"name": "Launch <1>"})).await;
    initiative(&server, &cookie, json!({"name": "Other one"})).await;

    // Unassigned: the select offers none and both initiatives, none selected.
    let resp = get(server.addr, "/p/WEB/1", Some(&cookie)).await;
    let body = &resp.body;
    assert!(body.contains(r#"id="f-initiative""#));
    assert!(body.contains(r#"data-field="initiative""#));
    assert!(
        body.contains(r#"<option value="" selected>None</option>"#),
        "{body}"
    );
    assert!(body.contains(&format!(
        r#"<option value="{id}">Launch &lt;1&gt; (planned)</option>"#
    )));
    assert!(body.contains("Other one (planned)"));

    assign(&server, &cookie, numbers[0], id).await;
    let resp = get(server.addr, "/p/WEB/1", Some(&cookie)).await;
    assert!(
        resp.body.contains(&format!(
            r#"<option value="{id}" selected>Launch &lt;1&gt; (planned)</option>"#
        )),
        "{}",
        resp.body
    );
    assert!(resp.body.contains(r#"<option value="">None</option>"#));
    // The activity names the initiative.
    assert!(resp.body.contains("initiative: none"), "{}", resp.body);
    assert!(resp.body.contains("Launch &lt;1&gt;"));
    let frag = get(server.addr, "/p/WEB/1/activity", Some(&cookie)).await;
    assert!(frag.body.contains("initiative: none"));

    // The board card carries a chip.
    let board = get(server.addr, "/p/WEB/board", Some(&cookie)).await;
    assert!(board.body.contains("chip-initiative"), "{}", board.body);
    assert!(board.body.contains("Launch &lt;1&gt;"));
    assert_eq!(board.body.matches("chip-initiative").count(), 1);

    // The script saves the field with PATCH and knows the initiative pages.
    let js = get(server.addr, "/assets/app.js", None).await.body;
    for needle in [
        "field === 'initiative'",
        "'new-initiative'",
        "delete-initiative",
        "/initiatives",
        "/list'",
        "/progress'",
        "initiativeId",
        "initiative.deleted",
        "page === 'initiatives'",
    ] {
        assert!(js.contains(needle), "app.js lacks {needle}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn stream_reports_initiative_and_issue_events() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let numbers = seed(&server, &cookie).await;
    let mut sse = Sse::open(server.addr, "/events/stream?project=WEB", &cookie, None).await;

    let id = initiative(&server, &cookie, json!({"name": "Live"})).await;
    sse.read_until("initiative.created").await;
    assign(&server, &cookie, numbers[0], id).await;
    sse.read_until("issue.updated").await;
    let (status, _) = server
        .api(
            &cookie,
            "PATCH",
            &format!("/projects/WEB/initiatives/{id}"),
            json!({"status": "active"}),
        )
        .await;
    assert_eq!(status, 200);
    sse.read_until("initiative.updated").await;
    let (status, _) = server
        .api(
            &cookie,
            "DELETE",
            &format!("/projects/WEB/initiatives/{id}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, 204);
    sse.read_until("initiative.deleted").await;

    let events = sse.events();
    let created = events
        .iter()
        .find(|e| e["kind"] == "initiative.created")
        .unwrap();
    // The page reacts to events whose entity is its initiative, or an issue.
    assert_eq!(created["entity_kind"], "initiative");
    assert_eq!(created["entity_id"], id);
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "initiative.created",
            "issue.updated",
            "initiative.updated",
            "issue.updated",
            "initiative.deleted"
        ]
    );
    assert_eq!(events[1]["entity_kind"], "issue");
    assert!(!sse.seen.contains("Live"), "the stream carries no names");
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
