//! Runs: one start of an agent process on one issue (TRK-15).
//!
//! The store only keeps the record; the process itself is the runner's
//! business (`crates/aurora/src/runner`). A run goes
//! `[queued ->] starting -> running -> succeeded | failed | interrupted |
//! canceled`; a run that never got a process (the template is invalid, the
//! command is missing) goes `starting -> failed`, and one whose first message
//! cannot even be rendered is created already `failed`. `interrupted` is what a
//! crash or a stop of the server leaves behind for a run that was active
//! (`starting` or `running`); a `queued` run is never interrupted, it waits.
//!
//! # The queue (TRK-18)
//!
//! [`create_run`] given [`Limits`] decides inside its own transaction whether
//! the run may start now: the number of `starting` and `running` runs must be
//! below the global limit and below the agent's `max_concurrent`, and no older
//! queued run may be eligible to go first. Otherwise the run is created
//! `queued`: it records who or what asked (reason, trigger event, actor,
//! comment, manual message and base branch) and holds **no process, key,
//! working copy or transcript**. [`schedule`] promotes queued runs oldest
//! first (an agent at its limit is skipped, not waited for) and re-checks each
//! one before it starts: a disabled agent, a removed project access or an
//! archived issue ends the queued run `canceled` with the reason. The first
//! message of a promoted run is rendered by the runner in the promoting
//! transaction, so it sees the issue as it is then.
//!
//! A queued run is the pending intent for its issue and counts as active for
//! the one-active-run rule: a second trigger for the issue is dropped, exactly
//! as for a running one. The cap on queued runs per agent is
//! [`Limits::max_queued_per_agent`]; a trigger beyond it is refused with
//! [`QUEUE_FULL`].
//!
//! # Rules the database and this module enforce
//!
//! * **One active run per issue**: a partial unique index over
//!   `status IN ('queued', 'starting', 'running')`. Creating a second is
//!   `Conflict`.
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
//! # Start-up reconciliation (TRK-45)
//!
//! The runner first looks at the process of every leftover active run
//! ([`leftover_runs`]: pid and `pid_started_at`, kills it only when its creation
//! time still matches) and then calls [`reconcile_startup_with`] with what it
//! found. That one transaction marks each run `interrupted`, records `reaped`,
//! comments on the issue as the run's agent and moves a card that is still in a
//! `started` column back to the first `unstarted` one. Runs that end any other
//! way (cancel, timeout, failure, a clean shutdown) get none of this: a person
//! sees those happen. Nothing is ever restarted.
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
//!   trigger_event_id, template_source, branch, worktree_state, queued}`
//!   (`queued: true` for a run that waits; its `branch` is null until promoted)
//! * `run.promoted` `{branch, worktree_state}`: `queued` to `starting`, in the
//!   transaction that also mints the key and plans the working copy
//! * `run.started` `{pid}`
//! * `run.finished` `{status, exit_code, error?, worktree_state?, has_changes?,
//!   transcript_bytes?}` (`transcript_bytes` is the size of the transcript file
//!   when the runner finished it, absent when the run was canceled by a person:
//!   the cancel is committed before the process is stopped, see below). The
//!   replay reads only `status`, so the additive field changes nothing there.
//!
//! The per-run key's own `key.created` and `key.revoked` events (actor: the
//! agent's user) are written in the same transactions.
//!
//! # The working copy (TRK-16)
//!
//! A project with a repository gives each run of an agent without an explicit
//! `work_dir` its own git worktree. [`create_run`] only *plans* it, in the
//! transaction that creates the run: the path (`<data folder>/worktrees/<run
//! id>`) and the branch (`aurora/<KEY>-<n>-run<id>`, see [`branch_name`]) are
//! functions of the run's id, so the first message can name them before git
//! has run. The state is then `created`. The runner makes the worktree,
//! records the base it resolved ([`set_worktree_base`]) and, when the run
//! ends, inspects it and passes what it found in [`Outcome::worktree`], which
//! [`finish_run`] stores in the same transaction as the end of the run.
//! `removed` means it had no changes and was deleted; `kept` means it has
//! changes (or nobody could tell) and stays. **Nothing here ever decides to
//! delete a working copy that has changes.** A run that ends without anyone
//! having inspected its working copy (a crash, the shutdown sweep, a panic) is
//! recorded `kept` with a note, and start-up reconciliation turns every
//! leftover `created` into `kept` without touching the disk. A run canceled by
//! a person is finished before its process is stopped, so its working copy is
//! settled a moment later by [`record_worktree`] (no event; the row is the
//! authority).
//!
//! # The transcript (TRK-17)
//!
//! The output of the process is kept in full, up to a cap, in an append-only
//! file per run ([`transcript_path`]) that the runner writes while the process
//! runs. The row only records what the file held when it was finished:
//! `transcript_bytes` and `transcript_truncated`, set by [`finish_run`] (from
//! [`Outcome::transcript`]) or, for a run a person canceled, a moment later by
//! [`record_transcript`]. **No event is written per chunk.** A crash leaves
//! the columns at 0 and the file as it was; nothing reconciles the file.

