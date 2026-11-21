//! Runs: one start of an agent process on one issue (TRK-15).
//!
//! The store only keeps the record; the process itself is the runner's
//! business (`crates/aurora/src/runner`). A run goes
//! `starting -> running -> succeeded | failed | interrupted | canceled`; a run
//! that never got a process (the template is invalid, the command is missing)
//! goes `starting -> failed`, and one whose first message cannot even be
//! rendered is created already `failed`.
//!
//! # Rules the database and this module enforce
//!
//! * **One active run per issue**: a partial unique index over
//!   `status IN ('starting', 'running')`. Creating a second is `Conflict`.
//! * **One run per (trigger event, agent)**: the runner is told about an event
//!   at least once, so a redelivered event finds its run
//!   ([`Created::duplicate`]) instead of making another.
//! * **The per-run key dies with the run.** [`create_run`] mints a write key
//!   for the agent's user named `run-<id>` in the same transaction as the row,
//!   and [`finish_run`] revokes it in the transaction that ends the run, so a
//!   run that is active always has a live key and a finished one never has.
//! * **`boot_id` names the server lifetime that owns an active run.**
//!   [`reconcile_startup`] marks every active run of another boot `interrupted`
//!   before the runner consumes anything.
//!
//! # Secrets
//!
//! The first message and the captured output can hold secrets (a person's
//! edited message, whatever the process printed). They live in the row only:
//! [`Run`] has neither, [`RunDetail`] has both and is for owner and members.
//! **No event ever carries them.**
//!
//! # Events
//!
//! Entity kind `run`, the run's id as entity id, the issue's project on the
//! event, written in the transaction of the change. The actor is the person who
//! asked (a manual start, a cancel) and absent for the system:
//!
//! * `run.created` `{agent_id, issue: {number, identifier}, reason,
//!   trigger_event_id, template_source}`
//! * `run.started` `{pid}`
//! * `run.finished` `{status, exit_code, error?}`
//!
//! The per-run key's own `key.created` and `key.revoked` events (actor: the
//! agent's user) are written in the same transactions.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::Serialize;
use serde_json::json;

use crate::StoreError;
use crate::agents;
use crate::auth::{self, Principal, Role, Scope};
use crate::projects::{conflict, emit_as, invalid, not_found, refusal};

/// The last this many bytes of output are kept.
pub const OUTPUT_TAIL_BYTES: usize = 64 * 1024;
/// A first message may be this long when a person edits it.
pub const MAX_MESSAGE_CHARS: usize = 200_000;
const MAX_ERROR_CHARS: usize = 500;
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Starting,
    Running,
    Succeeded,
    Failed,
    Interrupted,
    Canceled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Starting => "starting",
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Canceled => "canceled",
        }
    }

    pub fn parse(text: &str) -> Option<RunStatus> {
        Some(match text {
            "starting" => RunStatus::Starting,
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "interrupted" => RunStatus::Interrupted,
            "canceled" => RunStatus::Canceled,
            _ => return None,
        })
    }

    pub fn is_active(self) -> bool {
        matches!(self, RunStatus::Starting | RunStatus::Running)
    }
}

impl ToSql for RunStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for RunStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        RunStatus::parse(text).ok_or_else(|| FromSqlError::Other(format!("status {text}").into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunReason {
    Assigned,
    Mentioned,
    Manual,
}

impl RunReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RunReason::Assigned => "assigned",
            RunReason::Mentioned => "mentioned",
            RunReason::Manual => "manual",
        }
    }

    pub fn parse(text: &str) -> Option<RunReason> {
        Some(match text {
            "assigned" => RunReason::Assigned,
            "mentioned" => RunReason::Mentioned,
            "manual" => RunReason::Manual,
            _ => return None,
        })
    }
}

impl ToSql for RunReason {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for RunReason {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        RunReason::parse(text).ok_or_else(|| FromSqlError::Other(format!("reason {text}").into()))
    }
}

/// A run without its message and output: safe to show to anyone who may see
/// the issue, an agent's key included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: i64,
    pub agent_id: i64,
    /// The agent user's name.
    pub agent: String,
    /// The project key.
    pub project: String,
    /// `KEY-n`.
    pub issue: String,
    pub issue_number: i64,
    pub reason: RunReason,
    pub status: RunStatus,
    pub trigger_event_id: Option<i64>,
    pub pid: Option<i64>,
    pub template_source: Option<String>,
    pub template_id: Option<i64>,
    /// A person wrote or changed the first message.
    pub edited: bool,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    /// Output older than the last [`OUTPUT_TAIL_BYTES`] was dropped.
    pub output_truncated: bool,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    pub created: i64,
    pub created_by: Option<i64>,
}

