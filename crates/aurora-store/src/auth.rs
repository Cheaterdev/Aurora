//! Users, passwords, sessions and API keys.
//!
//! Version 1 is single-owner with a team-ready data model: one `owner`
//! account, `agent` users (one per agent, each with its own keys) and a plain
//! `member` role that nothing creates yet. There are no per-project
//! permissions; a key is limited to the [`Scope::Read`] or [`Scope::Write`]
//! scope. A session cookie acts with write scope.
//!
//! Plain functions over a transaction (writes) or a connection (reads), like
//! the other store modules. User and key changes append their event in the
//! same transaction; signing in and out do not (too noisy), and neither does
//! the `last_used` / `last_seen` bookkeeping.
//!
//! # Secrets
//!
//! * API keys are `aur_` plus 64 hex characters from the OS random source.
//!   Only their SHA-256 is stored; a lookup hashes the presented key and finds
//!   the row through the `UNIQUE` index, so no secret is ever compared in
//!   application code. High-entropy keys need no slow hash.
//! * Session tokens are 64 hex characters, stored the same way.
//! * Passwords are hashed with Argon2id and a per-password salt. The hash is
//!   slow on purpose, so the HTTP layer runs it off the writer thread: look
//!   the user up with [`login_lookup`], check with [`check_password`] on a
//!   blocking thread, then [`open_session`] in a write. [`sign_in`] does all
//!   three in one transaction for tests and tools.
//! * A secret is returned exactly once, in [`NewKey::key`] or
//!   [`NewSession::token`]. Types that hold one have a `Debug` that hides it.

use std::fmt;
use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash as PhcHash, PasswordHasher, PasswordVerifier, SaltString,
};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::StoreError;
use crate::projects::{Rejection, conflict, emit_as, invalid, not_found, refusal};

pub const MIN_PASSWORD_CHARS: usize = 10;
/// Longer passwords are refused so a request cannot make hashing expensive.
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// A session expires this long after it was last used (sliding window).
pub const SESSION_LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// `last_used` and `last_seen` are written at most this often, so reads do not
/// turn into a stream of write transactions.
pub const TOUCH_INTERVAL_MS: i64 = 60_000;
pub const KEY_PREFIX: &str = "aur_";
const SECRET_BYTES: usize = 32;
/// How much of a key is kept in the clear for lists: `aur_` and four more.
const LISTED_PREFIX_CHARS: usize = 8;
const MAX_NAME_CHARS: usize = 100;
/// Anything longer cannot be a key or token we issued; skip hashing it.
const MAX_SECRET_CHARS: usize = 200;
const SIGN_IN_FAILED: &str = "invalid name or password";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Member,
    Agent,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Member => "member",
            Role::Agent => "agent",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        Some(match text {
            "owner" => Role::Owner,
            "member" => Role::Member,
            "agent" => Role::Agent,
            _ => return None,
        })
    }

    /// The value of `user.kind` that goes with the role.
    fn kind(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Owner | Role::Member => "human",
        }
    }
}

impl ToSql for Role {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for Role {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Role::parse(text).ok_or_else(|| FromSqlError::Other(format!("role {text}").into()))
    }
}

/// What a key may do: `Read` is GET and HEAD only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
        }
    }

    pub fn parse(text: &str) -> Option<Scope> {
        Some(match text {
            "read" => Scope::Read,
            "write" => Scope::Write,
            _ => return None,
        })
    }

    pub fn allows_writes(self) -> bool {
        self == Scope::Write
    }
}

