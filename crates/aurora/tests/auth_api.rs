//! Sign-in, sessions, API keys and the authentication layer over real HTTP.

use aurora_store::Store;
use aurora_store::auth::{self, Principal, Scope};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const PASSWORD: &str = "correct horse battery";
const CSRF: (&str, &str) = ("X-Requested-With", "aurora");

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// `name=value` of the session cookie, ready to send back.
    fn session_cookie(&self) -> String {
        let set = self.header("set-cookie").expect("a Set-Cookie header");
        set.split(';').next().unwrap().to_string()
    }
}

struct Server {
    addr: std::net::SocketAddr,
    store: Store,
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.db")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = tokio::spawn(aurora::serve(
            listener,
            store.clone(),
            async {
                let _ = stopped.await;
            },
            aurora::ShutdownHooks::new(),
        ));
        Server {
            addr,
            store,
            stop,
            handle,
            _dir: dir,
        }
    }

    async fn finish(self) {
        self.stop.send(()).unwrap();
        self.handle.await.unwrap().unwrap();
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let extra: String = headers
            .iter()
            .map(|(n, v)| format!("{n}: {v}\r\n"))
            .collect();
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let (head, text) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
            .collect();
        let text = text.trim();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(text).unwrap_or_else(|_| panic!("not JSON: {raw}"))
        };
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn api(
        &self,
        method: &str,
        path: &str,
        auth: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        self.send(method, &format!("/api/v1{path}"), auth, body)
            .await
    }

    async fn setup(&self) -> Reply {
        self.api(
            "POST",
            "/setup",
            &[],
            Some(json!({"name": "Ada", "password": PASSWORD})),
        )
        .await
    }

    async fn sign_in(&self, name: &str, password: &str) -> Reply {
        self.api(
            "POST",
            "/auth/sign-in",
            &[],
            Some(json!({"name": name, "password": password})),
        )
        .await
    }

    /// Sets up the owner, signs in and returns the cookie.
    async fn owner_cookie(&self) -> String {
        assert_eq!(self.setup().await.status, 201);
        let reply = self.sign_in("Ada", PASSWORD).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.session_cookie()
    }

    /// A write key for the owner, created with the cookie.
    async fn owner_key(&self, cookie: &str, scope: &str) -> (i64, String) {
        let reply = self
            .api(
                "POST",
                "/keys",
                &[("Cookie", cookie), CSRF],
                Some(json!({"name": "test", "scope": scope})),
            )
            .await;
        assert_eq!(reply.status, 201, "{}", reply.body);
        (
            reply.body["id"].as_i64().unwrap(),
            reply.body["key"].as_str().unwrap().to_string(),
        )
    }
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

#[tokio::test]
async fn setup_works_once_and_health_and_sign_in_stay_public() {
    let server = Server::start().await;
    let health = server.send("GET", "/health", &[], None).await;
    assert_eq!(health.status, 200);

    // Refused setups do not use up the one-time setup.
    let short = server
        .api(
            "POST",
            "/setup",
            &[],
            Some(json!({"name": "Ada", "password": "short"})),
        )
        .await;
    assert_eq!(short.status, 422, "{}", short.body);
    let unknown_field = server
        .api(
            "POST",
            "/setup",
            &[],
            Some(json!({"name": "Ada", "password": PASSWORD, "role": "owner"})),
        )
        .await;
    assert_eq!(unknown_field.status, 422);

    let done = server.setup().await;
    assert_eq!(done.status, 201, "{}", done.body);
    assert_eq!(done.body["name"], "Ada");
    assert_eq!(done.body["role"], "owner");
    assert!(done.body.get("password").is_none() && done.body.get("password_hash").is_none());
    assert!(!done.body.to_string().contains(PASSWORD));

    let again = server.setup().await;
    assert_eq!(again.status, 409, "{}", again.body);
    let other = server
        .api(
            "POST",
            "/setup",
            &[],
            Some(json!({"name": "Mallory", "password": PASSWORD})),
        )
        .await;
    assert_eq!(other.status, 409);

    // Sign-in needs no credentials; a failure is the same for every reason.
    let wrong = server.sign_in("Ada", "not the password").await;
    let unknown = server.sign_in("nobody", PASSWORD).await;
    for reply in [&wrong, &unknown] {
        assert_eq!(reply.status, 401);
        assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
        assert!(reply.header("set-cookie").is_none());
    }
    assert_eq!(wrong.body, unknown.body);
    let ok = server.sign_in("Ada", PASSWORD).await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body["name"], "Ada");
    server.finish().await;
}

