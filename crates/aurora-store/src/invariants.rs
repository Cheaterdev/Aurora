//! The invariant checker: read-only checks that a database, live or offline,
//! is internally consistent. [`check`] runs every entry of [`INVARIANTS`] on
//! one connection inside one read snapshot; [`check_file`] opens a file for
//! that without migrating or writing anything. `aurora check` is a thin
//! wrapper, and the crash tests run it after every kill.
//!
//! # Adding a check
//!
//! Append an [`Invariant`] to [`INVARIANTS`]: a stable name, one line saying
//! what it proves, and a function that returns the violations it found (an
//! empty list passes). The name is part of the command's output, so never
//! rename one. Every check needs a test in `tests_invariants.rs` that corrupts
//! a copy of a database and proves the check notices.
//!
//! The slot `runs.no_orphans` exists but is vacuous until the runner has a
//! table (TRK-15); that task fills it in, TRK-18 and TRK-45 extend it.
//!
//! # What a clean report does not prove
//!
//! It is about structure and agreement, not about meaning. Not covered: the
//! text of descriptions and comments (the events do not carry them), the order
//! of columns and of issues within a column, timestamps, users, API keys and
//! sessions, and the contents of the backups folder. See `replay.rs` for the
//! exact model that events and tables are compared on.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::events::{self, EventFilter};
use crate::integrity::{self, IntegrityCheck};
use crate::{StoreError, migrations, replay, search};

const MAX_LINES: usize = 10;
const PAGE: i64 = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub name: &'static str,
    pub ok: bool,
    /// A short summary when `ok`; the violations, a few lines, when not.
    pub details: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The schema version recorded in the database, 0 when there is none.
    pub schema_version: i64,
    pub checks: Vec<CheckResult>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }

    pub fn failed(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|check| !check.ok).collect()
    }

    pub fn get(&self, name: &str) -> Option<&CheckResult> {
        self.checks.iter().find(|check| check.name == name)
    }
}

/// What one check found.
pub struct Finding {
    pub summary: String,
    pub violations: Vec<String>,
}

impl Finding {
    fn of(summary: impl Into<String>, violations: Vec<String>) -> Result<Finding, StoreError> {
        Ok(Finding {
            summary: summary.into(),
            violations,
        })
    }
}

pub struct Invariant {
    /// Stable; shown by `aurora check` and relied on by tests.
    pub name: &'static str,
    /// One line: what a pass proves.
    pub proves: &'static str,
    /// Needs the tables of the current schema; not run against an older one.
    pub needs_current_schema: bool,
    pub run: fn(&Connection) -> Result<Finding, StoreError>,
}

/// Every check, in the order they are reported. Later tasks add theirs here.
pub const INVARIANTS: &[Invariant] = &[
    Invariant {
        name: "sqlite.integrity_check",
        proves: "SQLite's own full integrity check passes: pages, indexes and rows are well formed",
        needs_current_schema: false,
        run: sqlite_integrity,
    },
    Invariant {
        name: "sqlite.foreign_key_check",
        proves: "no row refers to a row that does not exist",
        needs_current_schema: false,
        run: sqlite_foreign_keys,
    },
    Invariant {
        name: "schema.version",
        proves: "the recorded migrations are 1..n without holes and n is the schema this build expects",
        needs_current_schema: false,
        run: schema_version,
    },
    Invariant {
        name: "events.ids",
        proves: "event ids are 1..n without gaps and the AUTOINCREMENT counter is not behind them, so no event was lost or an id reused",
        needs_current_schema: true,
        run: event_ids,
    },
    Invariant {
        name: "events.immutable_triggers",
        proves: "the triggers that make the event log append-only are still installed",
        needs_current_schema: true,
        run: event_triggers,
    },
    Invariant {
        name: "events.state_agreement",
        proves: "replaying the event log reproduces the tables: no change without its event, no event without its change",
        needs_current_schema: true,
        run: state_agreement,
    },
    Invariant {
        name: "projects.issue_numbering",
        proves: "each project's next issue number is above every issue number, and numbers are unique",
        needs_current_schema: true,
        run: issue_numbering,
    },
    Invariant {
        name: "documents.revision_chain",
        proves: "each document's revisions are numbered 1..current and the newest one is the document's text",
        needs_current_schema: true,
        run: revision_chain,
    },
    Invariant {
        name: "references.same_project",
        proves: "parents, types, columns, initiatives, links and document links never cross projects",
        needs_current_schema: true,
        run: same_project,
    },
    Invariant {
        name: "comments.threads",
        proves: "every reply hangs off a top-level comment of the same issue",
        needs_current_schema: true,
        run: comment_threads,
    },
    Invariant {
        name: "consumers.cursors",
        proves: "no consumer cursor is beyond the newest event",
        needs_current_schema: true,
        run: consumer_cursors,
    },
    Invariant {
        name: "request_log.rows",
        proves: "every remembered request id belongs to an existing user and a successful response",
        needs_current_schema: true,
        run: request_log_rows,
    },
    Invariant {
        name: "search.index",
        proves: "the full-text index holds exactly the issues, comments and documents that should be in it",
        needs_current_schema: true,
        run: search_index,
    },
    Invariant {
        name: "runs.no_orphans",
        proves: "no run is left active without a process that owns it (VACUOUS until the runner exists, TRK-15)",
        needs_current_schema: true,
        run: runs_no_orphans,
    },
];