/// A run with the sensitive text. Owner and members only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunDetail {
    #[serde(flatten)]
    pub run: Run,
    /// The exact first message the process got.
    pub message: String,
    /// The last bytes of stdout and stderr together.
    pub output_tail: String,
}

const RUN_SELECT: &str = "SELECT r.id, r.agent_id, u.name, p.key, i.number, r.reason, r.status,
        r.trigger_event_id, r.pid, r.template_source, r.template_id, r.edited, r.exit_code,
        r.error, r.output_truncated, r.started, r.finished, r.created, r.created_by,
        r.message, r.output_tail
     FROM run r
     JOIN agent a ON a.id = r.agent_id
     JOIN user u ON u.id = a.user_id
     JOIN issue i ON i.id = r.issue_id
     JOIN project p ON p.id = r.project_id";

fn run_row(row: &Row<'_>) -> rusqlite::Result<RunDetail> {
    let key: String = row.get(3)?;
    let number: i64 = row.get(4)?;
    Ok(RunDetail {
        run: Run {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            agent: row.get(2)?,
            issue: format!("{key}-{number}"),
            project: key,
            issue_number: number,
            reason: row.get(5)?,
            status: row.get(6)?,
            trigger_event_id: row.get(7)?,
            pid: row.get(8)?,
            template_source: row.get(9)?,
            template_id: row.get(10)?,
            edited: row.get(11)?,
            exit_code: row.get(12)?,
            error: row.get(13)?,
            output_truncated: row.get(14)?,
            started: row.get(15)?,
            finished: row.get(16)?,
            created: row.get(17)?,
            created_by: row.get(18)?,
        },
        message: row.get(19)?,
        output_tail: row.get(20)?,
    })
}

pub fn get_run(conn: &Connection, id: i64) -> Result<RunDetail, StoreError> {
    conn.query_row(&format!("{RUN_SELECT} WHERE r.id = ?1"), [id], run_row)
        .optional()?
        .ok_or_else(|| not_found("run"))
}

/// A random id for one server lifetime; see the module docs.
pub fn new_boot_id() -> Result<String, StoreError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| StoreError::Random)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// What [`create_run`] needs. The first message is rendered by the caller
/// (`prompt::render_prompt`, or a person's own text) before the transaction.
#[derive(Debug, Clone)]
pub struct NewRun {
    pub agent_id: i64,
    /// The issue is `project` + `number`.
    pub project: String,
    pub number: i64,
    pub reason: RunReason,
    pub trigger_event_id: Option<i64>,
    /// The first message, or `None` when it is rendered in the same
    /// transaction with the run's id (see [`set_message`]).
    pub message: Option<String>,
    pub template_source: Option<String>,
    pub template_id: Option<i64>,
    pub edited: bool,
    pub boot_id: String,
    /// When set the run is recorded as `failed` with this reason and never
    /// gets a process or a key (the first message could not be rendered).
    pub failed: Option<String>,
}

/// The result of [`create_run`].
pub struct Created {
    pub run: Run,
    /// The per-run key's secret, shown only here; `None` for a duplicate and
    /// for a run created `failed`.
    pub key: Option<String>,
    /// The (event, agent) pair already had a run: nothing was written.
    pub duplicate: bool,
}