#[tokio::test]
async fn protected_routes_reject_missing_bad_and_malformed_credentials() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let (key_id, key) = server.owner_key(&cookie, "write").await;

    let routes = [
        ("GET", "/projects"),
        ("POST", "/projects"),
        ("GET", "/events"),
        ("GET", "/consumers"),
        ("GET", "/keys"),
        ("POST", "/keys"),
        ("DELETE", "/keys/1"),
        ("POST", "/keys/1/rotate"),
        ("GET", "/users"),
        ("POST", "/users"),
        ("GET", "/auth/me"),
        ("POST", "/auth/sign-out"),
    ];
    let bad_key = bearer("aur_0000000000000000000000000000000000000000000000000000000000000000");
    let bad_cookie = "aurora_session=0000";
    let attempts: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("no credentials", vec![]),
        ("unknown key", vec![("Authorization", bad_key.as_str())]),
        ("not a bearer header", vec![("Authorization", "Basic YTpi")]),
        ("empty bearer", vec![("Authorization", "Bearer ")]),
        ("unknown session", vec![("Cookie", bad_cookie)]),
    ];
    for (method, path) in routes {
        for (what, headers) in &attempts {
            let mut headers = headers.clone();
            headers.push(CSRF);
            let reply = server.api(method, path, &headers, Some(json!({}))).await;
            assert_eq!(
                reply.status, 401,
                "{method} {path} with {what}: {}",
                reply.body
            );
            assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
            assert!(reply.body["error"].is_string(), "{method} {path}");
        }
    }

    // A valid cookie next to a bad Authorization header does not rescue the request.
    let mixed = server
        .api(
            "GET",
            "/projects",
            &[
                ("Authorization", bad_key.as_str()),
                ("Cookie", cookie.as_str()),
            ],
            None,
        )
        .await;
    assert_eq!(mixed.status, 401);

    // A revoked key is rejected like an unknown one.
    let auth_header = bearer(&key);
    let ok = server
        .api(
            "GET",
            "/projects",
            &[("Authorization", auth_header.as_str())],
            None,
        )
        .await;
    assert_eq!(ok.status, 200);
    let revoked = server
        .api(
            "DELETE",
            &format!("/keys/{key_id}"),
            &[("Cookie", cookie.as_str()), CSRF],
            None,
        )
        .await;
    assert_eq!(revoked.status, 204);
    let after = server
        .api(
            "GET",
            "/projects",
            &[("Authorization", auth_header.as_str())],
            None,
        )
        .await;
    assert_eq!(after.status, 401, "{}", after.body);
    assert_eq!(after.header("www-authenticate"), Some("Bearer"));
    server.finish().await;
}