use std::collections::HashMap;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::Serialize;
use serde_json::json;

use crate::StoreError;
use crate::agents;
use crate::auth::{self, Principal, Role, Scope};
use crate::comments::{self, NewComment};
use crate::issues::{self, MoveTarget};
use crate::projects::{StateType, conflict, emit_as, invalid, not_found, refusal};

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
    Queued,
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
            RunStatus::Queued => "queued",
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
            "queued" => RunStatus::Queued,
            "starting" => RunStatus::Starting,
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "interrupted" => RunStatus::Interrupted,
            "canceled" => RunStatus::Canceled,
            _ => return None,
        })
    }

    /// Not finished: queued, starting or running.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            RunStatus::Queued | RunStatus::Starting | RunStatus::Running
        )
    }

    /// Holds a slot of the limits: starting or running (a queued run does not).
    pub fn holds_slot(self) -> bool {
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

/// What became of a run's working copy; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeState {
    None,
    Created,
    Removed,
    Kept,
}

impl WorktreeState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeState::None => "none",
            WorktreeState::Created => "created",
            WorktreeState::Removed => "removed",
            WorktreeState::Kept => "kept",
        }
    }

    pub fn parse(text: &str) -> Option<WorktreeState> {
        Some(match text {
            "none" => WorktreeState::None,
            "created" => WorktreeState::Created,
            "removed" => WorktreeState::Removed,
            "kept" => WorktreeState::Kept,
            _ => return None,
        })
    }
}

impl ToSql for WorktreeState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.as_str().to_sql()
    }
}

impl FromSql for WorktreeState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        WorktreeState::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("worktree state {text}").into()))
    }
}

/// What a run left in its working copy, found by the runner before the run is
/// finished. Counts are capped at [`MAX_COUNT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEnd {
    pub state: WorktreeState,
    pub has_changes: bool,
    pub changed_files: i64,
    pub ahead: i64,
    pub note: Option<String>,
}

/// Counts of changed files and commits are bounded.
pub const MAX_COUNT: i64 = 1_000_000;
const MAX_NOTE_CHARS: usize = 300;

impl WorktreeEnd {
    /// There never was a working copy (it could not be created).
    pub fn none(note: impl Into<String>) -> WorktreeEnd {
        WorktreeEnd {
            state: WorktreeState::None,
            has_changes: false,
            changed_files: 0,
            ahead: 0,
            note: Some(note.into()),
        }
    }

    /// No changes: the folder and the branch were deleted.
    pub fn removed() -> WorktreeEnd {
        WorktreeEnd {
            state: WorktreeState::Removed,
            has_changes: false,
            changed_files: 0,
            ahead: 0,
            note: None,
        }
    }

    /// The folder and the branch stay. `has_changes` is false only when nobody
    /// could tell (then `note` says why).
    pub fn kept(
        has_changes: bool,
        changed_files: i64,
        ahead: i64,
        note: Option<String>,
    ) -> WorktreeEnd {
        WorktreeEnd {
            state: WorktreeState::Kept,
            has_changes,
            changed_files: changed_files.clamp(0, MAX_COUNT),
            ahead: ahead.clamp(0, MAX_COUNT),
            note,
        }
    }

    fn not_inspected() -> WorktreeEnd {
        WorktreeEnd::kept(
            false,
            0,
            0,
            Some("the working copy was not inspected when the run ended".to_string()),
        )
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
    /// The run's working copy: its folder (absolute), kept for the run's life
    /// and afterwards when it has changes. Empty when there is none.
    pub worktree_path: Option<String>,
    /// The git branch of the working copy.
    pub branch: Option<String>,
    /// The branch the working copy was made from, as resolved when it started.
    pub base_ref: Option<String>,
    /// The full SHA the working copy started from.
    pub base_commit: Option<String>,
    pub worktree_state: WorktreeState,
    /// The working copy had uncommitted or untracked files, or commits, when
    /// the run ended.
    pub has_changes: bool,
    pub changed_files: i64,
    /// Commits the branch is ahead of `base_commit`.
    pub ahead: i64,
    pub worktree_note: Option<String>,
    /// The size of the transcript file when the run ended; 0 when not recorded.
    pub transcript_bytes: i64,
    /// The transcript hit its size cap and the rest of the output is not in it.
    pub transcript_truncated: bool,
    /// When the run was queued; kept after it was promoted. `None` for a run
    /// that started at once.
    pub queued_at: Option<i64>,
    /// 1-based place among all queued runs, oldest first; `None` unless the
    /// run is `queued`. Computed when read.
    pub queue_position: Option<i64>,
    /// What start-up reconciliation found of this run's process when it
    /// marked the run `interrupted` (TRK-45): `killed leftover process <pid>`,
    /// `process already gone`, `pid reused, left alone` or `pid unknown`.
    /// `None` for every other run. Hidden from agents (it names a pid).
    pub reaped: Option<String>,
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
        r.worktree_path, r.branch, r.base_ref, r.base_commit, r.worktree_state, r.has_changes,
        r.changed_files, r.ahead, r.worktree_note,
        r.message, r.output_tail, r.transcript_bytes, r.transcript_truncated, r.queued_at,
        CASE WHEN r.status = 'queued' THEN
            (SELECT COUNT(*) FROM run q WHERE q.status = 'queued'
                AND (q.queued_at < r.queued_at OR (q.queued_at = r.queued_at AND q.id <= r.id)))
        END, r.reaped
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
            worktree_path: row.get(19)?,
            branch: row.get(20)?,
            base_ref: row.get(21)?,
            base_commit: row.get(22)?,
            worktree_state: row.get(23)?,
            has_changes: row.get(24)?,
            changed_files: row.get(25)?,
            ahead: row.get(26)?,
            worktree_note: row.get(27)?,
            transcript_bytes: row.get(30)?,
            transcript_truncated: row.get(31)?,
            queued_at: row.get(32)?,
            queue_position: row.get(33)?,
            reaped: row.get(34)?,
        },
        message: row.get(28)?,
        output_tail: row.get(29)?,
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
    /// The server's data folder (absolute, UTF-8). A run of an agent without a
    /// `work_dir` in a project with a repository gets a working copy under it
    /// (see the module docs); `None` plans none.
    pub worktree_root: Option<String>,
    /// Start from this branch instead of the project's `base_branch`.
    pub base_branch: Option<String>,
    /// The comment the first message may refer to; kept on a queued run until
    /// it is promoted.
    pub comment_id: Option<i64>,
    /// `None`: no limits, the run always starts at once (store-level tests).
    pub limits: Option<Limits>,
}

