//! `aurora check`: runs the invariant checks of `aurora_store::invariants` on
//! a database file and reports them. Read-only: it opens the file the way a
//! reader does, never migrates, never takes the write lock and never touches
//! the run marker.

use std::path::Path;
use std::process::ExitCode;

use aurora_store::invariants::{self, Report};
use aurora_store::rusqlite::{Error as SqliteError, ErrorCode};
use aurora_store::{StoreError, marker_path};
use serde_json::json;

use crate::config::CheckConfig;

pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_CANNOT_OPEN: u8 = 2;

/// Prints the report (or the reason it could not be made) and returns the exit
/// code: 0 every check passed, 1 a check failed, 2 the database could not be opened.
pub fn run(config: &CheckConfig) -> ExitCode {
    let (output, code) = execute(&config.db_path, config.json);
    if code == EXIT_CANNOT_OPEN {
        eprintln!("{output}");
    } else {
        println!("{output}");
    }
    ExitCode::from(code)
}

/// The text to print and the exit code, separate from printing so tests can see both.
pub fn execute(path: &Path, json: bool) -> (String, u8) {
    let running = marker_path(path).exists();
    match invariants::check_file(path) {
        Ok(report) => {
            let code = if report.ok() { EXIT_OK } else { EXIT_FAILED };
            let text = if json {
                render_json(path, &report, running)
            } else {
                render_human(path, &report, running)
            };
            (text, code)
        }
        Err(err) => (open_failure(path, &err, json), EXIT_CANNOT_OPEN),
    }
}

fn open_failure(path: &Path, err: &StoreError, json: bool) -> String {
    let locked = matches!(
        err,
        StoreError::Sqlite(SqliteError::SqliteFailure(failure, _))
            if matches!(failure.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    );
    let mut message = format!("cannot open {}: {err}", path.display());
    if locked {
        message.push_str(
            "\nthe database is locked by another process, probably a running server; \
             wait and try again, or check a copy of the file",
        );
    }
    if json {
        json!({ "database": path.display().to_string(), "ok": false, "error": message }).to_string()
    } else {
        message
    }
}

fn render_json(path: &Path, report: &Report, running: bool) -> String {
    let value = json!({
        "database": path.display().to_string(),
        "ok": report.ok(),
        "schema_version": report.schema_version,
        "run_marker_present": running,
        "checks": report.checks,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

fn render_human(path: &Path, report: &Report, running: bool) -> String {
    let mut out = format!(
        "Checking {} (schema version {})\n",
        path.display(),
        report.schema_version
    );
    if running {
        out.push_str(&format!(
            "note: {} exists, so a server is running or stopped without closing the database;\n      \
             the checks read one consistent snapshot and change nothing\n",
            marker_path(path).display()
        ));
    }
    let width = report
        .checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);
    for check in &report.checks {
        let status = if check.ok { "ok  " } else { "FAIL" };
        if check.ok {
            out.push_str(&format!(
                "{status}  {:width$}  {}\n",
                check.name, check.details
            ));
        } else {
            out.push_str(&format!("{status}  {}\n", check.name));
            for line in check.details.lines() {
                out.push_str(&format!("        {line}\n"));
            }
        }
    }
    let failed = report.failed().len();
    if failed == 0 {
        out.push_str(&format!("All {} checks passed.", report.checks.len()));
    } else {
        out.push_str(&format!(
            "{failed} of {} checks FAILED.",
            report.checks.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_database_is_exit_code_two_in_both_formats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let (text, code) = execute(&path, false);
        assert_eq!(code, EXIT_CANNOT_OPEN);
        assert!(text.contains("cannot open"), "{text}");
        let (text, code) = execute(&path, true);
        assert_eq!(code, EXIT_CANNOT_OPEN);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap().contains("cannot open"));
    }
}
