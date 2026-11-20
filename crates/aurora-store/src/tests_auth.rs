//! Users, sessions and API keys (TRK-2).

use rusqlite::Connection;
use serde_json::Value;

use crate::auth::{
    self, ApiKey, Authenticated, Login, NewKey, PasswordHash, Principal, Role, SESSION_LIFETIME_MS,
    Scope, TOUCH_INTERVAL_MS, Touch, User,
};
use crate::{Rejection, Store, StoreError};

const NOW: i64 = 1_000_000;
const PASSWORD: &str = "correct horse battery";

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    (dir, store)
}

fn rejection(err: StoreError) -> Rejection {
    match err {
        StoreError::Rejected(rejection) => rejection,
        other => panic!("expected a rejection, got {other}"),
    }
}

fn principal(user: &User, scope: Scope) -> Principal {
    Principal {
        user_id: user.id,
        role: user.role,
        scope,
    }
}

async fn owner_of(store: &Store) -> User {
    let hash = auth::hash_password(PASSWORD).unwrap();
    store
        .write(move |tx| auth::create_owner(tx, NOW, "Ada", &hash))
        .await
        .unwrap()
}

async fn agent_of(store: &Store, owner: &User, name: &'static str) -> User {
    let actor = principal(owner, Scope::Write);
    store
        .write(move |tx| auth::create_agent_user(tx, NOW, &actor, name))
        .await
        .unwrap()
}

/// A human user with the member role; nothing creates one yet, so tests insert it.
async fn member_of(store: &Store, name: &'static str) -> User {
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO user (name, kind, role) VALUES (?1, 'human', 'member')",
                [name],
            )?;
            auth::get_user(tx, tx.last_insert_rowid())
        })
        .await
        .unwrap()
}

async fn new_key(store: &Store, actor: &User, user_id: i64, scope: Scope) -> NewKey {
    let actor = principal(actor, Scope::Write);
    store
        .write(move |tx| auth::create_key(tx, NOW, &actor, user_id, "ci", scope))
        .await
        .unwrap()
}

async fn authenticate(store: &Store, now: i64, key: &str) -> Option<Authenticated> {
    let key = key.to_string();
    store
        .read(move |c| auth::authenticate_key(c, now, &key))
        .await
        .unwrap()
}

/// Every column of every row of `table`, as text.
fn dump(conn: &Connection, table: &str) -> String {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
    let columns = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = String::new();
    while let Some(row) = rows.next().unwrap() {
        for i in 0..columns {
            let value: rusqlite::types::Value = row.get(i).unwrap();
            out.push_str(&format!("{value:?};"));
        }
        out.push('\n');
    }
    out
}

async fn dump_all(store: &Store) -> String {
    store
        .read(|c| {
            Ok(["user", "session", "api_key", "event"]
                .iter()
                .map(|t| dump(c, t))
                .collect::<Vec<_>>()
                .join("\n"))
        })
        .await
        .unwrap()
}

/// (kind, entity_kind, entity_id, actor_id, payload) of every event.
async fn events(store: &Store) -> Vec<(String, String, i64, Option<i64>, Value)> {
    store
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, entity_kind, entity_id, actor_id, payload_json FROM event ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| {
                let payload: String = r.get(4)?;
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    serde_json::from_str(&payload).unwrap(),
                ))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
}

// ---- owner and passwords ----

#[tokio::test]
async fn owner_can_be_created_only_once() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    assert_eq!(owner.role, Role::Owner);
    assert_eq!(owner.kind, "human");
    assert!(!owner.disabled);

    let hash = auth::hash_password(PASSWORD).unwrap();
    for name in ["Ada", "Someone Else"] {
        let hash = hash.clone();
        let err = store
            .write(move |tx| auth::create_owner(tx, NOW, name, &hash))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Conflict(_)), "{name}");
    }
    let users = store.read(auth::list_users).await.unwrap();
    assert_eq!(users, [owner]);
    store.close().await;
}

#[tokio::test]
async fn the_database_itself_refuses_a_second_owner_or_a_duplicate_name() {
    let (_dir, store) = temp_store();
    owner_of(&store).await;
    let second_owner = store
        .write(|tx| {
            Ok(tx.execute(
                "INSERT INTO user (name, kind, role) VALUES ('Other', 'human', 'owner')",
                [],
            )?)
        })
        .await;
    assert!(second_owner.is_err());
    let same_name = store
        .write(|tx| {
            Ok(tx.execute(
                "INSERT INTO user (name, kind, role) VALUES ('ADA', 'human', 'member')",
                [],
            )?)
        })
        .await;
    assert!(same_name.is_err(), "names are unique ignoring case");
    let bad_role = store
        .write(|tx| {
            Ok(tx.execute(
                "INSERT INTO user (name, kind, role) VALUES ('X', 'human', 'root')",
                [],
            )?)
        })
        .await;
    assert!(bad_role.is_err());
    store.close().await;
}