/// What decides between starting a run at once and queueing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// At most this many runs `starting` or `running` at the same time.
    pub max_runs: i64,
    /// At most this many `queued` runs per agent.
    pub max_queued_per_agent: i64,
    /// Queued runs older than this many milliseconds are canceled; 0 for never.
    pub max_queue_wait_ms: i64,
}

impl Limits {
    pub const DEFAULT_MAX_RUNS: i64 = 4;
    pub const DEFAULT_MAX_QUEUED_PER_AGENT: i64 = 20;
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_runs: Limits::DEFAULT_MAX_RUNS,
            max_queued_per_agent: Limits::DEFAULT_MAX_QUEUED_PER_AGENT,
            max_queue_wait_ms: 0,
        }
    }
}

/// The start of the `Conflict` text of a trigger that found its agent's queue
/// full; see [`is_queue_full`].
pub const QUEUE_FULL: &str = "queue full";

/// Whether a `Conflict` text says the agent's queue was full.
pub fn is_queue_full(why: &str) -> bool {
    why.starts_with(QUEUE_FULL)
}

/// The longest branch name [`branch_name`] makes.
pub const MAX_RUN_BRANCH_CHARS: usize = 100;

/// The branch of a run's working copy: `aurora/<KEY>-<number>-run<id>`, every
/// character outside ASCII letters, digits, `-` and `_` in the key replaced by
/// `-`, at most [`MAX_RUN_BRANCH_CHARS`] long (a long key is cut, the number and
/// the run id never are), so it is always a valid git ref name.
pub fn branch_name(key: &str, number: i64, run_id: i64) -> String {
    let suffix = format!("-{number}-run{run_id}");
    let prefix = "aurora/";
    let room = MAX_RUN_BRANCH_CHARS.saturating_sub(prefix.len() + suffix.len());
    let mut key: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(room)
        .collect();
    if key.is_empty() {
        key.push('x');
    }
    format!("{prefix}{key}{suffix}")
}