impl ToSql for Scope {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for Scope {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Scope::parse(text).ok_or_else(|| FromSqlError::Other(format!("scope {text}").into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    /// `human` or `agent`.
    pub kind: String,
    pub role: Role,
    pub disabled: bool,
}

/// Who a request acts as. Handlers read it from the request extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principal {
    pub user_id: i64,
    pub role: Role,
    pub scope: Scope,
}

impl Principal {
    pub fn is_owner(&self) -> bool {
        self.role == Role::Owner
    }
}

/// A credential that was due for a bookkeeping write; see [`apply_touch`].
#[derive(Clone, PartialEq, Eq)]
pub enum Touch {
    Key(i64),
    /// The stored (hashed) session id.
    Session(String),
}

impl fmt::Debug for Touch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Touch::Key(id) => write!(f, "Touch::Key({id})"),
            Touch::Session(_) => f.write_str("Touch::Session([redacted])"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    pub principal: Principal,
    /// `Some` when `last_used` / `last_seen` is older than [`TOUCH_INTERVAL_MS`].
    pub touch: Option<Touch>,
}

// ---- secrets ----

fn random_hex() -> Result<String, StoreError> {
    let mut bytes = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| StoreError::Random)?;
    Ok(to_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

pub(crate) fn sha256_hex(secret: &str) -> String {
    to_hex(&Sha256::digest(secret.as_bytes()))
}

fn new_key_secret() -> Result<String, StoreError> {
    Ok(format!("{KEY_PREFIX}{}", random_hex()?))
}

fn listed_prefix(key: &str) -> String {
    key.chars().take(LISTED_PREFIX_CHARS).collect()
}

/// An Argon2id hash in PHC string form, ready to store.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordHash(String);

impl PasswordHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash([redacted])")
    }
}

pub fn validate_password(password: &str) -> Result<(), StoreError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(invalid(format!(
            "password must be at least {MIN_PASSWORD_CHARS} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(invalid(format!(
            "password must be at most {MAX_PASSWORD_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Checks the password rules, then hashes with Argon2id and a fresh salt.
/// Takes tens of milliseconds: call it from a blocking thread.
pub fn hash_password(password: &str) -> Result<PasswordHash, StoreError> {
    validate_password(password)?;
    hash_unchecked(password)
}

fn hash_unchecked(password: &str) -> Result<PasswordHash, StoreError> {
    let mut salt_bytes = [0_u8; 16];
    getrandom::fill(&mut salt_bytes).map_err(|_| StoreError::Random)?;
    let salt =
        SaltString::encode_b64(&salt_bytes).map_err(|err| StoreError::Hashing(err.to_string()))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|err| StoreError::Hashing(err.to_string()))?;
    Ok(PasswordHash(hash.to_string()))
}

fn verify_hash(hash: &str, password: &str) -> bool {
    match PhcHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Burned for unknown users so a missing account costs as much as a wrong password.
fn dummy_hash() -> Option<&'static str> {
    static DUMMY: OnceLock<Option<PasswordHash>> = OnceLock::new();
    DUMMY
        .get_or_init(|| hash_unchecked("not a real password").ok())
        .as_ref()
        .map(PasswordHash::as_str)
}

// ---- users ----

fn user_row(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        role: row.get(3)?,
        disabled: row.get(4)?,
    })
}

const USER_SELECT: &str = "SELECT id, name, kind, role, disabled FROM user";

fn clean_name(what: &str, name: &str) -> Result<String, StoreError> {
    let name = name.trim();
    if (1..=MAX_NAME_CHARS).contains(&name.chars().count()) && !name.chars().any(char::is_control) {
        Ok(name.to_string())
    } else {
        Err(invalid(format!(
            "{what} name must be 1-{MAX_NAME_CHARS} characters without control characters"
        )))
    }
}

pub fn get_user(conn: &Connection, id: i64) -> Result<User, StoreError> {
    conn.query_row(&format!("{USER_SELECT} WHERE id = ?1"), [id], user_row)
        .optional()?
        .ok_or_else(|| not_found("user"))
}

pub fn list_users(conn: &Connection) -> Result<Vec<User>, StoreError> {
    let mut stmt = conn.prepare(&format!("{USER_SELECT} ORDER BY id"))?;
    let rows = stmt.query_map([], user_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn owner_exists(conn: &Connection) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM user WHERE role = 'owner')",
        [],
        |row| row.get(0),
    )?)
}

fn insert_user(
    tx: &Transaction<'_>,
    now: i64,
    actor: Option<i64>,
    name: &str,
    role: Role,
    password: Option<&PasswordHash>,
) -> Result<User, StoreError> {
    tx.execute(
        "INSERT INTO user (name, kind, role, password_hash) VALUES (?1, ?2, ?3, ?4)",
        params![name, role.kind(), role, password.map(PasswordHash::as_str)],
    )
    .map_err(|err| refusal(err, || format!("a user named {name} already exists")))?;
    let user = get_user(tx, tx.last_insert_rowid())?;
    emit_as(
        tx,
        now,
        actor,
        None,
        "user",
        user.id,
        "user.created",
        &json!({ "name": user.name, "role": user.role, "kind": user.kind }),
    )?;
    Ok(user)
}

