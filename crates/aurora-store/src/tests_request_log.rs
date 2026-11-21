//! Request ids (TRK-43): claim, replay, mismatch, expiry, purge, atomicity.

use std::time::Duration;

use crate::request_log::{
    self, MAX_STORED_BODY, PURGE_BATCH, Replayed, Reply, RequestKey, Run, claim, purge_expired,
    record,
};
use crate::{Rejection, Store, StoreError};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    (dir, store)
}

fn key(principal: i64, id: &str, request: &str) -> RequestKey {
    RequestKey::new(principal, id, &[b"POST", request.as_bytes()]).unwrap()
}

/// A write that "creates" one project named `name`, the stand-in for a change.
async fn create(
    store: &Store,
    key: Option<RequestKey>,
    now: i64,
    name: &'static str,
) -> Result<Run, StoreError> {
    store
        .write(move |tx| {
            request_log::run(tx, key.as_ref(), now, WEEK, || {
                tx.execute(
                    "INSERT INTO project (key, name, created) VALUES (?1, ?1, ?2)",
                    rusqlite::params![name, now],
                )?;
                Ok(Reply::new(201, format!("{{\"key\":\"{name}\"}}")))
            })
        })
        .await
}

async fn count(store: &Store, table: &'static str) -> i64 {
    store
        .read(move |c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_repeated_id_returns_the_first_answer_and_changes_nothing() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "create A");

    let first = create(&store, Some(id.clone()), 1_000, "A").await.unwrap();
    assert_eq!(first, Run::Fresh(Reply::new(201, "{\"key\":\"A\"}".into())));
    let second = create(&store, Some(id.clone()), 2_000, "A").await.unwrap();
    assert_eq!(
        second,
        Run::Replayed(Replayed {
            status: 201,
            body: Some("{\"key\":\"A\"}".into())
        })
    );
    assert_eq!(count(&store, "project").await, 1);
    assert_eq!(count(&store, "request_log").await, 1);
    store.close().await;
}

#[tokio::test]
async fn without_an_id_every_call_runs() {
    let (_dir, store) = open();
    create(&store, None, 1, "A").await.unwrap();
    let again = create(&store, None, 2, "A").await;
    assert!(again.is_err(), "the second insert collides on the key");
    create(&store, None, 3, "B").await.unwrap();
    assert_eq!(count(&store, "project").await, 2);
    assert_eq!(count(&store, "request_log").await, 0);
    store.close().await;
}

#[tokio::test]
async fn the_same_id_for_a_different_request_is_refused_and_runs_nothing() {
    let (_dir, store) = open();
    create(&store, Some(key(1, "request-0001", "create A")), 1, "A")
        .await
        .unwrap();
    let refused = create(&store, Some(key(1, "request-0001", "create B")), 2, "B")
        .await
        .expect_err("a different request under the same id");
    assert!(
        matches!(&refused, StoreError::Rejected(Rejection::Invalid(m))
            if m == "request id was already used for a different request"),
        "{refused}"
    );
    assert_eq!(count(&store, "project").await, 1);
    store.close().await;
}

#[tokio::test]
async fn ids_are_scoped_to_the_principal() {
    let (_dir, store) = open();
    let same = "request-0001";
    create(&store, Some(key(1, same, "x")), 1, "A")
        .await
        .unwrap();
    let other = create(&store, Some(key(2, same, "x")), 2, "B")
        .await
        .unwrap();
    assert!(matches!(other, Run::Fresh(_)), "another user's id is new");
    assert_eq!(count(&store, "project").await, 2);
    store.close().await;
}

#[test]
fn ids_have_a_format() {
    let ok = |id: &str| RequestKey::new(1, id, &[]).is_ok();
    assert!(ok("abcdefgh"));
    assert!(ok("a.b_c:d-e"));
    assert!(ok(&"x".repeat(128)));
    assert!(!ok("short"), "7 characters or fewer");
    assert!(!ok(&"x".repeat(129)));
    assert!(!ok("has space 123"));
    assert!(!ok("slash/in/id-1"));
    assert!(!ok("unicod\u{e9}-id"));
    assert!(!ok(""));
}

