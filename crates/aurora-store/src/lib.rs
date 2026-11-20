//! SQLite storage for Aurora: connection setup, forward-only migrations and a
//! single writer task. Every write goes through [`Store::write`]; reads use
//! their own read-only connections via [`Store::read`].

mod integrity;
mod location;
mod migrations;
#[cfg_attr(windows, path = "platform/windows.rs")]
#[cfg_attr(not(windows), path = "platform/other.rs")]
mod platform;

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction};
use tokio::sync::{mpsc, oneshot};

pub use integrity::IntegrityCheck;
pub use location::{synced_folder_reason, unsafe_reason};
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
    #[error(
        "the database {path} failed the {check} integrity check; refusing to start.\n{details}\n{hint}"
    )]
    IntegrityCheckFailed {
        path: PathBuf,
        check: &'static str,
        details: String,
        hint: String,
    },
    #[error(
        "refusing to use the database {path}: {reason}. SQLite needs local-disk locking and \
         flushing to be safe; move the database to a local, unsynced folder \
         (or pass --allow-unsafe-location for development only)"
    )]
    UnsafeLocation { path: PathBuf, reason: String },
    #[error("cannot write the run marker {path}: {source}")]
    Marker {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Settings for [`Store::open_with`].
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Where backups are expected; `None` means [`default_backup_dir`].
    pub backup_dir: Option<PathBuf>,
    /// Skip the network / synced-folder refusal (development and tests).
    pub allow_unsafe_location: bool,
}

/// What [`Store::open_with`] did, for logging and for later start-up steps.
#[derive(Debug, Clone)]
pub struct OpenReport {
    pub integrity_check: IntegrityCheck,
    /// The `.running` marker was left over from a run that did not close the store.
    pub unclean_shutdown: bool,
    /// The location refusal was skipped although the location looked unsafe.
    pub unsafe_location_overridden: Option<String>,
    pub backup_dir: PathBuf,
}

/// `<db file name>.backups` next to the database file.
pub fn default_backup_dir(db_path: &Path) -> PathBuf {
    sibling(db_path, ".backups")
}

/// The marker file that exists while a store is open: `<db file name>.running`.
pub fn marker_path(db_path: &Path) -> PathBuf {
    sibling(db_path, ".running")
}

fn sibling(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    db_path.with_file_name(name)
}

fn write_marker(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "pid {}", std::process::id())?;
    file.flush()?;
    file.sync_all()
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

struct Inner {
    path: PathBuf,
    schema_version: i64,
    report: OpenReport,
    marker: PathBuf,
    sender: Mutex<Option<mpsc::Sender<WriteJob>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

/// Cheap to clone; all clones share one writer.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl Store {
    /// [`Store::open_with`] with default options.
    pub fn open(path: impl AsRef<Path>) -> Result<Store, StoreError> {
        Store::open_with(path, &OpenOptions::default())
    }