/// Creates the one owner account (first-run setup). `Conflict` once an owner
/// exists. Hash the password with [`hash_password`] first.
pub fn create_owner(
    tx: &Transaction<'_>,
    now: i64,
    name: &str,
    password: &PasswordHash,
) -> Result<User, StoreError> {
    let name = clean_name("user", name)?;
    if owner_exists(tx)? {
        return Err(conflict("the owner account already exists"));
    }
    insert_user(tx, now, None, &name, Role::Owner, Some(password))
}

/// Creates an agent user: no password, so it can only act through its keys.
/// Owner only.
pub fn create_agent_user(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    name: &str,
) -> Result<User, StoreError> {
    if !actor.is_owner() {
        return Err(forbidden("only the owner can create users"));
    }
    let name = clean_name("agent", name)?;
    insert_user(tx, now, Some(actor.user_id), &name, Role::Agent, None)
}

fn forbidden(reason: &str) -> StoreError {
    Rejection::Forbidden(reason.to_string()).into()
}

fn unauthorized(reason: &str) -> StoreError {
    Rejection::Unauthorized(reason.to_string()).into()
}

// ---- sign-in and sessions ----

/// What [`check_password`] needs; its `Debug` hides the hash.
pub struct Login {
    pub user: User,
    hash: Option<PasswordHash>,
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("user", &self.user)
            .field("hash", &"[redacted]")
            .finish()
    }
}

/// Finds the user a name belongs to (case-insensitively). `None` if there is none.
pub fn login_lookup(conn: &Connection, name: &str) -> Result<Option<Login>, StoreError> {
    let found = conn
        .query_row(
            "SELECT id, name, kind, role, disabled, password_hash FROM user
             WHERE name = ?1 COLLATE NOCASE",
            [name.trim()],
            |row| {
                let hash: Option<String> = row.get(5)?;
                Ok(Login {
                    user: user_row(row)?,
                    hash: hash.map(PasswordHash),
                })
            },
        )
        .optional()?;
    Ok(found)
}

/// The user if the password is right and the account may sign in. Unknown
/// users, agents (no password) and disabled users all spend one Argon2
/// verification and give the same `None` as a wrong password. CPU-heavy.
pub fn check_password(login: Option<&Login>, password: &str) -> Option<User> {
    if password.len() > MAX_PASSWORD_BYTES {
        return None;
    }
    let stored = login.and_then(|l| l.hash.as_ref().map(PasswordHash::as_str));
    let hash: Option<&str> = match stored {
        Some(hash) => Some(hash),
        None => dummy_hash(),
    };
    let matched = match hash {
        Some(hash) => verify_hash(hash, password),
        None => false,
    };
    match login {
        Some(login) if stored.is_some() && matched && !login.user.disabled => {
            Some(login.user.clone())
        }
        _ => None,
    }
}

/// The error for every failed sign-in, whatever the reason.
pub fn sign_in_failed() -> StoreError {
    unauthorized(SIGN_IN_FAILED)
}

/// A session token, shown once.
#[derive(Clone, PartialEq, Eq)]
pub struct NewSession {
    pub token: String,
    /// Unix milliseconds.
    pub expires: i64,
}

impl fmt::Debug for NewSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewSession")
            .field("token", &"[redacted]")
            .field("expires", &self.expires)
            .finish()
    }
}

/// Starts a session for `user_id`; also drops sessions that have expired.
pub fn open_session(
    tx: &Transaction<'_>,
    now: i64,
    user_id: i64,
) -> Result<NewSession, StoreError> {
    tx.execute("DELETE FROM session WHERE expires <= ?1", [now])?;
    let token = random_hex()?;
    let expires = now.saturating_add(SESSION_LIFETIME_MS);
    tx.execute(
        "INSERT INTO session (id, user_id, created, expires, last_seen) VALUES (?1, ?2, ?3, ?4, ?3)",
        params![sha256_hex(&token), user_id, now, expires],
    )?;
    Ok(NewSession { token, expires })
}