#[test]
fn password_rules() {
    for short in ["", "short", "ninechars"] {
        let err = auth::hash_password(short).unwrap_err();
        assert!(matches!(rejection(err), Rejection::Invalid(_)), "{short:?}");
    }
    assert!(auth::hash_password("tencharsok").is_ok());
    // Length counts characters, not bytes.
    assert!(auth::hash_password("ééééééééé").is_err());
    assert!(auth::hash_password("éééééééééé").is_ok());
    let huge = "x".repeat(auth::MAX_PASSWORD_BYTES + 1);
    assert!(matches!(
        rejection(auth::hash_password(&huge).unwrap_err()),
        Rejection::Invalid(_)
    ));
}

#[tokio::test]
async fn passwords_are_stored_as_salted_argon2id_and_never_in_the_clear() {
    let (_dir, store) = temp_store();
    owner_of(&store).await;
    let stored: String = store
        .read(|c| Ok(c.query_row("SELECT password_hash FROM user", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(stored.starts_with("$argon2id$"), "{stored}");
    assert!(!dump_all(&store).await.contains(PASSWORD));
    let a = auth::hash_password(PASSWORD).unwrap();
    let b = auth::hash_password(PASSWORD).unwrap();
    assert_ne!(a.as_str(), b.as_str(), "each hash has its own salt");
    store.close().await;
}

// ---- sign-in and sessions ----

#[tokio::test]
async fn sign_in_failures_are_indistinguishable() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    agent_of(&store, &owner, "robot").await;
    let member = member_of(&store, "legacy").await;
    store
        .write(move |tx| {
            tx.execute(
                "UPDATE user SET password_hash = (SELECT password_hash FROM user WHERE id = ?1),
                                 disabled = 1 WHERE id = ?2",
                [owner.id, member.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let attempts = [
        ("nobody", PASSWORD),
        ("Ada", "wrong password!"),
        ("Ada", ""),
        ("robot", PASSWORD),
        ("legacy", PASSWORD),
    ];
    let mut messages = Vec::new();
    for (name, password) in attempts {
        let err = store
            .write(move |tx| auth::sign_in(tx, NOW, name, password).map(|_| ()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Rejected(Rejection::Unauthorized(_))),
            "{name}: {err}"
        );
        messages.push(err.to_string());
    }
    assert!(messages.iter().all(|m| *m == messages[0]), "{messages:?}");
    assert!(!messages[0].contains("Ada"));
    let sessions: i64 = store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(sessions, 0);

    // Names are matched ignoring case and surrounding spaces.
    let (_, user) = store
        .write(|tx| auth::sign_in(tx, NOW, "  aDa ", PASSWORD))
        .await
        .unwrap();
    assert_eq!(user.name, "Ada");
    store.close().await;
}

#[tokio::test]
async fn session_authenticates_expires_slides_and_signs_out() {
    let (_dir, store) = temp_store();
    owner_of(&store).await;
    let (session, user) = store
        .write(|tx| auth::sign_in(tx, NOW, "Ada", PASSWORD))
        .await
        .unwrap();
    assert_eq!(session.expires, NOW + SESSION_LIFETIME_MS);
    assert_eq!(session.token.len(), 64);

    let stored = store.read(|c| Ok(dump(c, "session"))).await.unwrap();
    assert!(!stored.contains(&session.token), "only the hash is stored");
    assert!(stored.contains(&auth::sha256_hex(&session.token)));

    let token = session.token.clone();
    let session_auth = move |store: Store, now: i64, token: String| async move {
        store
            .read(move |c| auth::authenticate_session(c, now, &token))
            .await
            .unwrap()
    };
    let found = session_auth(store.clone(), NOW + 5, token.clone())
        .await
        .expect("a fresh session authenticates");
    assert_eq!(
        found.principal,
        Principal {
            user_id: user.id,
            role: Role::Owner,
            scope: Scope::Write
        }
    );
    assert_eq!(found.touch, None, "just signed in, nothing to record yet");
    assert!(
        session_auth(store.clone(), NOW, "0".repeat(64))
            .await
            .is_none()
    );
    assert!(
        session_auth(store.clone(), NOW, String::new())
            .await
            .is_none()
    );
    assert!(
        session_auth(store.clone(), NOW, "x".repeat(500))
            .await
            .is_none()
    );

    // Using it later slides the expiry forward.
    let later = NOW + TOUCH_INTERVAL_MS + 1;
    let found = session_auth(store.clone(), later, token.clone())
        .await
        .unwrap();
    let touch = found.touch.expect("due for a touch");
    store
        .write(move |tx| auth::apply_touch(tx, later, &touch))
        .await
        .unwrap();
    let expires: i64 = store
        .read(|c| Ok(c.query_row("SELECT expires FROM session", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(expires, later + SESSION_LIFETIME_MS);

    // Past the expiry it is gone.
    assert!(
        session_auth(store.clone(), expires, token.clone())
            .await
            .is_none()
    );
    assert!(
        session_auth(store.clone(), expires - 1, token.clone())
            .await
            .is_some()
    );

    // Sign-out invalidates it, once.
    let t = token.clone();
    assert!(store.write(move |tx| auth::sign_out(tx, &t)).await.unwrap());
    assert!(
        session_auth(store.clone(), later, token.clone())
            .await
            .is_none()
    );
    let t = token.clone();
    assert!(!store.write(move |tx| auth::sign_out(tx, &t)).await.unwrap());
    store.close().await;
}

#[tokio::test]
async fn opening_a_session_drops_expired_ones_and_disabled_users_lose_theirs() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let (old, _) = store
        .write(|tx| auth::sign_in(tx, NOW, "Ada", PASSWORD))
        .await
        .unwrap();
    let later = NOW + SESSION_LIFETIME_MS + 1;
    let (fresh, _) = store
        .write(move |tx| auth::sign_in(tx, later, "Ada", PASSWORD))
        .await
        .unwrap();
    let count: i64 = store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 1, "the expired session was removed");
    assert_ne!(old.token, fresh.token);

    store
        .write(move |tx| Ok(tx.execute("UPDATE user SET disabled = 1 WHERE id = ?1", [owner.id])?))
        .await
        .unwrap();
    let token = fresh.token.clone();
    let found = store
        .read(move |c| auth::authenticate_session(c, later, &token))
        .await
        .unwrap();
    assert!(found.is_none());
    store.close().await;
}

// ---- keys ----

#[tokio::test]
async fn key_is_shown_once_and_only_its_hash_is_stored() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let created = new_key(&store, &owner, owner.id, Scope::Write).await;

    assert!(created.key.starts_with("aur_"));
    assert!(created.key.len() >= 44, "{}", created.key.len());
    assert!(created.key[4..].chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(created.info.prefix, created.key[..8]);
    assert_eq!(created.info.name, "ci");
    assert_eq!(created.info.scope, Scope::Write);
    assert_eq!(created.info.last_used, None);
    assert_eq!(created.info.revoked, None);

    let row = store.read(|c| Ok(dump(c, "api_key"))).await.unwrap();
    assert!(
        !row.contains(&created.key),
        "plaintext must not be stored: {row}"
    );
    assert!(row.contains(&auth::sha256_hex(&created.key)));
    assert!(!dump_all(&store).await.contains(&created.key));

    // Two keys never collide and never share a secret.
    let other = new_key(&store, &owner, owner.id, Scope::Read).await;
    assert_ne!(created.key, other.key);

    // Listing never returns a secret or a hash.
    let actor = principal(&owner, Scope::Write);
    let listed: Vec<ApiKey> = store
        .read(move |c| auth::list_keys(c, &actor, actor.user_id))
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    let json = serde_json::to_string(&listed).unwrap();
    assert!(!json.contains(&created.key) && !json.contains(&other.key));
    assert!(!json.contains(&auth::sha256_hex(&created.key)));
    assert!(!json.contains("key_hash"));
    assert!(json.contains(&created.info.prefix));
    let shown = serde_json::to_value(&created).unwrap();
    assert_eq!(
        shown["key"],
        created.key.as_str(),
        "the one time it is shown"
    );
    store.close().await;
}

#[tokio::test]
async fn authenticate_key_finds_the_user_and_scope_and_rejects_others() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let agent = agent_of(&store, &owner, "robot").await;
    let read = new_key(&store, &owner, agent.id, Scope::Read).await;
    let write = new_key(&store, &owner, agent.id, Scope::Write).await;

    let found = authenticate(&store, NOW, &read.key).await.unwrap();
    assert_eq!(
        found.principal,
        Principal {
            user_id: agent.id,
            role: Role::Agent,
            scope: Scope::Read
        }
    );
    assert!(!found.principal.scope.allows_writes());
    let found = authenticate(&store, NOW, &write.key).await.unwrap();
    assert_eq!(found.principal.scope, Scope::Write);
    assert!(found.principal.scope.allows_writes());

    assert!(authenticate(&store, NOW, "").await.is_none());
    assert!(authenticate(&store, NOW, "aur_nothing").await.is_none());
    assert!(authenticate(&store, NOW, &"a".repeat(500)).await.is_none());
    // The stored hash is not itself a credential.
    assert!(
        authenticate(&store, NOW, &auth::sha256_hex(&write.key))
            .await
            .is_none()
    );
    // Nor is the prefix, nor the key with a character changed.
    assert!(
        authenticate(&store, NOW, &write.info.prefix)
            .await
            .is_none()
    );
    let mut altered = write.key.clone();
    altered.pop();
    altered.push('z');
    assert!(authenticate(&store, NOW, &altered).await.is_none());
    store.close().await;
}

#[tokio::test]
async fn rotation_replaces_the_secret_at_once_in_the_same_row() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let created = new_key(&store, &owner, owner.id, Scope::Read).await;
    let actor = principal(&owner, Scope::Write);
    let id = created.info.id;

    let rotated = store
        .write(move |tx| auth::rotate_key(tx, NOW + 10, &actor, id))
        .await
        .unwrap();
    assert_ne!(rotated.key, created.key);
    assert_eq!(rotated.info.id, created.info.id);
    assert_eq!(rotated.info.name, created.info.name);
    assert_eq!(rotated.info.scope, Scope::Read);
    assert_eq!(rotated.info.created, created.info.created);
    assert_eq!(rotated.info.prefix, rotated.key[..8]);

    assert!(authenticate(&store, NOW + 10, &created.key).await.is_none());
    let found = authenticate(&store, NOW + 10, &rotated.key).await.unwrap();
    assert_eq!(found.principal.scope, Scope::Read);
    let count: i64 = store
        .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM api_key", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 1);
    let row = store.read(|c| Ok(dump(c, "api_key"))).await.unwrap();
    assert!(!row.contains(&rotated.key) && !row.contains(&created.key));

    let missing = store
        .write(move |tx| auth::rotate_key(tx, NOW, &actor, 9999))
        .await
        .unwrap_err();
    assert!(matches!(rejection(missing), Rejection::NotFound(_)));
    store.close().await;
}

#[tokio::test]
async fn revoked_key_stops_working_and_cannot_be_rotated() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let created = new_key(&store, &owner, owner.id, Scope::Write).await;
    let actor = principal(&owner, Scope::Write);
    let id = created.info.id;

    let revoked = store
        .write(move |tx| auth::revoke_key(tx, NOW + 5, &actor, id))
        .await
        .unwrap();
    assert_eq!(revoked.revoked, Some(NOW + 5));
    assert!(authenticate(&store, NOW + 6, &created.key).await.is_none());

    // Repeating changes nothing, writes no event.
    let before = events(&store).await.len();
    let again = store
        .write(move |tx| auth::revoke_key(tx, NOW + 99, &actor, id))
        .await
        .unwrap();
    assert_eq!(again.revoked, Some(NOW + 5));
    assert_eq!(events(&store).await.len(), before);

    let err = store
        .write(move |tx| auth::rotate_key(tx, NOW, &actor, id))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Conflict(_)));
    assert!(authenticate(&store, NOW, &created.key).await.is_none());

    // Revoked keys are still listed, marked.
    let listed = store
        .read(move |c| auth::list_keys(c, &actor, actor.user_id))
        .await
        .unwrap();
    assert_eq!(listed[0].revoked, Some(NOW + 5));
    store.close().await;
}

#[tokio::test]
async fn disabled_users_keys_do_not_authenticate_and_cannot_get_new_ones() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let agent = agent_of(&store, &owner, "robot").await;
    let key = new_key(&store, &owner, agent.id, Scope::Write).await;
    let agent_id = agent.id;
    store
        .write(move |tx| Ok(tx.execute("UPDATE user SET disabled = 1 WHERE id = ?1", [agent_id])?))
        .await
        .unwrap();
    assert!(authenticate(&store, NOW, &key.key).await.is_none());
    let actor = principal(&owner, Scope::Write);
    let err = store
        .write(move |tx| auth::create_key(tx, NOW, &actor, agent_id, "again", Scope::Read))
        .await
        .unwrap_err();
    assert!(matches!(rejection(err), Rejection::Conflict(_)));
    store.close().await;
}

#[tokio::test]
async fn only_the_owner_creates_users_and_manages_other_peoples_keys() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let agent = agent_of(&store, &owner, "robot").await;
    let member = member_of(&store, "mia").await;
    let owners_key = new_key(&store, &owner, owner.id, Scope::Write).await;

    for who in [&member, &agent] {
        let actor = principal(who, Scope::Write);
        let err = store
            .write(move |tx| auth::create_agent_user(tx, NOW, &actor, "sneaky"))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)), "{who:?}");

        let owner_id = owner.id;
        let err = store
            .write(move |tx| auth::create_key(tx, NOW, &actor, owner_id, "x", Scope::Write))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
        let err = store
            .read(move |c| auth::list_keys(c, &actor, owner_id).map(|_| ()))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
        let key_id = owners_key.info.id;
        let err = store
            .write(move |tx| auth::rotate_key(tx, NOW, &actor, key_id))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
        let err = store
            .write(move |tx| auth::revoke_key(tx, NOW, &actor, key_id))
            .await
            .unwrap_err();
        assert!(matches!(rejection(err), Rejection::Forbidden(_)));
    }
    assert!(authenticate(&store, NOW, &owners_key.key).await.is_some());

    // They can manage their own.
    let actor = principal(&agent, Scope::Write);
    let own = new_key(&store, &agent, agent.id, Scope::Read).await;
    let id = own.info.id;
    store
        .write(move |tx| auth::rotate_key(tx, NOW, &actor, id))
        .await
        .unwrap();
    store
        .write(move |tx| auth::revoke_key(tx, NOW, &actor, id))
        .await
        .unwrap();

    // The owner can manage an agent's keys and sees them.
    let actor = principal(&owner, Scope::Write);
    let agent_id = agent.id;
    let listed = store
        .read(move |c| auth::list_keys(c, &actor, agent_id))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    store.close().await;
}