    /// Checks the location, checks integrity, applies pending migrations and
    /// starts the writer thread, in that order. Blocks briefly; call before
    /// serving requests. Nothing is migrated or written to a database that
    /// fails its integrity check.
    pub fn open_with(path: impl AsRef<Path>, options: &OpenOptions) -> Result<Store, StoreError> {
        let path = path.as_ref().to_path_buf();
        let backup_dir = options
            .backup_dir
            .clone()
            .unwrap_or_else(|| default_backup_dir(&path));

        let mut unsafe_location_overridden = None;
        if let Some(reason) = location::unsafe_reason(&path) {
            if !options.allow_unsafe_location {
                return Err(StoreError::UnsafeLocation { path, reason });
            }
            tracing::warn!(
                path = %path.display(), %reason,
                "UNSAFE DATABASE LOCATION ALLOWED by override; committed data may be lost or corrupted"
            );
            unsafe_location_overridden = Some(reason);
        }

        let marker = marker_path(&path);
        let unclean_shutdown = marker.exists();
        let integrity_check = if unclean_shutdown {
            tracing::warn!(
                marker = %marker.display(),
                "previous run did not shut down cleanly; running the full integrity check"
            );
            IntegrityCheck::Full
        } else {
            IntegrityCheck::Quick
        };

        let conn = Connection::open(&path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        if let Err(details) = integrity::run(&conn, integrity_check) {
            return Err(StoreError::IntegrityCheckFailed {
                hint: integrity::recovery_hint(&backup_dir),
                path,
                check: integrity_check.name(),
                details,
            });
        }

        write_marker(&marker).map_err(|source| StoreError::Marker {
            path: marker.clone(),
            source,
        })?;
        // A marker this call created must not outlive a failed open, or the
        // next start would wrongly look like a crash recovery.
        let report = OpenReport {
            integrity_check,
            unclean_shutdown,
            unsafe_location_overridden,
            backup_dir,
        };
        let started = Store::start(path, conn, marker.clone(), report);
        if started.is_err() && !unclean_shutdown {
            let _ = std::fs::remove_file(&marker);
        }
        started
    }

    fn start(
        path: PathBuf,
        mut conn: Connection,
        marker: PathBuf,
        report: OpenReport,
    ) -> Result<Store, StoreError> {
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
                report,
                marker,
                sender: Mutex::new(Some(sender)),
                writer: Mutex::new(Some(writer)),
            }),
        })
    }

    pub fn schema_version(&self) -> i64 {
        self.inner.schema_version
    }

    pub fn open_report(&self) -> &OpenReport {
        &self.inner.report
    }

    /// Where backups are expected (and where TRK-40's will be written).
    pub fn backup_dir(&self) -> &Path {
        &self.inner.report.backup_dir
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
            // Only after the writer has exited and its connection closed.
            if let Err(err) = std::fs::remove_file(&self.inner.marker) {
                tracing::warn!(%err, "could not remove the run marker; next start will run the full check");
            }
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

    fn lenient() -> OpenOptions {
        OpenOptions {
            allow_unsafe_location: true,
            ..OpenOptions::default()
        }
    }

    async fn filled_db(path: &Path) {
        let store = Store::open(path).unwrap();
        store
            .write(|tx| {
                tx.execute_batch(
                    "CREATE TABLE filler (id INTEGER PRIMARY KEY, pad TEXT);
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
                     INSERT INTO filler (pad) SELECT hex(randomblob(64)) FROM n;",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        store.close().await;
    }

    fn corrupt(path: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() > 4096 * 4, "database too small to corrupt");
        let end = bytes.len().min(4096 + 16384);
        bytes[4096..end].fill(0xAB);
        std::fs::write(path, bytes).unwrap();
    }

    #[tokio::test]
    async fn fresh_database_passes_quick_check_and_leaves_no_marker_after_close() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let report = store.open_report();
        assert_eq!(report.integrity_check, IntegrityCheck::Quick);
        assert!(!report.unclean_shutdown);
        assert_eq!(report.backup_dir, default_backup_dir(&path));
        assert!(marker_path(&path).exists());
        store.close().await;
        assert!(!marker_path(&path).exists());
    }

    #[tokio::test]
    async fn leftover_marker_triggers_the_full_check() {
        let (_dir, path) = temp_db();
        filled_db(&path).await;
        std::fs::write(marker_path(&path), "pid 0").unwrap();

        let store = Store::open(&path).unwrap();
        assert!(store.open_report().unclean_shutdown);
        assert_eq!(store.open_report().integrity_check, IntegrityCheck::Full);
        assert!(marker_path(&path).exists(), "marker is recreated");
        store.close().await;
        assert!(!marker_path(&path).exists());

        let store = Store::open(&path).unwrap();
        assert_eq!(store.open_report().integrity_check, IntegrityCheck::Quick);
        store.close().await;
    }

    #[tokio::test]
    async fn corrupted_database_is_refused_without_backup() {
        let (_dir, path) = temp_db();
        filled_db(&path).await;
        corrupt(&path);

        let err = Store::open(&path).err().expect("open must fail");
        assert!(
            matches!(err, StoreError::IntegrityCheckFailed { check: "quick", .. }),
            "unexpected error: {err}"
        );
        let text = err.to_string();
        assert!(text.contains("refusing to start"), "{text}");
        assert!(text.contains("No backup was found"), "{text}");
        let expected_dir = default_backup_dir(&path);
        assert!(text.contains(&expected_dir.display().to_string()), "{text}");
        assert!(!marker_path(&path).exists(), "no marker for a refused db");
    }

    #[tokio::test]
    async fn corrupted_database_after_unclean_shutdown_fails_the_full_check() {
        let (_dir, path) = temp_db();
        filled_db(&path).await;
        corrupt(&path);
        std::fs::write(marker_path(&path), "pid 0").unwrap();
        let err = Store::open(&path).err().expect("open must fail");
        assert!(
            matches!(err, StoreError::IntegrityCheckFailed { check: "full", .. }),
            "unexpected error: {err}"
        );
        assert!(marker_path(&path).exists(), "evidence of the crash stays");
    }

    #[tokio::test]
    async fn hint_names_the_newest_backup() {
        let (dir, path) = temp_db();
        filled_db(&path).await;
        corrupt(&path);

        let backups = dir.path().join("my-backups");
        std::fs::create_dir(&backups).unwrap();
        let old = backups.join("old.db");
        let new = backups.join("new.db");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        std::fs::write(backups.join("newest-but-not-db.txt"), "x").unwrap();
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        let options = OpenOptions {
            backup_dir: Some(backups),
            ..OpenOptions::default()
        };
        let err = Store::open_with(&path, &options)
            .err()
            .expect("open must fail");
        let text = err.to_string();
        assert!(text.contains("new.db"), "{text}");
        assert!(!text.contains("old.db"), "{text}");
        assert!(!text.contains("No backup was found"), "{text}");
    }

    #[test]
    fn synced_folder_names_are_refused() {
        for bad in [
            "C:/Users/me/OneDrive/aurora.db",
            "C:/Users/me/OneDrive - Contoso/data/aurora.db",
            "/home/me/Dropbox/aurora.db",
            "D:/Google Drive/x/aurora.db",
            "/Users/me/iCloud Drive/aurora.db",
        ] {
            assert!(synced_folder_reason(Path::new(bad)).is_some(), "{bad}");
        }
        for good in ["C:/data/aurora.db", "/var/lib/aurora/aurora.db"] {
            assert!(synced_folder_reason(Path::new(good)).is_none(), "{good}");
        }
    }

    #[tokio::test]
    async fn unsafe_location_is_refused_unless_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let synced = dir.path().join("OneDrive");
        std::fs::create_dir(&synced).unwrap();
        let path = synced.join("test.db");

        let err = Store::open(&path).err().expect("open must fail");
        assert!(matches!(err, StoreError::UnsafeLocation { .. }), "{err}");
        assert!(err.to_string().contains("cloud-synced"), "{err}");

        let store = Store::open_with(&path, &lenient()).unwrap();
        assert!(store.open_report().unsafe_location_overridden.is_some());
        store.close().await;
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_are_network_locations() {
        assert!(unsafe_reason(Path::new(r"\\server\share\aurora.db")).is_some());
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
