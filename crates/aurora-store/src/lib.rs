//! SQLite storage for Aurora: connection setup, forward-only migrations and a
//! single writer task. Every write goes through [`Store::write`]; reads use
//! their own read-only connections via [`Store::read`].

mod migrations;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction};
use tokio::sync::{mpsc, oneshot};

pub use migrations::latest_version;
pub use rusqlite;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_QUEUE_DEPTH: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "database schema version {found} is newer than this binary supports ({supported}); \
         refusing to start, use a newer build"
    )]
    DatabaseTooNew { found: i64, supported: i64 },
    #[error("journal mode is {0:?}, WAL could not be enabled")]
    WalUnavailable(String),
    #[error("could not start the writer thread: {0}")]
    WriterSpawn(#[source] std::io::Error),
    #[error("a store operation panicked")]
    Panicked,
    #[error("the store is closed")]
    Closed,
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

struct Inner {
    path: PathBuf,
    schema_version: i64,
    sender: Mutex<Option<mpsc::Sender<WriteJob>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

/// Cheap to clone; all clones share one writer.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl Store {
    /// Opens (creating if needed) the database, applies pending migrations and
    /// starts the writer thread. Blocks briefly; call before serving requests.
    pub fn open(path: impl AsRef<Path>) -> Result<Store, StoreError> {
        let path = path.as_ref().to_path_buf();
        let mut conn = Connection::open(&path)?;
        apply_pragmas(&conn, true)?;
        let schema_version = migrations::migrate(&mut conn)?;

        let (sender, mut receiver) = mpsc::channel::<WriteJob>(WRITE_QUEUE_DEPTH);
        let writer = std::thread::Builder::new()
            .name("aurora-writer".into())
            .spawn(move || {
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut conn);
                }
            })
            .map_err(StoreError::WriterSpawn)?;

        Ok(Store {
            inner: Arc::new(Inner {
                path,
                schema_version,
                sender: Mutex::new(Some(sender)),
                writer: Mutex::new(Some(writer)),
            }),
        })
    }

    pub fn schema_version(&self) -> i64 {
        self.inner.schema_version
    }

    /// Runs `f` in a transaction on the writer thread. The transaction commits
    /// if `f` returns `Ok` and rolls back on `Err` or panic.
    pub async fn write<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let sender = lock(&self.inner.sender)
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        let (reply, response) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn| {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let tx = conn.transaction()?;
                let value = f(&tx)?;
                tx.commit()?;
                Ok(value)
            }))
            .unwrap_or(Err(StoreError::Panicked));
            let _ = reply.send(outcome);
        });
        sender.send(job).await.map_err(|_| StoreError::Closed)?;
        response.await.map_err(|_| StoreError::Closed)?
    }

    /// Runs `f` on a fresh read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let path = self.inner.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            apply_pragmas(&conn, false)?;
            f(&conn)
        })
        .await
        .map_err(|_| StoreError::Panicked)?
    }

    /// Stops accepting writes, lets the writer finish what is already queued
    /// and waits for it to exit.
    pub async fn close(&self) {
        drop(lock(&self.inner.sender).take());
        let writer = lock(&self.inner.writer).take();
        if let Some(writer) = writer {
            let _ = tokio::task::spawn_blocking(move || writer.join()).await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn apply_pragmas(conn: &Connection, writable: bool) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Read-only connections cannot switch the mode; the writer already did.
    if writable {
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::WalUnavailable(mode));
        }
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        (dir, path)
    }

    fn applied_rows(path: &Path) -> Vec<(i64, String, i64)> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT version, name, applied_at FROM schema_version ORDER BY version")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[tokio::test]
    async fn fresh_database_is_migrated_to_latest() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version(), latest_version());
        store.close().await;

        let rows = applied_rows(&path);
        assert_eq!(rows.len() as i64, latest_version());
        assert_eq!(rows.last().unwrap().0, latest_version());

        let conn = Connection::open(&path).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for expected in ["user", "project", "column", "issue_type", "issue", "event"] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}"
            );
        }
    }

    #[tokio::test]
    async fn pragmas_are_applied() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let (mode, fk, sync) = store
            .write(|tx| {
                Ok((
                    tx.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
                    tx.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))?,
                    tx.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(fk, 1);
        assert_eq!(sync, 2);
        let (fk, sync) = store
            .read(|c| {
                Ok((
                    c.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))?,
                    c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((fk, sync), (1, 2));
        store.close().await;
    }

    #[tokio::test]
    async fn reopening_applies_nothing() {
        let (_dir, path) = temp_db();
        Store::open(&path).unwrap().close().await;
        let before = applied_rows(&path);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version(), latest_version());
        store.close().await;

        assert_eq!(applied_rows(&path), before);
    }

    #[tokio::test]
    async fn newer_database_is_refused() {
        let (_dir, path) = temp_db();
        Store::open(&path).unwrap().close().await;
        let newer = latest_version() + 1;
        Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO schema_version (version, name, applied_at) VALUES (?1, 'future', 0)",
                [newer],
            )
            .unwrap();

        let err = Store::open(&path).err().expect("open must fail");
        assert!(
            matches!(err, StoreError::DatabaseTooNew { found, supported }
                if found == newer && supported == latest_version()),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("newer than this binary"));
    }

    #[tokio::test]
    async fn write_is_visible_to_reader() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO project (key, name, created) VALUES ('TRK', 'Tracker', 1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let name: String = store
            .read(|c| {
                Ok(
                    c.query_row("SELECT name FROM project WHERE key = 'TRK'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(name, "Tracker");
        store.close().await;
    }

    #[tokio::test]
    async fn failed_or_panicking_write_rolls_back_and_writer_survives() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let insert = |tx: &Transaction<'_>| {
            tx.execute(
                "INSERT INTO project (key, name, created) VALUES ('A', 'A', 1)",
                [],
            )
        };
        let failed = store
            .write(move |tx| {
                insert(tx)?;
                Err::<(), _>(StoreError::Closed)
            })
            .await;
        assert!(matches!(failed, Err(StoreError::Closed)));
        let panicked = store
            .write(move |tx| {
                insert(tx)?;
                panic!("boom");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;
        assert!(matches!(panicked, Err(StoreError::Panicked)));

        let count: i64 = store
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 0);
        store.write(move |tx| Ok(insert(tx)?)).await.unwrap();
        store.close().await;
    }

    #[tokio::test]
    async fn event_rows_cannot_be_updated_or_deleted() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO event (time, entity_kind, entity_id, kind, payload_json)
                     VALUES (1, 'issue', 1, 'created', '{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let update = store
            .write(|tx| Ok(tx.execute("UPDATE event SET kind = 'x'", [])?))
            .await;
        let delete = store
            .write(|tx| Ok(tx.execute("DELETE FROM event", [])?))
            .await;
        for result in [update, delete] {
            let err = result.expect_err("mutation must be rejected");
            assert!(err.to_string().contains("append-only"), "unexpected: {err}");
        }
        let count: i64 = store
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 1);
        store.close().await;
    }

    #[tokio::test]
    async fn write_after_close_fails() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        store.close().await;
        let result = store.write(|_| Ok(())).await;
        assert!(matches!(result, Err(StoreError::Closed)));
    }
}