#[tokio::test]
async fn last_used_is_written_at_most_once_a_minute() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let created = new_key(&store, &owner, owner.id, Scope::Write).await;
    let last_used = |store: Store| async move {
        store
            .read(|c| {
                Ok(c.query_row("SELECT last_used FROM api_key", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })?)
            })
            .await
            .unwrap()
    };

    let t0 = NOW + 1000;
    let first = authenticate(&store, t0, &created.key).await.unwrap();
    let touch = first.touch.expect("never used: due");
    assert!(matches!(touch, Touch::Key(_)));
    let t = touch.clone();
    store
        .write(move |tx| auth::apply_touch(tx, t0, &t))
        .await
        .unwrap();
    assert_eq!(last_used(store.clone()).await, Some(t0));

    // Within the interval: not due, and a stale touch changes nothing.
    let soon = t0 + TOUCH_INTERVAL_MS - 1;
    assert!(
        authenticate(&store, soon, &created.key)
            .await
            .unwrap()
            .touch
            .is_none()
    );
    let t = touch.clone();
    store
        .write(move |tx| auth::apply_touch(tx, soon, &t))
        .await
        .unwrap();
    assert_eq!(last_used(store.clone()).await, Some(t0));

    let due = t0 + TOUCH_INTERVAL_MS;
    let touch = authenticate(&store, due, &created.key)
        .await
        .unwrap()
        .touch
        .unwrap();
    store
        .write(move |tx| auth::apply_touch(tx, due, &touch))
        .await
        .unwrap();
    assert_eq!(last_used(store.clone()).await, Some(due));

    // A key that was just rotated starts again from "never used".
    let actor = principal(&owner, Scope::Write);
    let id = created.info.id;
    let rotated = store
        .write(move |tx| auth::rotate_key(tx, due, &actor, id))
        .await
        .unwrap();
    assert_eq!(rotated.info.last_used, None);
    store.close().await;
}