/// Runs every invariant on `conn`, which is not changed. The checks see one
/// snapshot, so the report is consistent even while a server is writing.
pub fn check(conn: &Connection) -> Report {
    let snapshot = conn.execute_batch("SAVEPOINT aurora_check").is_ok();
    let schema = recorded_schema(conn);
    let current = schema == Ok(migrations::latest_version());
    let checks = INVARIANTS
        .iter()
        .map(|invariant| {
            if invariant.needs_current_schema && !current {
                return CheckResult {
                    name: invariant.name,
                    ok: false,
                    details: format!(
                        "not run: the database schema is {}, this build expects {}",
                        schema
                            .as_ref()
                            .map_or_else(|why| why.clone(), |v| v.to_string()),
                        migrations::latest_version()
                    ),
                };
            }
            match (invariant.run)(conn) {
                Ok(finding) if finding.violations.is_empty() => CheckResult {
                    name: invariant.name,
                    ok: true,
                    details: finding.summary,
                },
                Ok(finding) => CheckResult {
                    name: invariant.name,
                    ok: false,
                    details: cap(&finding.violations),
                },
                Err(err) => CheckResult {
                    name: invariant.name,
                    ok: false,
                    details: format!("the check could not run: {err}"),
                },
            }
        })
        .collect();
    if snapshot {
        let _ = conn.execute_batch("RELEASE aurora_check");
    }
    Report {
        schema_version: schema.unwrap_or(0),
        checks,
    }
}

/// Opens `path` read-only (no migration, no marker, no write) and runs
/// [`check`]. Works on an offline file, on a copy, and on the database of a
/// running server (a WAL database can be read while it is written). An error
/// means the file could not be opened or is not an SQLite database at all.
pub fn check_file(path: &Path) -> Result<Report, StoreError> {
    if !path.is_file() {
        return Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!("{} is not a file", path.display())),
        )));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok(check(&conn))
}

