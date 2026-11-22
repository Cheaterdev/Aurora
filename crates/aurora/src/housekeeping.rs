//! What start-up reconciliation leaves for a person (TRK-45): working copies
//! the database does not account for, working copies it expects that are gone,
//! and what was found of each leftover process. **This module only reports.**
//! It never deletes a folder, never kills a process and never starts a run.
//!
//! The report is computed when asked (a directory listing and two small
//! queries), from the run rows and `<data folder>/worktrees/`. Paths are
//! always relative to the data folder; an agent never sees any of this (the
//! endpoint is owner only).

use std::path::Path;

use aurora_store::auth::Principal;
use aurora_store::runs::WorktreeState;
use aurora_store::rusqlite::Connection;
use aurora_store::{Rejection, Store, StoreError, now_millis};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Serialize;

use crate::routes::ApiResult;
use crate::transcript;

/// At most this many entries of each list are returned.
const LIMIT: usize = 500;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LeftoverProcess {
    pub run: i64,
    pub pid: Option<i64>,
    /// What start-up found: `killed leftover process <pid>`, `process already
    /// gone`, `pid reused, left alone`, `pid unknown`, ...
    pub note: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UnknownCopy {
    /// Relative to the data folder, with forward slashes: `worktrees/<name>`.
    pub name: String,
    /// `folder` or `file`.
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KeptCopy {
    pub run: i64,
    pub branch: Option<String>,
    /// `created` (a run is using it, or it was never settled) or `kept`.
    pub state: WorktreeState,
    /// Whether `worktrees/<run id>` is there.
    pub folder_exists: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    pub leftover_processes: Vec<LeftoverProcess>,
    pub unknown_working_copies: Vec<UnknownCopy>,
    pub kept_working_copies: Vec<KeptCopy>,
    pub checked_at: i64,
}

/// Runs only ever have a working copy under this folder of the data folder.
pub fn worktrees_dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("worktrees")
}

/// Builds the report. Blocking (reads a directory): call it from a database
/// read closure or a blocking thread.
pub fn scan(conn: &Connection, data_dir: &Path) -> Result<Report, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, pid, reaped FROM run WHERE reaped IS NOT NULL ORDER BY id DESC LIMIT ?1",
    )?;
    let leftover_processes = stmt
        .query_map([LIMIT as i64], |row| {
            Ok(LeftoverProcess {
                run: row.get(0)?,
                pid: row.get(1)?,
                note: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let root = worktrees_dir(data_dir);
    let mut stmt = conn.prepare(
        "SELECT id, branch, worktree_state FROM run
         WHERE worktree_state IN ('created', 'kept') ORDER BY id DESC LIMIT ?1",
    )?;
    let kept_working_copies = stmt
        .query_map([LIMIT as i64], |row| {
            let id: i64 = row.get(0)?;
            Ok(KeptCopy {
                run: id,
                branch: row.get(1)?,
                state: row.get(2)?,
                folder_exists: root.join(id.to_string()).is_dir(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut known = std::collections::HashSet::new();
    let mut stmt =
        conn.prepare("SELECT id FROM run WHERE worktree_state IN ('created', 'kept')")?;
    for id in stmt.query_map([], |row| row.get::<_, i64>(0))? {
        known.insert(id?.to_string());
    }
    let mut unknown_working_copies = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if known.contains(&name) {
                continue;
            }
            let kind = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                "folder"
            } else {
                "file"
            };
            unknown_working_copies.push(UnknownCopy {
                name: format!("worktrees/{name}"),
                kind,
            });
        }
    }
    unknown_working_copies.sort_by(|a, b| a.name.cmp(&b.name));
    unknown_working_copies.truncate(LIMIT);

    Ok(Report {
        leftover_processes,
        unknown_working_copies,
        kept_working_copies,
        checked_at: now_millis(),
    })
}

/// One log line per finding, at start-up (after reconciliation). Nothing is
/// changed. A problem reading is logged and ignored: this is a courtesy.
pub async fn log_at_startup(store: &Store, data_dir: &Path) {
    let dir = data_dir.to_path_buf();
    let report = store.read(move |c| scan(c, &dir)).await;
    let report = match report {
        Ok(report) => report,
        Err(err) => {
            tracing::warn!(error = %err, "could not look for unknown working copies");
            return;
        }
    };
    for copy in &report.unknown_working_copies {
        tracing::warn!(
            entry = %copy.name,
            "unknown working copy: not matched by any run; left alone (see docs/operations.md, Housekeeping)"
        );
    }
    for copy in report
        .kept_working_copies
        .iter()
        .filter(|c| !c.folder_exists)
    {
        tracing::warn!(
            run = copy.run,
            state = copy.state.as_str(),
            "a working copy the database expects is missing from the data folder"
        );
    }
    let kept = report
        .kept_working_copies
        .iter()
        .filter(|c| c.folder_exists)
        .count();
    tracing::info!(
        unknown = report.unknown_working_copies.len(),
        kept,
        "working copies checked; nothing was deleted"
    );
}

/// `GET /api/v1/admin/housekeeping`. Owner only.
pub(crate) async fn get_report(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Response> {
    if !principal.is_owner() {
        return Err(StoreError::from(Rejection::Forbidden(
            "only the owner can read the housekeeping report".into(),
        ))
        .into());
    }
    let dir = transcript::data_dir(&store);
    let report = store.read(move |c| scan(c, &dir)).await?;
    Ok(Json(report).into_response())
}
