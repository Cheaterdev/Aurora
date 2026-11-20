//! Start-up integrity check and the recovery hint shown when it fails.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::Connection;

const MAX_DETAIL_LINES: usize = 5;
const MAX_DETAIL_CHARS: usize = 600;

/// Which SQLite check ran at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityCheck {
    /// `PRAGMA quick_check`: always run.
    Quick,
    /// `PRAGMA integrity_check`: run after an unclean shutdown.
    Full,
}

impl IntegrityCheck {
    fn pragma(self) -> &'static str {
        match self {
            IntegrityCheck::Quick => "quick_check",
            IntegrityCheck::Full => "integrity_check",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IntegrityCheck::Quick => "quick",
            IntegrityCheck::Full => "full",
        }
    }
}

/// `Ok(())` when the database is sound, `Err(details)` otherwise. An error
/// from SQLite itself (for example a damaged header) counts as a failure.
pub fn run(conn: &Connection, check: IntegrityCheck) -> Result<(), String> {
    let lines = collect(conn, check).map_err(|e| e.to_string())?;
    if lines.len() == 1 && lines[0] == "ok" {
        return Ok(());
    }
    Err(cap(&lines))
}

fn collect(conn: &Connection, check: IntegrityCheck) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA {}", check.pragma()))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn cap(lines: &[String]) -> String {
    let mut text = lines
        .iter()
        .take(MAX_DETAIL_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_DETAIL_LINES {
        text.push_str(&format!(
            "\n... and {} more lines",
            lines.len() - MAX_DETAIL_LINES
        ));
    }
    if text.chars().count() > MAX_DETAIL_CHARS {
        text = text.chars().take(MAX_DETAIL_CHARS).collect();
        text.push_str("...");
    }
    text
}

/// The newest `*.db` file in `backup_dir` by modification time.
pub fn newest_backup(backup_dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(backup_dir).ok()?.flatten() {
        let path = entry.path();
        let is_db = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
        if !is_db {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

pub fn recovery_hint(backup_dir: &Path) -> String {
    match newest_backup(backup_dir) {
        Some(backup) => format!(
            "Restore from the newest backup: {} (stop Aurora, replace the database file with it, delete any -wal/-shm files next to it).",
            backup.display()
        ),
        None => format!(
            "No backup was found; backups are expected in {}.",
            backup_dir.display()
        ),
    }
}