fn cap(lines: &[String]) -> String {
    let mut text = lines
        .iter()
        .take(MAX_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_LINES {
        text.push_str(&format!("\n... and {} more", lines.len() - MAX_LINES));
    }
    text
}

/// The recorded schema version, or why it could not be read.
fn recorded_schema(conn: &Connection) -> Result<i64, String> {
    let has_table: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !has_table {
        return Ok(0);
    }
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

fn lines(
    conn: &Connection,
    sql: &str,
    describe: fn(&rusqlite::Row<'_>) -> rusqlite::Result<String>,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], describe)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn count(conn: &Connection, sql: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

// ---- SQLite ----

fn sqlite_integrity(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = match integrity::run(conn, IntegrityCheck::Full) {
        Ok(()) => Vec::new(),
        Err(details) => details.lines().map(String::from).collect(),
    };
    Finding::of("integrity_check: ok", violations)
}

fn sqlite_foreign_keys(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(conn, "PRAGMA foreign_key_check", |r| {
        Ok(format!(
            "{} row {} refers to a missing row in {}",
            r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1)?
                .map_or("?".into(), |id| id.to_string()),
            r.get::<_, String>(2)?
        ))
    })?;
    Finding::of("foreign_key_check: no violations", violations)
}

fn schema_version(conn: &Connection) -> Result<Finding, StoreError> {
    let latest = migrations::latest_version();
    let mut violations = Vec::new();
    let mut stmt = conn.prepare("SELECT version FROM schema_version ORDER BY version")?;
    let versions = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for (index, version) in versions.iter().enumerate() {
        let expected = i64::try_from(index + 1).unwrap_or(i64::MAX);
        if *version != expected {
            violations.push(format!("migration {expected} is missing (found {version})"));
            break;
        }
    }
    let newest = versions.last().copied().unwrap_or(0);
    if newest < latest {
        violations.push(format!(
            "the database is at schema {newest}, this build expects {latest}: start aurora to migrate it"
        ));
    } else if newest > latest {
        violations.push(format!(
            "the database is at schema {newest}, newer than this build ({latest})"
        ));
    }
    Finding::of(format!("schema version {newest}"), violations)
}

// ---- events ----

fn event_ids(conn: &Connection) -> Result<Finding, StoreError> {
    let mut violations = Vec::new();
    let total = count(conn, "SELECT COUNT(*) FROM event")?;
    let newest = count(conn, "SELECT COALESCE(MAX(id), 0) FROM event")?;
    let oldest = count(conn, "SELECT COALESCE(MIN(id), 0) FROM event")?;
    // AUTOINCREMENT updates its counter inside the writing transaction, so a
    // rollback gives the ids back and a committed log has no holes. A hole
    // therefore means an event was lost; ids only ever grow, so "strictly
    // increasing" is the id order itself and cannot be broken without
    // breaking this.
    if total != newest || (total > 0 && oldest != 1) {
        let missing = lines(
            conn,
            "SELECT 'event ' || (e.id + 1) || ' is missing between events ' || e.id || ' and ' ||
                    (SELECT MIN(n.id) FROM event n WHERE n.id > e.id)
             FROM event e
             WHERE e.id < (SELECT MAX(id) FROM event)
               AND NOT EXISTS (SELECT 1 FROM event n WHERE n.id = e.id + 1)
             ORDER BY e.id LIMIT 5",
            |r| r.get(0),
        )?;
        violations.extend(missing);
        if oldest > 1 {
            violations.push(format!("the log starts at event {oldest}, not at 1"));
        }
        violations.push(format!("{total} events but the newest id is {newest}"));
    }
    let sequence: Option<i64> = conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'event'",
            [],
            |r| r.get(0),
        )
        .ok();
    match sequence {
        Some(seq) if seq < newest => violations.push(format!(
            "the event id counter is {seq}, behind the newest event {newest}"
        )),
        None if total > 0 => violations.push("the event id counter is missing".to_string()),
        _ => {}
    }
    Finding::of(format!("{total} events, ids 1..{newest}"), violations)
}

fn event_triggers(conn: &Connection) -> Result<Finding, StoreError> {
    let mut violations = Vec::new();
    for name in ["event_no_update", "event_no_delete", "event_no_replace"] {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?1 AND tbl_name = 'event'",
                [name],
                |r| r.get(0),
            )
            .ok();
        match sql {
            None => violations.push(format!("trigger {name} is missing")),
            Some(sql) if !sql.contains("RAISE(ABORT") => {
                violations.push(format!("trigger {name} no longer aborts"));
            }
            Some(_) => {}
        }
    }
    Finding::of("3 triggers installed", violations)
}

fn state_agreement(conn: &Connection) -> Result<Finding, StoreError> {
    let mut replayer = replay::Replayer::default();
    let mut after = 0;
    let mut total = 0_i64;
    loop {
        let page = events::read_events(conn, after, Some(PAGE), &EventFilter::default())?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.id;
        for event in &page {
            total += 1;
            if let Err(why) = replayer.push(event) {
                return Finding::of("", vec![why]);
            }
        }
    }
    let from_log = replayer.finish();
    let from_tables = replay::table_snapshot(conn)?;
    Finding::of(
        format!(
            "{total} events reproduce {} projects, {} issues, {} comments, {} documents",
            from_tables.projects.len(),
            from_tables.issues.len(),
            from_tables.comments.len(),
            from_tables.docs.len()
        ),
        replay::diff(&from_log, &from_tables),
    )
}

// ---- tables ----

fn issue_numbering(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(
        conn,
        "SELECT 'project ' || p.key || ': next number ' || p.next_issue_number || ' is not above the highest issue number ' || m.max_number
         FROM project p JOIN (SELECT project_id, MAX(number) AS max_number FROM issue GROUP BY project_id) m
              ON m.project_id = p.id
         WHERE p.next_issue_number <= m.max_number
         UNION ALL
         SELECT 'project ' || p.key || ': issue number ' || i.number || ' is used ' || COUNT(*) || ' times'
         FROM issue i JOIN project p ON p.id = i.project_id
         GROUP BY i.project_id, i.number HAVING COUNT(*) > 1
         UNION ALL
         SELECT 'issue ' || id || ' has the number ' || number FROM issue WHERE number < 1",
        |r| r.get(0),
    )?;
    let projects = count(conn, "SELECT COUNT(*) FROM project")?;
    Finding::of(format!("{projects} projects"), violations)
}

fn revision_chain(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(
        conn,
        "SELECT 'document ' || d.id || ' is at revision ' || d.revision || ' but has ' || COUNT(r.id) ||
                ' revisions numbered ' || COALESCE(MIN(r.number), 0) || '..' || COALESCE(MAX(r.number), 0)
         FROM document d LEFT JOIN document_revision r ON r.document_id = d.id
         GROUP BY d.id
         HAVING NOT (COUNT(r.id) = d.revision AND MIN(r.number) = 1 AND MAX(r.number) = d.revision)
         UNION ALL
         SELECT 'document ' || d.id || ' differs from its revision ' || d.revision
         FROM document d JOIN document_revision r ON r.document_id = d.id AND r.number = d.revision
         WHERE r.title IS NOT d.title OR r.body IS NOT d.body",
        |r| r.get(0),
    )?;
    let documents = count(conn, "SELECT COUNT(*) FROM document")?;
    Finding::of(format!("{documents} documents"), violations)
}

fn same_project(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(
        conn,
        "SELECT 'issue ' || i.id || ' has a parent in another project' FROM issue i JOIN issue p ON p.id = i.parent_id WHERE p.project_id != i.project_id
         UNION ALL
         SELECT 'issue ' || i.id || ' has a type of another project' FROM issue i JOIN issue_type t ON t.id = i.type_id WHERE t.project_id != i.project_id
         UNION ALL
         SELECT 'issue ' || i.id || ' is in a column of another project' FROM issue i JOIN \"column\" c ON c.id = i.column_id WHERE c.project_id != i.project_id
         UNION ALL
         SELECT 'issue ' || i.id || ' belongs to an initiative of another project' FROM issue i JOIN initiative n ON n.id = i.initiative_id WHERE n.project_id != i.project_id
         UNION ALL
         SELECT 'link ' || l.id || ' joins issues of different projects' FROM issue_link l JOIN issue f ON f.id = l.from_issue_id JOIN issue t ON t.id = l.to_issue_id WHERE f.project_id != t.project_id
         UNION ALL
         SELECT 'document ' || d.id || ' has a parent in another project' FROM document d JOIN document p ON p.id = d.parent_id WHERE p.project_id != d.project_id
         UNION ALL
         SELECT 'document link ' || l.id || ' leaves the project of its document' FROM document_link l JOIN document d ON d.id = l.document_id
           LEFT JOIN issue i ON i.id = l.issue_id LEFT JOIN initiative n ON n.id = l.initiative_id
           WHERE COALESCE(i.project_id, n.project_id) != d.project_id",
        |r| r.get(0),
    )?;
    Finding::of("no reference crosses a project", violations)
}

fn comment_threads(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(
        conn,
        "SELECT 'comment ' || c.id || ' replies to comment ' || p.id || ', which is on another issue or is itself a reply'
         FROM comment c JOIN comment p ON p.id = c.parent_id
         WHERE p.issue_id != c.issue_id OR p.parent_id IS NOT NULL",
        |r| r.get(0),
    )?;
    let comments = count(conn, "SELECT COUNT(*) FROM comment")?;
    Finding::of(format!("{comments} comments"), violations)
}

fn consumer_cursors(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(
        conn,
        "SELECT 'consumer ' || name || ' is at event ' || last_event_id || ', beyond the newest event ' ||
                (SELECT COALESCE(MAX(id), 0) FROM event)
         FROM consumer_cursor WHERE last_event_id > (SELECT COALESCE(MAX(id), 0) FROM event)",
        |r| r.get(0),
    )?;
    let consumers = count(conn, "SELECT COUNT(*) FROM consumer_cursor")?;
    Finding::of(format!("{consumers} consumers"), violations)
}

fn request_log_rows(conn: &Connection) -> Result<Finding, StoreError> {
    let violations = lines(
        conn,
        "SELECT 'request id ' || request_id || ' belongs to the missing user ' || principal_user_id
         FROM request_log WHERE principal_user_id NOT IN (SELECT id FROM user)
         UNION ALL
         SELECT 'request id ' || request_id || ' recorded the status ' || status
         FROM request_log WHERE status NOT BETWEEN 200 AND 299",
        |r| r.get(0),
    )?;
    let rows = count(conn, "SELECT COUNT(*) FROM request_log")?;
    Finding::of(format!("{rows} remembered request ids"), violations)
}

fn search_index(conn: &Connection) -> Result<Finding, StoreError> {
    let found = search::search_check(conn)?;
    let violations = if found.is_consistent() {
        Vec::new()
    } else {
        vec![format!(
            "indexable {:?} but indexed {:?}",
            found.indexable, found.indexed
        )]
    };
    Finding::of(
        format!(
            "{} issues, {} comments, {} documents indexed",
            found.indexed.issues, found.indexed.comments, found.indexed.documents
        ),
        violations,
    )
}

/// Vacuous until TRK-15 adds the runner's tables: there is nothing that could
/// be orphaned yet. The runner tasks replace this body with the real query
/// (a run marked active whose owning process is gone) and keep the name.
fn runs_no_orphans(_conn: &Connection) -> Result<Finding, StoreError> {
    Finding::of("vacuous: there are no runs yet (TRK-15)", Vec::new())
}