/// The folder of a run's working copy under the data folder `root`.
pub fn worktree_dir(root: &str, run_id: i64) -> String {
    std::path::Path::new(root)
        .join("worktrees")
        .join(run_id.to_string())
        .to_string_lossy()
        .into_owned()
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
            "SELECT id FROM run WHERE issue_id = ?1 AND status IN ('queued', 'starting', 'running')",
            [issue_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(active) = active {
        return Err(conflict(format!(
            "{identifier} already has an active run (run {active})"
        )));
    }

    let mut queued = false;
    if new.failed.is_none()
        && let Some(limits) = &new.limits
        && !has_capacity(tx, agent.id, agent.max_concurrent, limits)?
    {
        let waiting: i64 = tx.query_row(
            "SELECT COUNT(*) FROM run WHERE agent_id = ?1 AND status = 'queued'",
            [agent.id],
            |row| row.get(0),
        )?;
        if waiting >= limits.max_queued_per_agent {
            return Err(conflict(format!(
                "{QUEUE_FULL}: agent {} already has {waiting} queued run(s), at most {} are kept",
                agent.name, limits.max_queued_per_agent
            )));
        }
        queued = true;
    }
    let (status, finished, error) = match &new.failed {
        Some(error) => (
            RunStatus::Failed,
            Some(now),
            Some(clip(error, MAX_ERROR_CHARS)),
        ),
        None if queued => (RunStatus::Queued, None, None),
        None => (RunStatus::Starting, None, None),
    };
    let (queued_at, comment, requested_branch) = if queued {
        (Some(now), new.comment_id, new.base_branch.clone())
    } else {
        (None, None, None)
    };
    tx.execute(
        "INSERT INTO run (agent_id, issue_id, project_id, trigger_event_id, reason, status,
                          boot_id, message, template_source, template_id, edited, error,
                          finished, created, created_by, queued_at, trigger_comment_id,
                          requested_base_branch)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
            actor,
            queued_at,
            comment,
            requested_branch
        ],
    )
    .map_err(|err| {
        refusal(err, || {
            format!("{identifier} already has an active run, or this event already started one")
        })
    })?;
    let id = tx.last_insert_rowid();

    let mut secret = None;
    let mut branch = None;
    let mut worktree_state = WorktreeState::None;
    if new.failed.is_none() && !queued {
        let armed = arm_run(
            tx,
            now,
            &agent,
            id,
            project_id,
            (&new.project, new.number),
            new.worktree_root.as_deref(),
            new.base_branch.as_deref(),
        )?;
        secret = Some(armed.secret);
        branch = armed.branch;
        worktree_state = armed.worktree_state;
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
            "branch": branch,
            "worktree_state": worktree_state,
            "queued": queued,
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

/// Whether a run of `agent_id` may start now: both limits have room and no
/// older queued run that could start is waiting (it goes first; the scheduler
/// promotes it on its next pass).
fn has_capacity(
    conn: &Connection,
    agent_id: i64,
    max_concurrent: i64,
    limits: &Limits,
) -> Result<bool, StoreError> {
    let running: i64 = conn.query_row(
        "SELECT COUNT(*) FROM run WHERE status IN ('starting', 'running')",
        [],
        |row| row.get(0),
    )?;
    if running >= limits.max_runs {
        return Ok(false);
    }
    let mine: i64 = conn.query_row(
        "SELECT COUNT(*) FROM run WHERE agent_id = ?1 AND status IN ('starting', 'running')",
        [agent_id],
        |row| row.get(0),
    )?;
    if mine >= max_concurrent {
        return Ok(false);
    }
    let older_waits: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM run q JOIN agent a ON a.id = q.agent_id
             WHERE q.status = 'queued'
               AND (SELECT COUNT(*) FROM run x
                    WHERE x.agent_id = q.agent_id AND x.status IN ('starting', 'running'))
                   < a.max_concurrent)",
        [],
        |row| row.get(0),
    )?;
    Ok(!older_waits)
}

/// What starting a run needs beyond its row: the per-run key and, for a
/// project with a repository, the planned working copy.
struct Armed {
    secret: String,
    branch: Option<String>,
    worktree_state: WorktreeState,
}