#[tokio::test]
async fn key_lifecycle_create_use_rotate_revoke() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let (id, key) = server.owner_key(&cookie, "write").await;
    assert!(key.starts_with("aur_") && key.len() >= 44);

    let created = server
        .api(
            "POST",
            "/keys",
            &[("Cookie", cookie.as_str()), CSRF],
            Some(json!({"name": "other"})),
        )
        .await;
    assert_eq!(created.header("cache-control"), Some("no-store"));
    assert_eq!(created.body["scope"], "write", "write is the default scope");

    // Using the key.
    let header = bearer(&key);
    let auth_headers = [("Authorization", header.as_str())];
    let made = server
        .api(
            "POST",
            "/projects",
            &auth_headers,
            Some(json!({"key": "TRK", "name": "Tracker"})),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.body);

    // Listing shows the key's prefix and use, never the secret.
    let listed = server
        .api("GET", "/keys", &[("Cookie", cookie.as_str())], None)
        .await;
    assert_eq!(listed.status, 200);
    assert!(!listed.body.to_string().contains(&key));
    let row = listed
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == id)
        .unwrap();
    assert_eq!(row["prefix"], key[..8]);
    assert!(row.get("key").is_none() && row.get("key_hash").is_none());
    assert!(row["last_used"].is_i64(), "{row}");
    assert!(row["revoked"].is_null());

    // Rotation: same id, new secret, the old one dies at once.
    let rotated = server
        .api("POST", &format!("/keys/{id}/rotate"), &auth_headers, None)
        .await;
    assert_eq!(rotated.status, 200, "{}", rotated.body);
    assert_eq!(rotated.body["id"], id);
    let new_key = rotated.body["key"].as_str().unwrap().to_string();
    assert_ne!(new_key, key);
    let old = server.api("GET", "/projects", &auth_headers, None).await;
    assert_eq!(old.status, 401);
    let new_header = bearer(&new_key);
    let new_headers = [("Authorization", new_header.as_str())];
    let fresh = server.api("GET", "/projects", &new_headers, None).await;
    assert_eq!(fresh.status, 200);

    // Revocation.
    let revoked = server
        .api("DELETE", &format!("/keys/{id}"), &new_headers, None)
        .await;
    assert_eq!(revoked.status, 204);
    assert_eq!(
        server
            .api("GET", "/projects", &new_headers, None)
            .await
            .status,
        401
    );
    let again = server
        .api(
            "POST",
            &format!("/keys/{id}/rotate"),
            &[("Cookie", cookie.as_str()), CSRF],
            None,
        )
        .await;
    assert_eq!(again.status, 409);
    let missing = server
        .api(
            "DELETE",
            "/keys/99999",
            &[("Cookie", cookie.as_str()), CSRF],
            None,
        )
        .await;
    assert_eq!(missing.status, 404);

    // The events name the acting user and never carry a secret.
    let events = server
        .api(
            "GET",
            "/events?kind_prefix=key.",
            &[("Cookie", cookie.as_str())],
            None,
        )
        .await;
    let kinds: Vec<&str> = events.body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["key.created", "key.created", "key.rotated", "key.revoked"]
    );
    for event in events.body["events"].as_array().unwrap() {
        assert_eq!(event["actor"], 1);
    }
    let text = events.body.to_string();
    assert!(!text.contains(&key) && !text.contains(&new_key));
    server.finish().await;
}

#[tokio::test]
async fn read_scope_key_can_only_read() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let (_, write_key) = server.owner_key(&cookie, "write").await;
    let (read_id, read_key) = server.owner_key(&cookie, "read").await;
    let write_header = bearer(&write_key);
    server
        .api(
            "POST",
            "/projects",
            &[("Authorization", write_header.as_str())],
            Some(json!({"key": "TRK", "name": "T"})),
        )
        .await;

    let header = bearer(&read_key);
    let headers = [("Authorization", header.as_str())];
    for path in ["/projects", "/projects/TRK", "/events", "/keys", "/auth/me"] {
        let reply = server.api("GET", path, &headers, None).await;
        assert_eq!(reply.status, 200, "GET {path}: {}", reply.body);
    }
    let head = server.api("HEAD", "/projects", &headers, None).await;
    assert_eq!(head.status, 200);
    let me = server.api("GET", "/auth/me", &headers, None).await;
    assert_eq!(
        (me.body["scope"].as_str(), me.body["via"].as_str()),
        (Some("read"), Some("key"))
    );

    for (method, path, body) in [
        (
            "POST",
            "/projects",
            Some(json!({"key": "NEW", "name": "N"})),
        ),
        ("PATCH", "/projects/TRK", Some(json!({"name": "X"}))),
        ("POST", "/projects/TRK/issues", Some(json!({"title": "x"}))),
        (
            "POST",
            "/keys",
            Some(json!({"name": "escalate", "scope": "write"})),
        ),
        ("POST", &format!("/keys/{read_id}/rotate"), None),
        ("DELETE", &format!("/keys/{read_id}"), None),
        ("POST", "/users", Some(json!({"name": "x"}))),
    ] {
        let reply = server.api(method, path, &headers, body).await;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
        assert!(reply.body["error"].is_string());
    }
    // Nothing was changed by the refused requests.
    let projects = server.api("GET", "/projects", &headers, None).await;
    assert_eq!(projects.body.as_array().unwrap().len(), 1);
    assert_eq!(projects.body[0]["name"], "T");
    server.finish().await;
}