// ---- events ----

#[tokio::test]
async fn user_and_key_changes_write_one_event_each_without_secrets() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let agent = agent_of(&store, &owner, "robot").await;
    let created = new_key(&store, &owner, agent.id, Scope::Read).await;
    let actor = principal(&owner, Scope::Write);
    let id = created.info.id;
    let rotated = store
        .write(move |tx| auth::rotate_key(tx, NOW, &actor, id))
        .await
        .unwrap();
    store
        .write(move |tx| auth::revoke_key(tx, NOW, &actor, id))
        .await
        .unwrap();
    // Signing in and out is not an event.
    let (session, _) = store
        .write(|tx| auth::sign_in(tx, NOW, "Ada", PASSWORD))
        .await
        .unwrap();
    let token = session.token.clone();
    store
        .write(move |tx| auth::sign_out(tx, &token))
        .await
        .unwrap();
    authenticate(&store, NOW, &rotated.key).await;

    let seen = events(&store).await;
    let kinds: Vec<&str> = seen.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "user.created",
            "user.created",
            "key.created",
            "key.rotated",
            "key.revoked"
        ]
    );
    assert_eq!(seen[0].3, None, "setup has no acting user");
    assert_eq!(seen[0].4["role"], "owner");
    assert_eq!((seen[0].1.as_str(), seen[0].2), ("user", owner.id));
    assert_eq!(seen[1].3, Some(owner.id));
    assert_eq!(seen[1].4["name"], "robot");
    for (event, key_event) in seen[2..].iter().zip(["created", "rotated", "revoked"]) {
        assert_eq!(event.3, Some(owner.id), "{key_event}");
        assert_eq!((event.1.as_str(), event.2), ("api_key", id));
        assert_eq!(event.4["user_id"], agent.id);
    }
    assert_eq!(seen[2].4["scope"], "read");
    assert_eq!(seen[3].4["previous_prefix"], created.info.prefix.as_str());
    assert_eq!(seen[3].4["prefix"], rotated.info.prefix.as_str());

    let payloads = serde_json::to_string(&seen).unwrap();
    for secret in [
        created.key.as_str(),
        rotated.key.as_str(),
        session.token.as_str(),
        PASSWORD,
    ] {
        assert!(!payloads.contains(secret));
    }
    assert!(!payloads.contains("$argon2"));
    assert!(!payloads.contains(&auth::sha256_hex(&created.key)));
    assert!(!payloads.contains(&auth::sha256_hex(&rotated.key)));
    store.close().await;
}