/// Look-up, password check and session in one transaction. This runs Argon2
/// on the calling thread; a server should use the split described in the
/// module docs so the writer is not blocked.
pub fn sign_in(
    tx: &Transaction<'_>,
    now: i64,
    name: &str,
    password: &str,
) -> Result<(NewSession, User), StoreError> {
    let login = login_lookup(tx, name)?;
    match check_password(login.as_ref(), password) {
        Some(user) => Ok((open_session(tx, now, user.id)?, user)),
        None => Err(sign_in_failed()),
    }
}

/// Ends the session. `false` if the token was not a live session.
pub fn sign_out(tx: &Transaction<'_>, token: &str) -> Result<bool, StoreError> {
    if token.len() > MAX_SECRET_CHARS {
        return Ok(false);
    }
    let removed = tx.execute("DELETE FROM session WHERE id = ?1", [sha256_hex(token)])?;
    Ok(removed > 0)
}

fn due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| now.saturating_sub(last) >= TOUCH_INTERVAL_MS)
}

/// Who a session token belongs to. Read-only; a session acts with write scope.
pub fn authenticate_session(
    conn: &Connection,
    now: i64,
    token: &str,
) -> Result<Option<Authenticated>, StoreError> {
    if token.is_empty() || token.len() > MAX_SECRET_CHARS {
        return Ok(None);
    }
    let id = sha256_hex(token);
    let found = conn
        .query_row(
            "SELECT s.user_id, u.role, s.last_seen
             FROM session s JOIN user u ON u.id = s.user_id
             WHERE s.id = ?1 AND s.expires > ?2 AND u.disabled = 0",
            params![id, now],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Role>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(found.map(|(user_id, role, last_seen)| Authenticated {
        principal: Principal {
            user_id,
            role,
            scope: Scope::Write,
        },
        touch: due(Some(last_seen), now).then_some(Touch::Session(id)),
    }))
}

/// Who an API key belongs to. Read-only. Revoked keys and keys of disabled
/// users find nothing.
pub fn authenticate_key(
    conn: &Connection,
    now: i64,
    key: &str,
) -> Result<Option<Authenticated>, StoreError> {
    if key.is_empty() || key.len() > MAX_SECRET_CHARS {
        return Ok(None);
    }
    let found = conn
        .query_row(
            "SELECT k.id, k.user_id, u.role, k.scope, k.last_used
             FROM api_key k JOIN user u ON u.id = k.user_id
             WHERE k.key_hash = ?1 AND k.revoked IS NULL AND u.disabled = 0",
            [sha256_hex(key)],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Role>(2)?,
                    row.get::<_, Scope>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;
    Ok(
        found.map(|(id, user_id, role, scope, last_used)| Authenticated {
            principal: Principal {
                user_id,
                role,
                scope,
            },
            touch: due(last_used, now).then_some(Touch::Key(id)),
        }),
    )
}

/// Records that a credential was used and, for a session, slides its expiry.
/// Safe to repeat: it only writes when the stored time is still older than
/// [`TOUCH_INTERVAL_MS`].
pub fn apply_touch(tx: &Transaction<'_>, now: i64, touch: &Touch) -> Result<(), StoreError> {
    let stale_before = now.saturating_sub(TOUCH_INTERVAL_MS);
    match touch {
        Touch::Key(id) => tx.execute(
            "UPDATE api_key SET last_used = ?1
             WHERE id = ?2 AND (last_used IS NULL OR last_used <= ?3)",
            params![now, id, stale_before],
        )?,
        Touch::Session(id) => tx.execute(
            "UPDATE session SET last_seen = ?1, expires = ?2
             WHERE id = ?3 AND last_seen <= ?4 AND expires > ?1",
            params![
                now,
                now.saturating_add(SESSION_LIFETIME_MS),
                id,
                stale_before
            ],
        )?,
    };
    Ok(())
}

// ---- API keys ----

/// A key as listed: never the secret, never the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKey {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    /// The first characters of the key, to recognise it in a list.
    pub prefix: String,
    pub scope: Scope,
    pub created: i64,
    pub last_used: Option<i64>,
    pub revoked: Option<i64>,
}

/// A key as created or rotated: the only time its secret is available.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct NewKey {
    #[serde(flatten)]
    pub info: ApiKey,
    pub key: String,
}