#[test]
fn the_fingerprint_depends_on_every_part_and_on_where_the_parts_split() {
    let f = |parts: &[&[u8]]| RequestKey::new(1, "request-0001", parts).unwrap();
    let base = f(&[b"ab", b"c"]);
    assert_eq!(base.fingerprint, f(&[b"ab", b"c"]).fingerprint);
    assert_ne!(base.fingerprint, f(&[b"a", b"bc"]).fingerprint);
    assert_ne!(base.fingerprint, f(&[b"ab", b"d"]).fingerprint);
}

#[tokio::test]
async fn a_failed_attempt_leaves_no_row_so_the_retry_runs() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "create");
    let failed = store
        .write({
            let id = id.clone();
            move |tx| {
                request_log::run(tx, Some(&id), 1, WEEK, || {
                    tx.execute(
                        "INSERT INTO project (key, name, created) VALUES ('A', 'A', 1)",
                        [],
                    )?;
                    Err::<Reply, _>(Rejection::Conflict("nope".into()).into())
                })
            }
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(count(&store, "project").await, 0, "the change rolled back");
    assert_eq!(count(&store, "request_log").await, 0, "and left no id");

    let retry = create(&store, Some(id), 2, "A").await.unwrap();
    assert!(matches!(retry, Run::Fresh(_)));
    assert_eq!(count(&store, "project").await, 1);
    store.close().await;
}

#[tokio::test]
async fn an_error_status_is_not_recorded() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "create");
    let run = store
        .write({
            let id = id.clone();
            move |tx| {
                request_log::run(tx, Some(&id), 1, WEEK, || {
                    Ok(Reply::new(409, "{\"error\":\"x\"}".into()))
                })
            }
        })
        .await
        .unwrap();
    assert!(matches!(run, Run::Fresh(_)));
    assert_eq!(count(&store, "request_log").await, 0);
    store.close().await;
}

#[tokio::test]
async fn a_panic_between_the_change_and_the_commit_leaves_neither_row_nor_change() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "create");
    let panicked = store
        .write({
            let id = id.clone();
            move |tx| {
                let run = request_log::run(tx, Some(&id), 1, WEEK, || {
                    tx.execute(
                        "INSERT INTO project (key, name, created) VALUES ('A', 'A', 1)",
                        [],
                    )?;
                    Ok(Reply::new(201, "{}".into()))
                })?;
                // The record is written by now; the commit never happens.
                assert!(matches!(run, Run::Fresh(_)));
                assert_eq!(
                    tx.query_row("SELECT COUNT(*) FROM request_log", [], |r| r
                        .get::<_, i64>(0))?,
                    1
                );
                panic!("power loss");
                #[allow(unreachable_code)]
                Ok(())
            }
        })
        .await;
    assert!(matches!(panicked, Err(StoreError::Panicked)));
    assert_eq!(count(&store, "project").await, 0);
    assert_eq!(count(&store, "request_log").await, 0);

    let retry = create(&store, Some(id), 2, "A").await.unwrap();
    assert!(matches!(retry, Run::Fresh(_)), "the retry applies once");
    assert_eq!(count(&store, "project").await, 1);
    store.close().await;
}