#[tokio::test]
async fn a_rolled_back_change_leaves_no_row_and_no_event() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let before = events(&store).await.len();
    let actor = principal(&owner, Scope::Write);
    let owner_id = owner.id;
    let outcome = store
        .write(move |tx| {
            auth::create_agent_user(tx, NOW, &actor, "robot")?;
            auth::create_key(tx, NOW, &actor, owner_id, "k", Scope::Write)?;
            Err::<(), _>(StoreError::Closed)
        })
        .await;
    assert!(matches!(outcome, Err(StoreError::Closed)));
    assert_eq!(events(&store).await.len(), before);
    let (users, keys): (i64, i64) = store
        .read(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM api_key", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((users, keys), (1, 0));

    let dup = store
        .write(move |tx| auth::create_agent_user(tx, NOW, &actor, "ADA"))
        .await
        .unwrap_err();
    assert!(matches!(rejection(dup), Rejection::Conflict(_)));
    assert_eq!(events(&store).await.len(), before);
    store.close().await;
}

// ---- Debug output ----

#[tokio::test]
async fn debug_output_never_shows_secrets() {
    let (_dir, store) = temp_store();
    let owner = owner_of(&store).await;
    let key = new_key(&store, &owner, owner.id, Scope::Write).await;
    let (session, _) = store
        .write(|tx| auth::sign_in(tx, NOW, "Ada", PASSWORD))
        .await
        .unwrap();
    let login: Login = store
        .read(|c| Ok(auth::login_lookup(c, "Ada")?.unwrap()))
        .await
        .unwrap();
    let hash: PasswordHash = auth::hash_password(PASSWORD).unwrap();
    let stored_hash: String = store
        .read(|c| Ok(c.query_row("SELECT password_hash FROM user", [], |r| r.get(0))?))
        .await
        .unwrap();
    let hashed_session = auth::sha256_hex(&session.token);
    let found = store
        .read({
            let token = session.token.clone();
            move |c| auth::authenticate_session(c, NOW + TOUCH_INTERVAL_MS, &token)
        })
        .await
        .unwrap()
        .unwrap();

    let text = format!(
        "{key:?} {session:?} {login:?} {hash:?} {found:?} {:?}",
        found.touch
    );
    for secret in [
        key.key.as_str(),
        session.token.as_str(),
        stored_hash.as_str(),
        hash.as_str(),
        hashed_session.as_str(),
        PASSWORD,
    ] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("redacted"));
    store.close().await;
}