impl std::fmt::Debug for Created {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Created")
            .field("run", &self.run)
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("duplicate", &self.duplicate)
            .finish()
    }
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Records a new run and mints its key. `actor` is the user who asked, `None`
/// for the system.
///
/// Errors: the agent does not exist (not found); the agent or its user is
/// disabled, the issue does not exist or its project is not on the agent's
/// allow-list, the message is empty or over [`MAX_MESSAGE_CHARS`] (invalid);
/// the issue already has an active run (conflict). A `(trigger event, agent)`
/// pair that already has a run is not an error: see [`Created::duplicate`].
pub fn create_run(
    tx: &Transaction<'_>,
    now: i64,
    actor: Option<i64>,
    new: NewRun,
) -> Result<Created, StoreError> {
    if let Some(event) = new.trigger_event_id {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM run WHERE trigger_event_id = ?1 AND agent_id = ?2",
                params![event, new.agent_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(Created {
                run: get_run(tx, id)?.run,
                key: None,
                duplicate: true,
            });
        }
    }
    let agent = agents::agent_config(tx, new.agent_id)?;
    if !agent.enabled {
        return Err(invalid(format!("agent {} is disabled", agent.name)));
    }
    let user = auth::get_user(tx, agent.user_id)?;
    if user.disabled {
        return Err(invalid(format!(
            "the user of agent {} is disabled",
            agent.name
        )));
    }
    if !agent.projects.contains(&new.project) {
        return Err(invalid(format!(
            "agent {} may not access project {}",
            agent.name, new.project
        )));
    }
    let issue: Option<(i64, i64)> = tx
        .query_row(
            "SELECT i.id, p.id FROM issue i JOIN project p ON p.id = i.project_id
             WHERE p.key = ?1 AND i.number = ?2",
            params![new.project, new.number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((issue_id, project_id)) = issue else {
        return Err(invalid(format!(
            "unknown issue {}-{}",
            new.project, new.number
        )));
    };
    let identifier = format!("{}-{}", new.project, new.number);
    if new.failed.is_none()
        && let Some(message) = &new.message
    {
        check_message(message)?;
    }
    let active: Option<i64> = tx
        .query_row(
            "SELECT id FROM run WHERE issue_id = ?1 AND status IN ('starting', 'running')",
            [issue_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(active) = active {
        return Err(conflict(format!(
            "{identifier} already has an active run (run {active})"
        )));
    }

    let (status, finished, error) = match &new.failed {
        Some(error) => (
            RunStatus::Failed,
            Some(now),
            Some(clip(error, MAX_ERROR_CHARS)),
        ),
        None => (RunStatus::Starting, None, None),
    };
    tx.execute(
        "INSERT INTO run (agent_id, issue_id, project_id, trigger_event_id, reason, status,
                          boot_id, message, template_source, template_id, edited, error,
                          finished, created, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            agent.id,
            issue_id,
            project_id,
            new.trigger_event_id,
            new.reason,
            status,
            new.boot_id,
            new.message.as_deref().unwrap_or(""),
            new.template_source,
            new.template_id,
            new.edited,
            error,
            finished,
            now,
            actor
        ],
    )
    .map_err(|err| {
        refusal(err, || {
            format!("{identifier} already has an active run, or this event already started one")
        })
    })?;
    let id = tx.last_insert_rowid();

    let mut secret = None;
    if new.failed.is_none() {
        let principal = Principal {
            user_id: agent.user_id,
            role: Role::Agent,
            scope: Scope::Write,
        };
        let key = auth::create_key(
            tx,
            now,
            &principal,
            agent.user_id,
            &format!("run-{id}"),
            Scope::Write,
        )?;
        tx.execute(
            "UPDATE run SET key_id = ?1 WHERE id = ?2",
            params![key.info.id, id],
        )?;
        secret = Some(key.key);
    }
    emit_as(
        tx,
        now,
        actor,
        Some(project_id),
        "run",
        id,
        "run.created",
        &json!({
            "agent_id": agent.id,
            "issue": { "number": new.number, "identifier": identifier },
            "reason": new.reason,
            "trigger_event_id": new.trigger_event_id,
            "template_source": new.template_source,
        }),
    )?;
    if let Some(error) = &error {
        emit_as(
            tx,
            now,
            actor,
            Some(project_id),
            "run",
            id,
            "run.finished",
            &json!({ "status": "failed", "exit_code": null, "error": error }),
        )?;
    }
    Ok(Created {
        run: get_run(tx, id)?.run,
        key: secret,
        duplicate: false,
    })
}

fn check_message(message: &str) -> Result<(), StoreError> {
    if message.trim().is_empty() || message.contains('\0') {
        return Err(invalid("the message must not be empty or contain NUL"));
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(invalid(format!(
            "the message must be at most {MAX_MESSAGE_CHARS} characters"
        )));
    }
    Ok(())
}

/// Stores the rendered first message of a run that is still `starting`, in
/// the transaction that created it (the text needs the run's id).
pub fn set_message(
    tx: &Transaction<'_>,
    run_id: i64,
    message: &str,
    template_source: &str,
    template_id: Option<i64>,
) -> Result<(), StoreError> {
    check_message(message)?;
    let changed = tx.execute(
        "UPDATE run SET message = ?1, template_source = ?2, template_id = ?3
         WHERE id = ?4 AND status = 'starting'",
        params![message, template_source, template_id, run_id],
    )?;
    if changed == 0 {
        return Err(conflict("the run is not starting"));
    }
    Ok(())
}

/// `starting` to `running` once the process exists. `Conflict` when the run is
/// no longer starting (it was canceled meanwhile): the caller must stop the
/// process.
pub fn mark_running(
    tx: &Transaction<'_>,
    now: i64,
    run_id: i64,
    pid: i64,
) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE run SET status = 'running', pid = ?1, started = ?2
         WHERE id = ?3 AND status = 'starting'",
        params![pid, now, run_id],
    )?;
    if changed == 0 {
        get_run(tx, run_id)?;
        return Err(conflict("the run is not starting"));
    }
    let project_id: i64 =
        tx.query_row("SELECT project_id FROM run WHERE id = ?1", [run_id], |r| {
            r.get(0)
        })?;
    emit_as(
        tx,
        now,
        None,
        Some(project_id),
        "run",
        run_id,
        "run.started",
        &json!({ "pid": pid }),
    )
}

/// How a run ended.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `succeeded`, `failed`, `interrupted` or `canceled`.
    pub status: RunStatus,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    pub output_tail: String,
    pub truncated: bool,
}