#[tokio::test]
async fn the_row_and_the_change_commit_together() {
    let (_dir, store) = open();
    create(&store, Some(key(1, "request-0001", "create")), 1, "A")
        .await
        .unwrap();
    let both = store
        .read(|c| {
            Ok(c.query_row(
                "SELECT (SELECT COUNT(*) FROM project) = 1
                    AND (SELECT COUNT(*) FROM request_log) = 1",
                [],
                |r| r.get::<_, bool>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(both);
    store.close().await;
}

#[tokio::test]
async fn an_expired_id_is_a_new_request() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "create A");
    create(&store, Some(id.clone()), 1_000, "A").await.unwrap();

    let just_before = 1_000 + 7 * DAY_MS - 1;
    let early = store
        .write({
            let id = id.clone();
            move |tx| claim(tx, &id, just_before, WEEK)
        })
        .await
        .unwrap();
    assert!(
        early.is_some(),
        "still remembered a moment before the limit"
    );

    let expired = 1_000 + 7 * DAY_MS;
    let late = store
        .write({
            let id = id.clone();
            move |tx| claim(tx, &id, expired, WEEK)
        })
        .await
        .unwrap();
    assert_eq!(late, None, "after the retention the id is free");
    assert_eq!(count(&store, "request_log").await, 0, "and its row is gone");

    // Another request may now use the id.
    let other = key(1, "request-0001", "create B");
    let fresh = create(&store, Some(other), expired + 1, "B").await.unwrap();
    assert!(matches!(fresh, Run::Fresh(_)));
    store.close().await;
}

#[tokio::test]
async fn purge_deletes_expired_rows_in_batches_and_keeps_live_ones() {
    let (_dir, store) = open();
    let total = PURGE_BATCH * 2 + 200;
    store
        .write(move |tx| {
            for i in 0..total {
                let k = key(1, &format!("request-{i:05}"), "x");
                record(
                    tx,
                    &k,
                    i64::try_from(i).unwrap() + 1,
                    &Reply::new(200, "{}".into()),
                )?;
            }
            // Two rows that are not old at the time of the purge.
            for i in 0..2 {
                let k = key(1, &format!("fresh-{i:05}"), "x");
                record(tx, &k, 10 * DAY_MS, &Reply::new(200, "{}".into()))?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let now = 8 * DAY_MS;
    let mut batches = Vec::new();
    loop {
        let deleted = store
            .write(move |tx| purge_expired(tx, now, WEEK, PURGE_BATCH))
            .await
            .unwrap();
        batches.push(deleted);
        if deleted < PURGE_BATCH {
            break;
        }
    }
    assert_eq!(batches, [PURGE_BATCH, PURGE_BATCH, 200]);
    assert_eq!(count(&store, "request_log").await, 2);
    store.close().await;
}

#[tokio::test]
async fn the_store_purges_with_its_own_retention_and_clock_argument() {
    let (_dir, store) = open();
    assert_eq!(store.request_retention(), request_log::DEFAULT_RETENTION);
    store
        .write(|tx| {
            record(
                tx,
                &key(1, "request-0001", "x"),
                5,
                &Reply::new(200, "{}".into()),
            )
        })
        .await
        .unwrap();
    assert_eq!(store.purge_request_log(DAY_MS).await.unwrap(), 0);
    assert_eq!(store.purge_request_log(8 * DAY_MS).await.unwrap(), 1);
    assert_eq!(store.purge_request_log(8 * DAY_MS).await.unwrap(), 0);
    store.close().await;
}

#[tokio::test]
async fn retention_can_be_set_when_opening() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with(
        dir.path().join("test.db"),
        &crate::OpenOptions {
            request_retention: Some(DAY),
            ..crate::OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(store.request_retention(), DAY);
    store.close().await;
}

#[tokio::test]
async fn a_large_response_is_not_kept_but_the_id_still_blocks_a_second_apply() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "big");
    let big = "x".repeat(MAX_STORED_BODY + 1);
    let first = store
        .write({
            let id = id.clone();
            let big = big.clone();
            move |tx| request_log::run(tx, Some(&id), 1, WEEK, || Ok(Reply::new(200, big)))
        })
        .await
        .unwrap();
    assert!(matches!(first, Run::Fresh(r) if r.body.len() == MAX_STORED_BODY + 1));
    let again = store
        .write(move |tx| request_log::run(tx, Some(&id), 2, WEEK, || unreachable!()))
        .await
        .unwrap();
    assert_eq!(
        again,
        Run::Replayed(Replayed {
            status: 200,
            body: None
        })
    );

    let at_limit = "y".repeat(MAX_STORED_BODY);
    let kept = key(1, "request-0002", "limit");
    store
        .write({
            let kept = kept.clone();
            move |tx| request_log::run(tx, Some(&kept), 1, WEEK, || Ok(Reply::new(200, at_limit)))
        })
        .await
        .unwrap();
    let replay = store
        .write(move |tx| claim(tx, &kept, 2, WEEK))
        .await
        .unwrap();
    assert_eq!(replay.unwrap().body.map(|b| b.len()), Some(MAX_STORED_BODY));
    store.close().await;
}

#[tokio::test]
async fn a_body_with_a_secret_is_stored_as_its_replacement() {
    let (_dir, store) = open();
    let id = key(1, "request-0001", "secret");
    store
        .write({
            let id = id.clone();
            move |tx| {
                request_log::run(tx, Some(&id), 1, WEEK, || {
                    Ok(Reply {
                        status: 201,
                        body: "{\"key\":\"aur_secret\"}".into(),
                        stored_body: Some("{\"key\":null}".into()),
                    })
                })
            }
        })
        .await
        .unwrap();
    let stored: String = store
        .read(|c| Ok(c.query_row("SELECT response_body FROM request_log", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(stored, "{\"key\":null}");
    store.close().await;
}
