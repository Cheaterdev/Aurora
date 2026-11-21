//! Request ids: a write that carries one is applied at most once.
//!
//! The id is recorded in the same transaction as the change, together with
//! the response to replay, so a crash can leave both or neither and a retry
//! after a crash can never apply the change twice. [`run`] is the only way to
//! do it: it checks the id before the change runs and records it before the
//! caller's transaction commits.
//!
//! Only successful results are recorded (`2xx`): a failed attempt rolls back
//! and leaves no row, so a retry runs again. Ids are scoped to the principal.

use std::time::Duration;

use rusqlite::{OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::{Rejection, StoreError};

/// How long a recorded id keeps answering; a retry later than this is a new request.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A response body larger than this is not kept; replays say so instead.
pub const MAX_STORED_BODY: usize = 256 * 1024;
/// Rows removed by one [`purge_expired`] call.
pub const PURGE_BATCH: usize = 500;
pub const MIN_ID_LEN: usize = 8;
pub const MAX_ID_LEN: usize = 128;

/// Who sent which id for which request.
#[derive(Debug, Clone)]
pub struct RequestKey {
    pub(crate) principal: i64,
    pub(crate) id: String,
    pub(crate) fingerprint: String,
}

impl RequestKey {
    /// `parts` describe the request (for example method, path, body); the same
    /// id with other parts is refused. Each part is hashed with its length so
    /// that `["ab", "c"]` and `["a", "bc"]` differ.
    pub fn new(principal: i64, id: &str, parts: &[&[u8]]) -> Result<RequestKey, Rejection> {
        validate_id(id)?;
        let mut hash = Sha256::new();
        for part in parts {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        Ok(RequestKey {
            principal,
            id: id.to_string(),
            fingerprint: format!("{:x}", hash.finalize()),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

fn validate_id(id: &str) -> Result<(), Rejection> {
    let length_ok = (MIN_ID_LEN..=MAX_ID_LEN).contains(&id.len());
    let chars_ok = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if length_ok && chars_ok {
        Ok(())
    } else {
        Err(Rejection::Invalid(format!(
            "a request id must be {MIN_ID_LEN} to {MAX_ID_LEN} characters of A-Z a-z 0-9 . _ : -"
        )))
    }
}

/// What a write answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    /// Kept for replays instead of `body`, when `body` must not be stored
    /// (it carries a secret).
    pub stored_body: Option<String>,
}

impl Reply {
    pub fn new(status: u16, body: String) -> Reply {
        Reply {
            status,
            body,
            stored_body: None,
        }
    }
}

/// The recorded answer of an earlier request with the same id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replayed {
    pub status: u16,
    /// `None` when the original body was larger than [`MAX_STORED_BODY`].
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    /// The change ran now.
    Fresh(Reply),
    /// The id was used before: nothing ran and nothing changed.
    Replayed(Replayed),
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Runs `change` unless `key` was already used. Call it inside the write
/// transaction `tx`; the record is part of that transaction. Without a key it
/// just runs `change`. An error from `change` propagates, so the transaction
/// rolls back and nothing is recorded.
pub fn run(
    tx: &Transaction<'_>,
    key: Option<&RequestKey>,
    now: i64,
    retention: Duration,
    change: impl FnOnce() -> Result<Reply, StoreError>,
) -> Result<Run, StoreError> {
    let Some(key) = key else {
        return change().map(Run::Fresh);
    };
    if let Some(replayed) = claim(tx, key, now, retention)? {
        return Ok(Run::Replayed(replayed));
    }
    let reply = change()?;
    if is_success(reply.status) {
        record(tx, key, now, &reply)?;
    }
    Ok(Run::Fresh(reply))
}

fn retention_millis(retention: Duration) -> i64 {
    i64::try_from(retention.as_millis()).unwrap_or(i64::MAX)
}

/// The recorded answer for `key`, if there is a live one. An expired row that
/// the purge has not reached yet is deleted here and counts as no row.
pub fn claim(
    tx: &Transaction<'_>,
    key: &RequestKey,
    now: i64,
    retention: Duration,
) -> Result<Option<Replayed>, StoreError> {
    let found = tx
        .query_row(
            "SELECT fingerprint, status, response_body, created FROM request_log
             WHERE principal_user_id = ?1 AND request_id = ?2",
            params![key.principal, key.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((fingerprint, status, body, created)) = found else {
        return Ok(None);
    };
    if created <= now.saturating_sub(retention_millis(retention)) {
        tx.execute(
            "DELETE FROM request_log WHERE principal_user_id = ?1 AND request_id = ?2",
            params![key.principal, key.id],
        )?;
        return Ok(None);
    }
    if fingerprint != key.fingerprint {
        return Err(Rejection::Invalid(
            "request id was already used for a different request".into(),
        )
        .into());
    }
    Ok(Some(Replayed {
        status: u16::try_from(status).unwrap_or(500),
        body,
    }))
}

/// Records `reply` for `key`; the body is dropped when it is too large to keep.
pub fn record(
    tx: &Transaction<'_>,
    key: &RequestKey,
    now: i64,
    reply: &Reply,
) -> Result<(), StoreError> {
    let body = reply.stored_body.as_deref().unwrap_or(&reply.body);
    let body = (body.len() <= MAX_STORED_BODY).then_some(body);
    tx.execute(
        "INSERT INTO request_log
             (principal_user_id, request_id, fingerprint, status, response_body, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            key.principal,
            key.id,
            key.fingerprint,
            reply.status,
            body,
            now
        ],
    )?;
    Ok(())
}

/// Deletes up to `limit` rows older than `retention` and returns how many.
pub fn purge_expired(
    tx: &Transaction<'_>,
    now: i64,
    retention: Duration,
    limit: usize,
) -> Result<usize, StoreError> {
    let cutoff = now.saturating_sub(retention_millis(retention));
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    Ok(tx.execute(
        "DELETE FROM request_log WHERE rowid IN
             (SELECT rowid FROM request_log WHERE created <= ?1 ORDER BY created LIMIT ?2)",
        params![cutoff, limit],
    )?)
}