impl Outcome {
    pub fn new(status: RunStatus) -> Outcome {
        Outcome {
            status,
            exit_code: None,
            error: None,
            output_tail: String::new(),
            truncated: false,
        }
    }

    pub fn error(mut self, text: impl Into<String>) -> Outcome {
        self.error = Some(text.into());
        self
    }
}

/// Ends an active run and revokes its key in the same transaction. Returns
/// `false`, changing nothing, when the run was already finished, so a cancel
/// racing an exit is harmless. `actor` is the person who canceled, if any.
pub fn finish_run(
    tx: &Transaction<'_>,
    now: i64,
    actor: Option<i64>,
    run_id: i64,
    outcome: Outcome,
) -> Result<bool, StoreError> {
    if outcome.status.is_active() {
        return Err(invalid("a run can only finish with a final status"));
    }
    let found: Option<(RunStatus, Option<i64>, i64, i64)> = tx
        .query_row(
            "SELECT r.status, r.key_id, a.user_id, r.project_id
             FROM run r JOIN agent a ON a.id = r.agent_id WHERE r.id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((status, key_id, user_id, project_id)) = found else {
        return Err(not_found("run"));
    };
    if !status.is_active() {
        return Ok(false);
    }
    let error = outcome.error.as_deref().map(|e| clip(e, MAX_ERROR_CHARS));
    tx.execute(
        "UPDATE run SET status = ?1, exit_code = ?2, error = ?3, output_tail = ?4,
                        output_truncated = ?5, finished = ?6
         WHERE id = ?7",
        params![
            outcome.status,
            outcome.exit_code,
            error,
            outcome.output_tail,
            outcome.truncated,
            now,
            run_id
        ],
    )?;
    if let Some(key_id) = key_id {
        let principal = Principal {
            user_id,
            role: Role::Agent,
            scope: Scope::Write,
        };
        auth::revoke_key(tx, now, &principal, key_id)?;
    }
    let mut payload = json!({ "status": outcome.status, "exit_code": outcome.exit_code });
    if let (Some(error), Some(map)) = (&error, payload.as_object_mut()) {
        map.insert("error".into(), json!(error));
    }
    emit_as(
        tx,
        now,
        actor,
        Some(project_id),
        "run",
        run_id,
        "run.finished",
        &payload,
    )?;
    Ok(true)
}

/// A run canceled by a person is finished (and its key revoked) before the
/// process has been stopped; once it has, the supervisor adds what it
/// captured. Changes nothing for any other run, and nothing already captured.
pub fn record_output(
    tx: &Transaction<'_>,
    run_id: i64,
    exit_code: Option<i64>,
    output_tail: &str,
    truncated: bool,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE run SET output_tail = ?1, output_truncated = ?2, exit_code = ?3
         WHERE id = ?4 AND status = 'canceled' AND output_tail = '' AND exit_code IS NULL",
        params![output_tail, truncated, exit_code, run_id],
    )?;
    Ok(())
}

