//! `GET /api/v1/search` over HTTP: results, filters, paging, errors and scopes.

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

    async fn search(&self, query: &str) -> (u16, Value) {
        self.call("GET", &format!("/search?{query}"), None).await
    }
}

fn enc(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn names(found: &Value) -> Vec<String> {
    found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| match h["kind"].as_str().unwrap() {
            "issue" => h["issue"]["identifier"].as_str().unwrap().to_string(),
            "comment" => format!(
                "{}#c{}",
                h["issue"]["identifier"].as_str().unwrap(),
                h["comment_id"]
            ),
            _ => format!(
                "{}:doc{}",
                h["project"].as_str().unwrap(),
                h["document"]["id"]
            ),
        })
        .collect()
}

#[tokio::test]
async fn search_over_http() {
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
    }
    let make = |key: &'static str, title: &'static str, description: &'static str| {
        let api = &api;
        async move {
            let (status, issue) = api
                .call(
                    "POST",
                    &format!("/projects/{key}/issues"),
                    Some(json!({"title": title, "description": description})),
                )
                .await;
            assert_eq!(status, 201, "{issue}");
            issue["number"].as_i64().unwrap()
        }
    };
    let flicker = make("TRK", "Compositor flicker", "The screen flashes on resize").await;
    make("TRK", "Login page", "Password field loses focus").await;
    make("OTH", "Flicker elsewhere", "").await;
    let (status, comment) = api
        .call(
            "POST",
            &format!("/projects/TRK/issues/{flicker}/comments"),
            Some(json!({"body": "Reproduced with the <b>nightly</b> build"})),
        )
        .await;
    assert_eq!(status, 201);
    let comment_id = comment["id"].as_i64().unwrap();
    let (status, document) = api
        .call(
            "POST",
            "/projects/TRK/documents",
            Some(json!({"title": "Rendering notes", "body": "Why the compositor flickers"})),
        )
        .await;
    assert_eq!(status, 201);
    let document_id = document["id"].as_i64().unwrap();
    let (_, read_key) = api
        .call(
            "POST",
            "/keys",
            Some(json!({"name": "ro", "scope": "read"})),
        )
        .await;
    let read_key = read_key["key"].as_str().unwrap().to_string();

    // Authentication; a read key may search because it only reads.
    let (status, _) = api.call_as(None, "GET", "/search?q=flicker", None).await;
    assert_eq!(status, 401);
    let (status, found) = api
        .call_as(Some(&read_key), "GET", "/search?q=flicker", None)
        .await;
    assert_eq!(status, 200, "{found}");

    // Shape of the answer: issue, comment and document hits.
    let (status, found) = api.search("q=flicker").await;
    assert_eq!(status, 200);
    assert_eq!(found["query"], "flicker");
    assert_eq!(found["total"], 3, "{found}");
    assert_eq!(found["total_capped"], false);
    assert_eq!(found["limit"], 20);
    assert_eq!(found["offset"], 0);
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    let issue_hit = hits
        .iter()
        .find(|h| h["kind"] == "issue" && h["project"] == "TRK")
        .unwrap();
    assert_eq!(issue_hit["issue"]["identifier"], format!("TRK-{flicker}"));
    assert_eq!(issue_hit["issue"]["title"], "Compositor flicker");
    assert_eq!(issue_hit["archived"], false);
    assert_eq!(issue_hit["exact"], false);
    assert!(issue_hit["rank"].is_number());
    assert!(issue_hit.get("comment_id").is_none());
    assert!(issue_hit.get("document").is_none());
    let title_matches: Vec<&Value> = issue_hit["title"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["matched"] == true)
        .collect();
    assert_eq!(title_matches.len(), 1);
    assert_eq!(title_matches[0]["text"], "flicker");

    let (_, comments) = api.search("q=nightly&kind=comment").await;
    let comment_hit = &comments["hits"][0];
    assert_eq!(comment_hit["kind"], "comment");
    assert_eq!(comment_hit["comment_id"], comment_id);
    assert_eq!(comment_hit["issue"]["number"], flicker);
    // The text comes back as data: markup in it stays text.
    let snippet: String = comment_hit["snippet"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["text"].as_str().unwrap())
        .collect();
    assert!(snippet.contains("<b>"), "{snippet}");

    let (_, docs) = api.search("q=flickers&kind=document").await;
    assert_eq!(docs["hits"][0]["kind"], "document");
    assert_eq!(docs["hits"][0]["document"]["id"], document_id);
    assert_eq!(docs["hits"][0]["document"]["title"], "Rendering notes");

    // Filters.
    let (_, trk) = api.search("q=flicker&project=TRK").await;
    assert_eq!(trk["total"], 3 - 1);
    let (_, two) = api.search("q=flicker&kind=issue,document").await;
    assert_eq!(two["total"], 3);
    assert!(
        two["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["kind"] != "comment")
    );
    let (_, oth) = api.search("q=flicker&project=OTH&kind=issue").await;
    assert_eq!(names(&oth), ["OTH-1"]);

    // Prefix and several words.
    let (_, prefix) = api.search("q=compos").await;
    assert!(prefix["total"].as_i64().unwrap() >= 2);
    let (_, both) = api.search(&format!("q={}", enc("screen flashes"))).await;
    assert_eq!(names(&both), [format!("TRK-{flicker}")]);

    // An identifier finds its issue first.
    let (_, ident) = api.search(&format!("q={}", enc("trk 2"))).await;
    assert_eq!(ident["hits"][0]["issue"]["identifier"], "TRK-2");
    assert_eq!(ident["hits"][0]["exact"], true);

    // Search follows edits made through the API.
    let (status, _) = api
        .call(
            "PATCH",
            &format!("/projects/TRK/issues/{flicker}"),
            Some(json!({"title": "Compositor shimmer"})),
        )
        .await;
    assert_eq!(status, 200);
    let (_, gone) = api.search("q=flicker&kind=issue&project=TRK").await;
    assert_eq!(gone["total"], 0);
    let (_, new) = api.search("q=shimmer").await;
    assert_eq!(new["total"], 1);

    // Archived issues are left out unless asked for.
    let (status, _) = api
        .call(
            "POST",
            &format!("/projects/TRK/issues/{flicker}/archive"),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let (_, hidden) = api.search("q=shimmer").await;
    assert_eq!(hidden["total"], 0);
    let (_, shown) = api.search("q=shimmer&include_archived=true").await;
    assert_eq!(shown["hits"][0]["archived"], true);

    // The comment goes when it is deleted.
    let (status, _) = api
        .call(
            "DELETE",
            &format!("/projects/TRK/comments/{comment_id}"),
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (_, none) = api.search("q=nightly&include_archived=true").await;
    assert_eq!(none["total"], 0);

    // Paging.
    for i in 0..5 {
        api.call(
            "POST",
            "/projects/OTH/issues",
            Some(json!({"title": format!("paged {i}")})),
        )
        .await;
    }
    let (_, first) = api.search("q=paged&limit=2").await;
    let (_, second) = api.search("q=paged&limit=2&offset=2").await;
    let (_, third) = api.search("q=paged&limit=2&offset=4").await;
    assert_eq!(first["total"], 5);
    assert_eq!(second["offset"], 2);
    let mut all = names(&first);
    all.extend(names(&second));
    all.extend(names(&third));
    assert_eq!(all, ["OTH-2", "OTH-3", "OTH-4", "OTH-5", "OTH-6"]);

    // Errors: 404 for an unknown project, 422 for bad parameters, JSON bodies.
    let (status, body) = api.search("q=x&project=NOPE").await;
    assert_eq!(status, 404);
    assert_eq!(body, json!({"error": "project not found"}));
    let long = "a".repeat(201);
    let many = vec!["w"; 13].join(" ");
    let bad = [
        String::new(),
        "q=".to_string(),
        "q=%20%20".to_string(),
        "q=%22".to_string(),
        "q=%2A%2A".to_string(),
        "q=%00".to_string(),
        format!("q={long}"),
        format!("q={}", enc(&many)),
        "q=x&limit=0".to_string(),
        "q=x&limit=101".to_string(),
        "q=x&limit=ten".to_string(),
        "q=x&offset=-1".to_string(),
        "q=x&kind=bogus".to_string(),
        "q=x&kind=".to_string(),
        "q=x&include_archived=maybe".to_string(),
        "q=x&surprise=1".to_string(),
    ];
    for query in bad {
        let (status, body) = api.search(&query).await;
        assert_eq!(status, 422, "{query}: {body}");
        assert!(body["error"].is_string(), "{query}: {body}");
    }

    // Operators in the text are plain words, not syntax errors.
    for text in [
        "NEAR(",
        "AND OR NOT",
        "col:value",
        "^x",
        "\"unbalanced",
        "(a",
        "a OR",
    ] {
        let (status, body) = api.search(&format!("q={}", enc(text))).await;
        assert_eq!(status, 200, "{text}: {body}");
    }

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