#[allow(clippy::too_many_arguments)]
fn arm_run(
    tx: &Transaction<'_>,
    now: i64,
    agent: &agents::Agent,
    run_id: i64,
    project_id: i64,
    issue: (&str, i64),
    worktree_root: Option<&str>,
    base_branch: Option<&str>,
) -> Result<Armed, StoreError> {
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
        &format!("run-{run_id}"),
        Scope::Write,
    )?;
    tx.execute(
        "UPDATE run SET key_id = ?1 WHERE id = ?2",
        params![key.info.id, run_id],
    )?;

    let fixed_folder = agent
        .private
        .as_ref()
        .is_none_or(|private| private.work_dir.is_some());
    let mut branch = None;
    let mut worktree_state = WorktreeState::None;
    if !fixed_folder && let Some(root) = worktree_root {
        let repo: Option<(Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT repo_path, base_branch FROM project WHERE id = ?1",
                [project_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((Some(repo), configured)) = repo {
            let name = branch_name(issue.0, issue.1, run_id);
            tx.execute(
                "UPDATE run SET worktree_path = ?1, worktree_repo = ?2, branch = ?3,
                                base_ref = ?4, worktree_state = 'created'
                 WHERE id = ?5",
                params![
                    worktree_dir(root, run_id),
                    repo,
                    name,
                    base_branch.map(str::to_string).or(configured),
                    run_id
                ],
            )?;
            branch = Some(name);
            worktree_state = WorktreeState::Created;
        }
    }
    Ok(Armed {
        secret: key.key,
        branch,
        worktree_state,
    })
}

/// A queued run that [`schedule`] started: its row is `starting`, its key is
/// minted and its working copy planned. The first message is still the
/// caller's to render (unless `run.edited`: a person's text is kept).
#[derive(Debug)]
pub struct Promotion {
    pub created: Created,
    /// The comment the template may refer to.
    pub comment_id: Option<i64>,
    /// Who asked for the run when it was queued (the event's actor or the
    /// person who started it).
    pub actor: Option<i64>,
}

/// What one [`schedule`] pass did.
#[derive(Debug, Default)]
pub struct Scheduled {
    pub promoted: Vec<Promotion>,
    /// Queued runs that ended `canceled` instead of starting, with the reason.
    pub canceled: Vec<(i64, String)>,
}

/// Why a queued run may no longer start, if so.
fn queued_problem(tx: &Transaction<'_>, id: i64) -> Result<Option<String>, StoreError> {
    let (agent_id, project, issue_id): (i64, String, i64) = tx.query_row(
        "SELECT r.agent_id, p.key, r.issue_id FROM run r JOIN project p ON p.id = r.project_id
         WHERE r.id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let agent = agents::agent_config(tx, agent_id)?;
    if !agent.enabled {
        return Ok(Some("agent disabled".to_string()));
    }
    if auth::get_user(tx, agent.user_id)?.disabled {
        return Ok(Some("agent user disabled".to_string()));
    }
    if !agent.projects.contains(&project) {
        return Ok(Some(format!(
            "agent no longer has access to project {project}"
        )));
    }
    let archived: bool = tx.query_row(
        "SELECT archived <> 0 FROM issue WHERE id = ?1",
        [issue_id],
        |row| row.get(0),
    )?;
    if archived {
        return Ok(Some("issue archived".to_string()));
    }
    Ok(None)
}

/// One pass of the scheduler, in the caller's transaction:
///
/// 1. queued runs older than `limits.max_queue_wait_ms` (when set) are
///    canceled ("queue wait exceeded");
/// 2. every queued run is checked again: a disabled agent or user, a project
///    the agent may no longer access and an archived issue cancel it with that
///    reason, so a queue never starts what is no longer allowed;
/// 3. queued runs are promoted oldest first while the global limit has room,
///    skipping a run whose agent is at its own limit (it does not hold the
///    others back).
///
/// Nothing is promoted when `start` is false (the server is stopping).
pub fn schedule(
    tx: &Transaction<'_>,
    now: i64,
    boot_id: &str,
    worktree_root: Option<&str>,
    limits: &Limits,
    start: bool,
) -> Result<Scheduled, StoreError> {
    let mut out = Scheduled::default();
    let waiting: Vec<(i64, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT id, queued_at FROM run WHERE status = 'queued' ORDER BY queued_at, id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, queued_at) in waiting {
        let why = if limits.max_queue_wait_ms > 0 && now - queued_at >= limits.max_queue_wait_ms {
            Some("queue wait exceeded".to_string())
        } else {
            queued_problem(tx, id)?
        };
        if let Some(why) = why {
            let outcome = Outcome::new(RunStatus::Canceled).error(why.clone());
            if finish_run(tx, now, None, id, outcome)? {
                out.canceled.push((id, why));
            }
        }
    }
    // `while start` would never change `start`; the loop ends by `break`.
    loop {
        if !start {
            break;
        }
        let running: i64 = tx.query_row(
            "SELECT COUNT(*) FROM run WHERE status IN ('starting', 'running')",
            [],
            |row| row.get(0),
        )?;
        if running >= limits.max_runs {
            break;
        }
        let next: Option<i64> = tx
            .query_row(
                "SELECT q.id FROM run q JOIN agent a ON a.id = q.agent_id
                 WHERE q.status = 'queued'
                   AND (SELECT COUNT(*) FROM run x
                        WHERE x.agent_id = q.agent_id AND x.status IN ('starting', 'running'))
                       < a.max_concurrent
                 ORDER BY q.queued_at, q.id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = next else {
            break;
        };
        out.promoted
            .push(promote(tx, now, id, boot_id, worktree_root)?);
    }
    Ok(out)
}

fn promote(
    tx: &Transaction<'_>,
    now: i64,
    id: i64,
    boot_id: &str,
    worktree_root: Option<&str>,
) -> Result<Promotion, StoreError> {
    #[allow(clippy::type_complexity)]
    let (agent_id, project_id, project, number, comment_id, actor, base_branch): (
        i64,
        i64,
        String,
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = tx.query_row(
        "SELECT r.agent_id, r.project_id, p.key, i.number, r.trigger_comment_id, r.created_by,
                r.requested_base_branch
         FROM run r JOIN project p ON p.id = r.project_id JOIN issue i ON i.id = r.issue_id
         WHERE r.id = ?1",
        [id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    let changed = tx.execute(
        "UPDATE run SET status = 'starting', boot_id = ?1 WHERE id = ?2 AND status = 'queued'",
        params![boot_id, id],
    )?;
    if changed == 0 {
        return Err(conflict("the run is not queued"));
    }
    let agent = agents::agent_config(tx, agent_id)?;
    let armed = arm_run(
        tx,
        now,
        &agent,
        id,
        project_id,
        (&project, number),
        worktree_root,
        base_branch.as_deref(),
    )?;
    emit_as(
        tx,
        now,
        None,
        Some(project_id),
        "run",
        id,
        "run.promoted",
        &json!({ "branch": armed.branch, "worktree_state": armed.worktree_state }),
    )?;
    Ok(Promotion {
        created: Created {
            run: get_run(tx, id)?.run,
            key: Some(armed.secret),
            duplicate: false,
        },
        comment_id,
        actor,
    })
}

/// How many runs are queued.
pub fn queued_count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM run WHERE status = 'queued'",
        [],
        |row| row.get(0),
    )?)
}

/// One queued run in the queue view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueEntry {
    pub run_id: i64,
    pub agent_id: i64,
    pub agent: String,
    pub project: String,
    /// `KEY-n`.
    pub issue: String,
    pub queued_at: i64,
    /// 1-based among all queued runs, oldest first, whoever may see them.
    pub position: i64,
    /// `agent`: its agent is at its own limit; `global`: the overall limit (or
    /// an older run that goes first) holds it back.
    pub waiting_on: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueView {
    /// Runs `starting` or `running` (of the readable projects).
    pub running: i64,
    pub queued: Vec<QueueEntry>,
}

/// The queue, oldest first. `allowed` limits it to runs of those projects
/// (`None`: every project, an empty list: none); `agent_id` to one agent.
pub fn queue_view(
    conn: &Connection,
    allowed: Option<&[String]>,
    agent_id: Option<i64>,
) -> Result<QueueView, StoreError> {
    let visible = |project: &str, agent: i64| {
        allowed.is_none_or(|keys| keys.iter().any(|key| key == project))
            && agent_id.is_none_or(|wanted| wanted == agent)
    };
    let mut stmt = conn.prepare(
        "SELECT r.id, r.agent_id, u.name, p.key, i.number, r.queued_at,
                (SELECT COUNT(*) FROM run x
                 WHERE x.agent_id = r.agent_id AND x.status IN ('starting', 'running'))
                    >= a.max_concurrent
         FROM run r
         JOIN agent a ON a.id = r.agent_id
         JOIN user u ON u.id = a.user_id
         JOIN issue i ON i.id = r.issue_id
         JOIN project p ON p.id = r.project_id
         WHERE r.status = 'queued' ORDER BY r.queued_at, r.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, bool>(6)?,
        ))
    })?;
    let mut queued = Vec::new();
    for (index, row) in rows.enumerate() {
        let (run_id, agent, name, project, number, queued_at, agent_full) = row?;
        if visible(&project, agent) {
            queued.push(QueueEntry {
                run_id,
                agent_id: agent,
                agent: name,
                issue: format!("{project}-{number}"),
                project,
                queued_at,
                position: i64::try_from(index).unwrap_or(i64::MAX - 1) + 1,
                waiting_on: if agent_full { "agent" } else { "global" },
            });
        }
    }
    let mut stmt = conn.prepare(
        "SELECT p.key, r.agent_id FROM run r JOIN project p ON p.id = r.project_id
         WHERE r.status IN ('starting', 'running')",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut running = 0;
    for row in rows {
        let (project, agent) = row?;
        if visible(&project, agent) {
            running += 1;
        }
    }
    Ok(QueueView { running, queued })
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
    mark_running_process(tx, now, run_id, pid, None)
}

/// [`mark_running`] that also records when the process was created (see
/// `pid_started_at` in migration 0018), so a later start-up can tell this
/// process from a new one that got the same pid. `None` when the system would
/// not say.
pub fn mark_running_process(
    tx: &Transaction<'_>,
    now: i64,
    run_id: i64,
    pid: i64,
    pid_started_at: Option<i64>,
) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE run SET status = 'running', pid = ?1, started = ?2, pid_started_at = ?3
         WHERE id = ?4 AND status = 'starting'",
        params![pid, now, pid_started_at, run_id],
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
    /// What the runner found in the working copy. `None`: nobody looked; a run
    /// that ends this way and is not a cancel gets its working copy recorded as
    /// `kept` (see the module docs), a cancel is settled later.
    pub worktree: Option<WorktreeEnd>,
    /// What the transcript file held when the runner finished it; `None` when
    /// there is none (or, for a cancel, it is recorded later).
    pub transcript: Option<TranscriptEnd>,
}