/// Selects what [`list_runs`] returns.
#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    /// The issue, as project key and number.
    pub issue: Option<(String, i64)>,
    pub agent_id: Option<i64>,
    pub status: Option<RunStatus>,
    /// Only runs of these projects (`None`: no limit, an empty list: none).
    pub allowed_projects: Option<Vec<String>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Newest first. `limit` defaults to 50 and is clamped to 1..=200.
pub fn list_runs(conn: &Connection, filter: &RunFilter) -> Result<Vec<Run>, StoreError> {
    let mut conditions = vec!["1 = 1".to_string()];
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some((key, number)) = &filter.issue {
        conditions.push("p.key = ? AND i.number = ?".into());
        values.push(key.clone().into());
        values.push((*number).into());
    }
    if let Some(agent) = filter.agent_id {
        conditions.push("r.agent_id = ?".into());
        values.push(agent.into());
    }
    if let Some(status) = filter.status {
        conditions.push("r.status = ?".into());
        values.push(status.as_str().to_string().into());
    }
    if let Some(allowed) = &filter.allowed_projects {
        let marks = vec!["?"; allowed.len()].join(", ");
        conditions.push(format!("p.key IN ({marks})"));
        values.extend(allowed.iter().map(|key| key.clone().into()));
    }
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = filter.offset.unwrap_or(0).max(0);
    values.push(limit.into());
    values.push(offset.into());
    let mut stmt = conn.prepare(&format!(
        "{RUN_SELECT} WHERE {} ORDER BY r.id DESC LIMIT ? OFFSET ?",
        conditions.join(" AND ")
    ))?;
    let rows = stmt.query_map(params_from_iter(values.iter()), run_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?.run);
    }
    Ok(out)
}

/// Every run that is starting or running, oldest first.
pub fn active_runs(conn: &Connection) -> Result<Vec<Run>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "{RUN_SELECT} WHERE r.status IN ('starting', 'running') ORDER BY r.id"
    ))?;
    let rows = stmt.query_map([], run_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?.run);
    }
    Ok(out)
}

fn interrupt(
    tx: &Transaction<'_>,
    now: i64,
    ids: Vec<i64>,
    error: &str,
) -> Result<Vec<i64>, StoreError> {
    let mut done = Vec::new();
    for id in ids {
        let outcome = Outcome::new(RunStatus::Interrupted).error(error);
        if finish_run(tx, now, None, id, outcome)? {
            done.push(id);
        }
    }
    Ok(done)
}

fn ids_of(tx: &Transaction<'_>, sql: &str, boot_id: &str) -> Result<Vec<i64>, StoreError> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map([boot_id], |row| row.get::<_, i64>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// At start-up, before anything consumes events: every active run that
/// belongs to another boot was orphaned by a crash. Each becomes
/// `interrupted` ("server restarted") and its key is revoked, in one
/// transaction. Returns the ids. Does not touch processes: a process that
/// outlived a hard kill is for the supervisor (TRK-45) to find.
pub fn reconcile_startup(
    tx: &Transaction<'_>,
    now: i64,
    boot_id: &str,
) -> Result<Vec<i64>, StoreError> {
    let ids = ids_of(
        tx,
        "SELECT id FROM run WHERE status IN ('starting', 'running') AND boot_id <> ?1 ORDER BY id",
        boot_id,
    )?;
    interrupt(tx, now, ids, "server restarted")
}

/// At shutdown, after the run tasks stopped: whatever of this boot is still
/// active (created but never launched) becomes `interrupted` ("server
/// stopped").
pub fn interrupt_boot(
    tx: &Transaction<'_>,
    now: i64,
    boot_id: &str,
) -> Result<Vec<i64>, StoreError> {
    let ids = ids_of(
        tx,
        "SELECT id FROM run WHERE status IN ('starting', 'running') AND boot_id = ?1 ORDER BY id",
        boot_id,
    )?;
    interrupt(tx, now, ids, "server stopped")
}

/// Active runs that do not belong to `boot_id`: when a live server checks
/// itself after [`reconcile_startup`] there are none.
pub fn foreign_active_runs(conn: &Connection, boot_id: &str) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT 'run ' || id || ' is ' || status || ' under boot ' || boot_id || ', not the current ' || ?1
         FROM run WHERE status IN ('starting', 'running') AND boot_id <> ?1",
    )?;
    let rows = stmt.query_map([boot_id], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