impl fmt::Debug for NewKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewKey")
            .field("info", &self.info)
            .field("key", &"[redacted]")
            .finish()
    }
}

fn key_row(row: &Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        id: row.get(0)?,
        user_id: row.get(1)?,
        name: row.get(2)?,
        prefix: row.get(3)?,
        scope: row.get(4)?,
        created: row.get(5)?,
        last_used: row.get(6)?,
        revoked: row.get(7)?,
    })
}

const KEY_SELECT: &str =
    "SELECT id, user_id, name, prefix, scope, created, last_used, revoked FROM api_key";

fn get_key(conn: &Connection, id: i64) -> Result<ApiKey, StoreError> {
    conn.query_row(&format!("{KEY_SELECT} WHERE id = ?1"), [id], key_row)
        .optional()?
        .ok_or_else(|| not_found("key"))
}

fn may_manage(actor: &Principal, user_id: i64) -> Result<(), StoreError> {
    if actor.is_owner() || actor.user_id == user_id {
        Ok(())
    } else {
        Err(forbidden("you can only manage your own keys"))
    }
}

/// Keys of `user_id` (the actor's own, or any user's for the owner), oldest first.
pub fn list_keys(
    conn: &Connection,
    actor: &Principal,
    user_id: i64,
) -> Result<Vec<ApiKey>, StoreError> {
    may_manage(actor, user_id)?;
    let mut stmt = conn.prepare(&format!("{KEY_SELECT} WHERE user_id = ?1 ORDER BY id"))?;
    let rows = stmt.query_map([user_id], key_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn create_key(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    user_id: i64,
    name: &str,
    scope: Scope,
) -> Result<NewKey, StoreError> {
    may_manage(actor, user_id)?;
    let name = clean_name("key", name)?;
    let target = get_user(tx, user_id)?;
    if target.disabled {
        return Err(conflict("the user is disabled"));
    }
    let key = new_key_secret()?;
    let prefix = listed_prefix(&key);
    tx.execute(
        "INSERT INTO api_key (user_id, name, prefix, key_hash, scope, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![user_id, name, prefix, sha256_hex(&key), scope, now],
    )?;
    let info = get_key(tx, tx.last_insert_rowid())?;
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "api_key",
        info.id,
        "key.created",
        &json!({ "user_id": user_id, "name": info.name, "prefix": info.prefix, "scope": scope }),
    )?;
    Ok(NewKey { info, key })
}

/// Replaces the secret of a live key in place: same row, name and scope. The
/// old secret stops working with the commit. `Conflict` for a revoked key.
pub fn rotate_key(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    id: i64,
) -> Result<NewKey, StoreError> {
    let old = get_key(tx, id)?;
    may_manage(actor, old.user_id)?;
    if old.revoked.is_some() {
        return Err(conflict("the key is revoked; create a new one"));
    }
    let key = new_key_secret()?;
    let prefix = listed_prefix(&key);
    tx.execute(
        "UPDATE api_key SET key_hash = ?1, prefix = ?2, last_used = NULL WHERE id = ?3",
        params![sha256_hex(&key), prefix, id],
    )?;
    let info = get_key(tx, id)?;
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "api_key",
        id,
        "key.rotated",
        &json!({
            "user_id": info.user_id, "name": info.name,
            "prefix": info.prefix, "previous_prefix": old.prefix, "scope": info.scope,
        }),
    )?;
    Ok(NewKey { info, key })
}

/// Stops the key working. Revoking a revoked key changes nothing.
pub fn revoke_key(
    tx: &Transaction<'_>,
    now: i64,
    actor: &Principal,
    id: i64,
) -> Result<ApiKey, StoreError> {
    let key = get_key(tx, id)?;
    may_manage(actor, key.user_id)?;
    if key.revoked.is_some() {
        return Ok(key);
    }
    tx.execute(
        "UPDATE api_key SET revoked = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    emit_as(
        tx,
        now,
        Some(actor.user_id),
        None,
        "api_key",
        id,
        "key.revoked",
        &json!({ "user_id": key.user_id, "name": key.name, "prefix": key.prefix }),
    )?;
    get_key(tx, id)
}