/// The state of a finished transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptEnd {
    /// The file's size in bytes, after the runner's last write and sync.
    pub bytes: i64,
    /// The size cap was reached.
    pub truncated: bool,
}

/// The transcript file of a run under the data folder `root`:
/// `<root>/transcripts/<run id>.log`.
pub fn transcript_path(root: &std::path::Path, run_id: i64) -> std::path::PathBuf {
    root.join("transcripts").join(format!("{run_id}.log"))
}

impl Outcome {
    pub fn new(status: RunStatus) -> Outcome {
        Outcome {
            status,
            exit_code: None,
            error: None,
            output_tail: String::new(),
            truncated: false,
            worktree: None,
            transcript: None,
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
                        output_truncated = ?5, finished = ?6, transcript_bytes = ?7,
                        transcript_truncated = ?8
         WHERE id = ?9",
        params![
            outcome.status,
            outcome.exit_code,
            error,
            outcome.output_tail,
            outcome.truncated,
            now,
            outcome.transcript.map_or(0, |t| t.bytes.max(0)),
            outcome
                .transcript
                .is_some_and(|t| t.truncated && t.bytes > 0),
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
    let settled = match &outcome.worktree {
        Some(end) => Some(end.clone()),
        None if outcome.status != RunStatus::Canceled => Some(WorktreeEnd::not_inspected()),
        None => None,
    };
    let mut worktree = None;
    if let Some(end) = settled
        && apply_worktree_end(tx, run_id, &end)?
    {
        worktree = Some(end);
    }
    let mut payload = json!({ "status": outcome.status, "exit_code": outcome.exit_code });
    if let (Some(error), Some(map)) = (&error, payload.as_object_mut()) {
        map.insert("error".into(), json!(error));
    }
    if let (Some(end), Some(map)) = (&worktree, payload.as_object_mut()) {
        map.insert("worktree_state".into(), json!(end.state));
        map.insert("has_changes".into(), json!(end.has_changes));
    }
    if let (Some(transcript), Some(map)) = (outcome.transcript, payload.as_object_mut()) {
        map.insert("transcript_bytes".into(), json!(transcript.bytes.max(0)));
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

/// Stores what was found in a working copy that is still `created`. `false`
/// (nothing changed) for any other state, so a repeat or a late settle never
/// overwrites a result.
fn apply_worktree_end(
    tx: &Transaction<'_>,
    run_id: i64,
    end: &WorktreeEnd,
) -> Result<bool, StoreError> {
    if end.state == WorktreeState::Created {
        return Err(invalid("a working copy cannot be settled as created"));
    }
    let note = end.note.as_deref().map(|n| clip(n, MAX_NOTE_CHARS));
    let changed = tx.execute(
        "UPDATE run SET worktree_state = ?1, has_changes = ?2, changed_files = ?3,
                        ahead = ?4, worktree_note = ?5
         WHERE id = ?6 AND worktree_state = 'created'",
        params![
            end.state,
            end.has_changes,
            end.changed_files.clamp(0, MAX_COUNT),
            end.ahead.clamp(0, MAX_COUNT),
            note,
            run_id
        ],
    )?;
    Ok(changed > 0)
}

/// Settles the working copy of a run that was finished before anyone looked at
/// it (a cancel finishes the run first and stops the process after). `false`
/// when the working copy was settled already.
pub fn record_worktree(
    tx: &Transaction<'_>,
    run_id: i64,
    end: &WorktreeEnd,
) -> Result<bool, StoreError> {
    apply_worktree_end(tx, run_id, end)
}

/// Records the base the runner resolved and made the working copy from.
pub fn set_worktree_base(
    tx: &Transaction<'_>,
    run_id: i64,
    base_ref: &str,
    base_commit: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE run SET base_ref = ?1, base_commit = ?2
         WHERE id = ?3 AND worktree_state = 'created'",
        params![base_ref, base_commit, run_id],
    )?;
    Ok(())
}

/// What the runner needs to make, inspect and remove a run's working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeTarget {
    pub repo: String,
    pub path: String,
    pub branch: String,
    /// The branch asked for (the run's override or the project's setting);
    /// `None` when the repository's default is to be used.
    pub base_ref: Option<String>,
    pub base_commit: Option<String>,
    pub state: WorktreeState,
}

/// The working copy of a run, if it has one planned or made.
pub fn worktree_target(
    conn: &Connection,
    run_id: i64,
) -> Result<Option<WorktreeTarget>, StoreError> {
    let found = conn
        .query_row(
            "SELECT worktree_repo, worktree_path, branch, base_ref, base_commit, worktree_state
             FROM run WHERE id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, WorktreeState>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| not_found("run"))?;
    Ok(match found {
        (Some(repo), Some(path), Some(branch), base_ref, base_commit, state) => {
            Some(WorktreeTarget {
                repo,
                path,
                branch,
                base_ref,
                base_commit,
                state,
            })
        }
        _ => None,
    })
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

/// Records what the transcript file held for a run that is already finished
/// (a cancel finishes the run before the process is stopped, and a run that
/// exited at the same moment is finished by the cancel). Changes nothing for
/// an active run or one whose transcript is recorded already, so a repeat is
/// harmless. Returns whether it recorded.
pub fn record_transcript(
    tx: &Transaction<'_>,
    run_id: i64,
    end: TranscriptEnd,
) -> Result<bool, StoreError> {
    if end.bytes <= 0 {
        return Ok(false);
    }
    let changed = tx.execute(
        "UPDATE run SET transcript_bytes = ?1, transcript_truncated = ?2
         WHERE id = ?3 AND status NOT IN ('starting', 'running') AND transcript_bytes = 0",
        params![end.bytes, end.truncated, run_id],
    )?;
    Ok(changed > 0)
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
    /// `Some(true)`: only runs that left changes in their working copy.
    pub has_changes: Option<bool>,
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
    if let Some(changes) = filter.has_changes {
        conditions.push("r.has_changes = ?".into());
        values.push(i64::from(changes).into());
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

/// An active run of an earlier boot and what is known of its process: what
/// start-up reconciliation has to look at before it marks the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub run_id: i64,
    pub pid: Option<i64>,
    /// The process's creation time as recorded when it started; see migration
    /// 0018.
    pub pid_started_at: Option<i64>,
}

/// Every `starting` or `running` run that does not belong to `boot_id`, oldest
/// first.
pub fn leftover_runs(conn: &Connection, boot_id: &str) -> Result<Vec<Leftover>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, pid, pid_started_at FROM run
         WHERE status IN ('starting', 'running') AND boot_id <> ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([boot_id], |row| {
        Ok(Leftover {
            run_id: row.get(0)?,
            pid: row.get(1)?,
            pid_started_at: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The note recorded when the caller found out nothing about a process.
pub const NOT_CHECKED: &str = "process not checked";

/// [`reconcile_startup_with`] for a caller that did not look at any process.
pub fn reconcile_startup(
    tx: &Transaction<'_>,
    now: i64,
    boot_id: &str,
) -> Result<Vec<i64>, StoreError> {
    reconcile_startup_with(tx, now, boot_id, &HashMap::new())
}

/// At start-up, before anything consumes events: every active run that
/// belongs to another boot was orphaned by a crash. Each becomes
/// `interrupted` ("server restarted") and its key is revoked, in one
/// transaction, which also (TRK-45)
///
/// * records in `reaped` what the caller found of its process (`notes`, by run
///   id; [`NOT_CHECKED`] without one),
/// * adds a comment on the issue, written as the run's agent and starting with
///   `[Aurora]` (it says what happened and names the working copy's branch,
///   never a path or a secret), and
/// * when the issue's card is in a column of the `started` kind, moves it to
///   the end of the project's first `unstarted` column (a card a person
///   already moved is left where it is; with no such column the comment says
///   the card stayed).
///
/// A run is only reconciled while it is active, so a second start-up finds
/// nothing to do and adds no second comment or move. Returns the ids. The
/// processes themselves are the runner's business (`runner::reap`): this
/// function never touches one.
pub fn reconcile_startup_with(
    tx: &Transaction<'_>,
    now: i64,
    boot_id: &str,
    notes: &HashMap<i64, String>,
) -> Result<Vec<i64>, StoreError> {
    let ids = ids_of(
        tx,
        "SELECT id FROM run WHERE status IN ('starting', 'running') AND boot_id <> ?1 ORDER BY id",
        boot_id,
    )?;
    let mut done = Vec::new();
    for id in ids {
        let outcome = Outcome::new(RunStatus::Interrupted).error("server restarted");
        if !finish_run(tx, now, None, id, outcome)? {
            continue;
        }
        let note = notes.get(&id).map_or(NOT_CHECKED, String::as_str);
        tx.execute(
            "UPDATE run SET reaped = ?1 WHERE id = ?2",
            params![clip(note, MAX_REAPED_CHARS), id],
        )?;
        explain_interruption(tx, now, id, note)?;
        done.push(id);
    }
    // A working copy still `created` under another boot was never looked at
    // (a crash, or a cancel whose process was still being stopped). Without
    // touching the disk, the safe reading is "may have changes".
    tx.execute(
        "UPDATE run SET worktree_state = 'kept',
                        worktree_note = 'the server stopped before the working copy was inspected'
         WHERE worktree_state = 'created' AND boot_id <> ?1 AND status NOT IN ('starting', 'running')",
        [boot_id],
    )?;
    Ok(done)
}

const MAX_REAPED_CHARS: usize = 200;

/// The comment and the card move of [`reconcile_startup_with`] for one run
/// that was just interrupted, written as its agent's user.
fn explain_interruption(
    tx: &Transaction<'_>,
    now: i64,
    run_id: i64,
    note: &str,
) -> Result<(), StoreError> {
    type Found = (
        i64,
        String,
        i64,
        i64,
        Option<String>,
        WorktreeState,
        StateType,
    );
    let (project_id, project, number, user_id, branch, worktree, column): Found = tx.query_row(
        "SELECT p.id, p.key, i.number, a.user_id, r.branch, r.worktree_state, c.state_type
         FROM run r
         JOIN issue i ON i.id = r.issue_id
         JOIN project p ON p.id = r.project_id
         JOIN agent a ON a.id = r.agent_id
         JOIN \"column\" c ON c.id = i.column_id
         WHERE r.id = ?1",
        [run_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    let actor = Principal {
        user_id,
        role: Role::Agent,
        scope: Scope::Write,
    };
    let mut body = format!(
        "[Aurora] Run {run_id} was interrupted because the server restarted. Process: {note}."
    );
    if let (WorktreeState::Kept, Some(branch)) = (worktree, branch) {
        body.push_str(&format!(" Working copy: kept on branch {branch}."));
    }
    if column == StateType::Started {
        let target: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, name FROM \"column\"
                 WHERE project_id = ?1 AND state_type = 'unstarted' ORDER BY position, id LIMIT 1",
                [project_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match target {
            Some((column_id, name)) => {
                issues::move_issue(
                    tx,
                    now,
                    &actor,
                    &project,
                    number,
                    MoveTarget {
                        column_id,
                        ..MoveTarget::default()
                    },
                )?;
                body.push_str(&format!(" The card was moved back to {name}."));
            }
            None => body.push_str(
                " The card stays where it is: the project has no column of the unstarted kind.",
            ),
        }
    }
    comments::add_comment(
        tx,
        now,
        &actor,
        &project,
        number,
        NewComment { body, parent: None },
    )?;
    Ok(())
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
    let done = interrupt(tx, now, ids, "server stopped")?;
    tx.execute(
        "UPDATE run SET worktree_state = 'kept',
                        worktree_note = 'the server stopped before the working copy was inspected'
         WHERE worktree_state = 'created' AND boot_id = ?1 AND status NOT IN ('starting', 'running')",
        [boot_id],
    )?;
    Ok(done)
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