#[tokio::test]
async fn session_cookie_attributes_csrf_and_sign_out() {
    let server = Server::start().await;
    assert_eq!(server.setup().await.status, 201);
    let reply = server.sign_in("Ada", PASSWORD).await;
    let set = reply.header("set-cookie").unwrap().to_string();
    assert!(set.starts_with("aurora_session="), "{set}");
    for attribute in ["HttpOnly", "SameSite=Lax", "Path=/", "Max-Age="] {
        assert!(set.contains(attribute), "{set}");
    }
    assert!(!set.contains("Secure"), "plain HTTP: {set}");
    assert_eq!(reply.all("set-cookie").len(), 1);
    assert!(
        !reply
            .body
            .to_string()
            .contains(set.split(';').next().unwrap().split('=').nth(1).unwrap())
    );

    let behind_proxy = server
        .api(
            "POST",
            "/auth/sign-in",
            &[("X-Forwarded-Proto", "https")],
            Some(json!({"name": "Ada", "password": PASSWORD})),
        )
        .await;
    assert!(
        behind_proxy
            .header("set-cookie")
            .unwrap()
            .contains("; Secure")
    );

    let cookie = reply.session_cookie();
    // Reads work with the cookie alone.
    let me = server
        .api("GET", "/auth/me", &[("Cookie", cookie.as_str())], None)
        .await;
    assert_eq!(me.status, 200, "{}", me.body);
    assert_eq!(me.body["user"]["name"], "Ada");
    assert_eq!(me.body["via"], "session");
    assert_eq!(
        server
            .api("GET", "/projects", &[("Cookie", cookie.as_str())], None)
            .await
            .status,
        200
    );

    // A state-changing request needs the CSRF header; without it nothing happens.
    let project = json!({"key": "TRK", "name": "T"});
    let refused = server
        .api(
            "POST",
            "/projects",
            &[("Cookie", cookie.as_str())],
            Some(project.clone()),
        )
        .await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(
        refused.body["error"]
            .as_str()
            .unwrap()
            .contains("X-Requested-With")
    );
    let wrong_value = server
        .api(
            "POST",
            "/projects",
            &[("Cookie", cookie.as_str()), ("X-Requested-With", "nope")],
            Some(project.clone()),
        )
        .await;
    assert_eq!(wrong_value.status, 403);
    let list = server
        .api("GET", "/projects", &[("Cookie", cookie.as_str())], None)
        .await;
    assert_eq!(list.body, json!([]));
    let allowed = server
        .api(
            "POST",
            "/projects",
            &[("Cookie", cookie.as_str()), CSRF],
            Some(project),
        )
        .await;
    assert_eq!(allowed.status, 201, "{}", allowed.body);

    // Sign-out without the header is refused and keeps the session; with it, it ends it.
    let refused = server
        .api(
            "POST",
            "/auth/sign-out",
            &[("Cookie", cookie.as_str())],
            None,
        )
        .await;
    assert_eq!(refused.status, 403);
    assert_eq!(
        server
            .api("GET", "/auth/me", &[("Cookie", cookie.as_str())], None)
            .await
            .status,
        200
    );
    let out = server
        .api(
            "POST",
            "/auth/sign-out",
            &[("Cookie", cookie.as_str()), CSRF],
            None,
        )
        .await;
    assert_eq!(out.status, 204);
    let cleared = out.header("set-cookie").unwrap();
    assert!(
        cleared.starts_with("aurora_session=;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );
    let after = server
        .api("GET", "/auth/me", &[("Cookie", cookie.as_str())], None)
        .await;
    assert_eq!(
        after.status, 401,
        "the signed-out cookie is dead on the server too"
    );
    server.finish().await;
}

#[tokio::test]
async fn only_the_owner_manages_users_and_other_peoples_keys() {
    let server = Server::start().await;
    let cookie = server.owner_cookie().await;
    let owner_headers = [("Cookie", cookie.as_str()), CSRF];

    // The owner creates an agent user and a key for it.
    let agent = server
        .api(
            "POST",
            "/users",
            &owner_headers,
            Some(json!({"name": "robot"})),
        )
        .await;
    assert_eq!(agent.status, 201, "{}", agent.body);
    assert_eq!(
        (agent.body["role"].as_str(), agent.body["kind"].as_str()),
        (Some("agent"), Some("agent"))
    );
    let agent_id = agent.body["id"].as_i64().unwrap();
    let dup = server
        .api(
            "POST",
            "/users",
            &owner_headers,
            Some(json!({"name": "ROBOT"})),
        )
        .await;
    assert_eq!(dup.status, 409);
    let users = server
        .api("GET", "/users", &[("Cookie", cookie.as_str())], None)
        .await;
    assert_eq!(users.status, 200);
    assert_eq!(users.body.as_array().unwrap().len(), 2);
    assert!(!users.body.to_string().contains("password"));

    let agent_key = server
        .api(
            "POST",
            "/keys",
            &owner_headers,
            Some(json!({"name": "agent key", "user_id": agent_id})),
        )
        .await;
    assert_eq!(agent_key.status, 201, "{}", agent_key.body);
    assert_eq!(agent_key.body["user_id"], agent_id);
    let agent_secret = agent_key.body["key"].as_str().unwrap().to_string();
    let agent_key_id = agent_key.body["id"].as_i64().unwrap();
    let owners = server
        .api(
            "GET",
            &format!("/keys?user_id={agent_id}"),
            &[("Cookie", cookie.as_str())],
            None,
        )
        .await;
    assert_eq!(owners.body.as_array().unwrap().len(), 1);

    // The agent acts as itself but cannot create users or touch the owner's keys.
    let agent_header = bearer(&agent_secret);
    let agent_headers = [("Authorization", agent_header.as_str())];
    let me = server.api("GET", "/auth/me", &agent_headers, None).await;
    assert_eq!(me.body["user"]["name"], "robot");
    assert_eq!(me.body["user"]["role"], "agent");
    let owner_key_list = server
        .api("GET", "/keys", &[("Cookie", cookie.as_str())], None)
        .await;
    assert!(owner_key_list.body.as_array().unwrap().is_empty());
    for (method, path, body) in [
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
            Some(json!({"name": "x", "user_id": 1})),
        ),
    ] {
        let reply = server.api(method, &path, &agent_headers, body).await;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
    }
    // ...but it manages its own.
    let own = server
        .api(
            "POST",
            "/keys",
            &agent_headers,
            Some(json!({"name": "second", "scope": "read"})),
        )
        .await;
    assert_eq!(own.status, 201, "{}", own.body);
    assert_eq!(own.body["user_id"], agent_id);
    let own_id = own.body["id"].as_i64().unwrap();
    let rotated = server
        .api(
            "POST",
            &format!("/keys/{own_id}/rotate"),
            &agent_headers,
            None,
        )
        .await;
    assert_eq!(rotated.status, 200);

    // A member (nothing creates one yet, so it is seeded) is held to the same rules.
    let owner_principal = Principal {
        user_id: 1,
        role: auth::Role::Owner,
        scope: Scope::Write,
    };
    let member_key = server
        .store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO user (name, kind, role) VALUES ('mia', 'human', 'member')",
                [],
            )?;
            let member = tx.last_insert_rowid();
            auth::create_key(tx, 1, &owner_principal, member, "mia", Scope::Write)
        })
        .await
        .unwrap();
    let member_header = bearer(&member_key.key);
    let member_headers = [("Authorization", member_header.as_str())];
    let created = server
        .api(
            "POST",
            "/users",
            &member_headers,
            Some(json!({"name": "another"})),
        )
        .await;
    assert_eq!(created.status, 403, "{}", created.body);
    let rotate_other = server
        .api(
            "POST",
            &format!("/keys/{agent_key_id}/rotate"),
            &member_headers,
            None,
        )
        .await;
    assert_eq!(rotate_other.status, 403);
    let revoke_other = server
        .api(
            "DELETE",
            &format!("/keys/{agent_key_id}"),
            &member_headers,
            None,
        )
        .await;
    assert_eq!(revoke_other.status, 403);
    // The agent's key is untouched by all of the above.
    assert_eq!(
        server
            .api("GET", "/auth/me", &agent_headers, None)
            .await
            .status,
        200
    );
    server.finish().await;
}
