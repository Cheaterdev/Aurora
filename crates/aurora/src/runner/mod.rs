//! The runner: starts an agent's command as a plain child process when an
//! issue is assigned to the agent or the agent is mentioned (TRK-15).
//!
//! * The **dispatcher** is a supervised task: consumer `runner` (registered at
//!   the newest event the first time, never replaying history) polls events,
//!   asks `agents::triggers_for_event` what each wants, creates the run rows
//!   and acknowledges the batch afterwards. A crash before the acknowledgement
//!   redelivers events; `UNIQUE(trigger_event_id, agent_id)` makes that harmless.
//!   An issue that already has an active run drops the trigger (logged, no
//!   comment; TRK-18 adds queueing).
//! * A **run task** per run spawns the process (see [`spawn`]), records the
//!   pid, captures the last 64 KiB of output, enforces the agent's timeout and
//!   records the result. Whatever happens it ends with one `finish_run`
//!   transaction that also revokes the per-run key.
//! * [`Runner::shutdown`] stops the dispatcher, kills the children and marks
//!   their runs `interrupted`; [`Runner::start`] first reconciles runs left
//!   active by a crashed server.
//!
//! * **A working copy per run** (TRK-16, see `aurora_store::runs` and
//!   [`crate::gitwork`]): a run planned with one gets a git worktree made before
//!   its process starts (the first message already names it), and when the run
//!   ends, whatever the way, the worktree is inspected before the run is
//!   finished: without changes it is removed together with its branch, with
//!   changes (or when anything is unclear) it is kept for a person. The runner
//!   never deletes a worktree that has changes.
//!
//! Children are killed as a tree on a best-effort basis only; they are not tied
//! to the server's life (TRK-45 adds containment), so a hard-killed server can
//! leave an agent process behind.

mod capture;
#[cfg_attr(windows, path = "platform/windows.rs")]
#[cfg_attr(not(windows), path = "platform/other.rs")]
mod platform;
mod spawn;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aurora_store::agents::{self, Trigger, TriggerReason};
use aurora_store::events::{self, EventFilter, Start};
use aurora_store::failpoints::{self, Point};
use aurora_store::projects::Rejection;
use aurora_store::prompt::{self, PromptRequest, TriggerCtx, TriggerKind};
use aurora_store::runs::{
    self, Created, NewRun, OUTPUT_TAIL_BYTES, Outcome, RunDetail, RunReason, RunStatus,
    WorktreeEnd, WorktreeState, WorktreeTarget,
};
use aurora_store::rusqlite::Transaction;
use aurora_store::{Store, StoreError, now_millis};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::gitwork::Git;
use capture::{SharedTail, Tail, pump};

/// The name of the dispatcher's consumer cursor.
pub const CONSUMER: &str = "runner";
const BATCH: i64 = 100;
const FINISH_ATTEMPTS: u32 = 5;
const READER_GRACE: Duration = Duration::from_secs(2);
const CANCEL_WAIT: Duration = Duration::from_secs(10);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);

/// A command file found by [`platform::resolve_command`].
pub struct Resolved {
    pub path: PathBuf,
    /// A `.cmd` / `.bat` file, run through `cmd.exe`.
    pub shim: bool,
}

/// Knobs for tests; the defaults are what the server uses.
#[derive(Debug, Clone)]
pub struct RunnerOptions {
    /// How often the dispatcher looks for new events.
    pub poll_every: Duration,
    /// Replaces every agent's timeout (the registry's minimum is 60 s).
    pub timeout_override: Option<Duration>,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        RunnerOptions {
            poll_every: Duration::from_millis(500),
            timeout_override: None,
        }
    }
}

pub struct RunnerConfig {
    /// The server's data folder: runs get `runs/<id>/` under it.
    pub data_dir: PathBuf,
    /// What the agent is told the server's address is (no trailing slash).
    pub base_url: String,
    pub options: RunnerOptions,
}

impl RunnerConfig {
    /// The data folder is the database's folder; the address is the public
    /// URL if one is configured, else the listening address (a wildcard host
    /// becomes loopback).
    pub fn for_server(store: &Store, listen: SocketAddr, options: RunnerOptions) -> RunnerConfig {
        let data_dir = store
            .path()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), PathBuf::from);
        let data_dir = std::path::absolute(&data_dir).unwrap_or(data_dir);
        let ip = if listen.ip().is_unspecified() {
            if listen.is_ipv6() {
                "[::1]".to_string()
            } else {
                "127.0.0.1".to_string()
            }
        } else if listen.is_ipv6() {
            format!("[{}]", listen.ip())
        } else {
            listen.ip().to_string()
        };
        let base_url = store
            .public_url()
            .map_or_else(|| format!("http://{ip}:{}", listen.port()), str::to_string);
        RunnerConfig {
            data_dir,
            base_url,
            options,
        }
    }
}

/// Why a run is told to stop.
#[derive(Clone, Copy)]
enum Stop {
    Cancel(Option<i64>),
    Shutdown,
}

/// What the runner holds for one live run.
struct Control {
    stop: watch::Sender<Option<Stop>>,
    done: watch::Sender<bool>,
    pid: AtomicU32,
}

struct Inner {
    store: Store,
    git: Git,
    config: RunnerConfig,
    boot_id: String,
    stopping: AtomicBool,
    halt: watch::Sender<bool>,
    active: Mutex<HashMap<i64, Arc<Control>>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
}

/// Cheap to clone; all clones are the one runner.
#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What starts a run: from an event, or by a person.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub agent_id: i64,
    pub project: String,
    pub number: i64,
    pub reason: RunReason,
    pub trigger_event_id: Option<i64>,
    pub comment_id: Option<i64>,
    /// Who caused it: the actor of the event, or the person starting it.
    pub actor: Option<i64>,
    /// A person's edited first message, sent verbatim.
    pub message: Option<String>,
    /// Start the working copy from this branch instead of the project's
    /// (already checked by the caller).
    pub base_branch: Option<String>,
}

fn kind_of(reason: RunReason) -> TriggerKind {
    match reason {
        RunReason::Assigned => TriggerKind::Assigned,
        RunReason::Mentioned => TriggerKind::Mentioned,
        RunReason::Manual => TriggerKind::Manual,
    }
}

fn split_issue(issue: &str) -> Option<(String, i64)> {
    let (key, number) = issue.rsplit_once('-')?;
    Some((key.to_string(), number.parse().ok().filter(|n| *n > 0)?))
}

fn describe(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{secs:.0}")
    } else {
        format!("{secs:.1}")
    }
}

impl Runner {
    /// Reconciles runs a crashed server left active, checks the runs
    /// invariant, registers the consumer and starts the dispatcher. Call it
    /// before the server answers requests.
    pub async fn start(store: Store, config: RunnerConfig) -> Result<Runner, StoreError> {
        let boot_id = runs::new_boot_id()?;
        let boot = boot_id.clone();
        let interrupted = store
            .write_background(move |tx| runs::reconcile_startup(tx, now_millis(), &boot))
            .await?;
        if !interrupted.is_empty() {
            tracing::warn!(
                runs = ?interrupted,
                "runs were still active when the server last stopped; marked them interrupted \
                 (a process they started may still be running: TRK-45 will find those)"
            );
        }
        let boot = boot_id.clone();
        let report = store
            .read(move |c| Ok(aurora_store::invariants::check_with_boot(c, &boot)))
            .await?;
        if let Some(check) = report.get("runs.no_orphans")
            && !check.ok
        {
            tracing::error!(details = %check.details, "the runs invariant failed at start-up");
        }
        store
            .write_background(|tx| {
                events::register_consumer(tx, now_millis(), CONSUMER, Start::Latest).map(|_| ())
            })
            .await?;

        let (halt, _) = watch::channel(false);
        let runner = Runner {
            inner: Arc::new(Inner {
                store,
                git: Git::system(),
                config,
                boot_id,
                stopping: AtomicBool::new(false),
                halt,
                active: Mutex::new(HashMap::new()),
                dispatcher: Mutex::new(None),
            }),
        };
        let looping = runner.clone();
        let handle = crate::spawn_supervised("runner-dispatcher", async move {
            looping.dispatch_loop().await;
            Ok(())
        });
        *lock(&runner.inner.dispatcher) = Some(handle);
        Ok(runner)
    }

    pub fn boot_id(&self) -> &str {
        &self.inner.boot_id
    }

    fn stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    // ---- creating runs ----

    /// Creates the run row, mints its key and renders its first message, all in
    /// the transaction `tx`. The text needs the run's id, so it is rendered
    /// after the row exists; a template that does not pass validation ends the
    /// run as `failed` in the same transaction (no process, key revoked).
    pub fn create_in_tx(
        &self,
        tx: &Transaction<'_>,
        now: i64,
        request: &StartRequest,
    ) -> Result<Created, StoreError> {
        let edited = request.message.is_some();
        let mut created = runs::create_run(
            tx,
            now,
            request.actor,
            NewRun {
                agent_id: request.agent_id,
                project: request.project.clone(),
                number: request.number,
                reason: request.reason,
                trigger_event_id: request.trigger_event_id,
                message: request.message.clone(),
                template_source: edited.then(|| "inline".to_string()),
                template_id: None,
                edited,
                boot_id: self.inner.boot_id.clone(),
                failed: None,
                worktree_root: self.inner.config.data_dir.to_str().map(str::to_string),
                base_branch: request.base_branch.clone(),
            },
        )?;
        if created.duplicate || edited {
            return Ok(created);
        }
        let id = created.run.id;
        // The path and the branch are known from the run's id before git has
        // made anything, so the message can name them.
        let tree = created.run.worktree_path.clone();
        let branch = created.run.branch.clone();
        let rendered = prompt::render_prompt(
            tx,
            now,
            &PromptRequest {
                agent_id: request.agent_id,
                project: &request.project,
                number: request.number,
                trigger: TriggerCtx {
                    kind: kind_of(request.reason),
                    comment_id: request.comment_id,
                    actor_id: request.actor,
                },
                workdir: tree.as_deref(),
                branch: branch.as_deref(),
                run_id: Some(id),
                public_url: self.inner.store.public_url(),
                override_template: None,
            },
        );
        match rendered {
            Ok(rendered) => runs::set_message(
                tx,
                id,
                &rendered.text,
                rendered.template_source.as_str(),
                rendered.template_id,
            )?,
            Err(StoreError::Rejected(Rejection::Invalid(why))) => {
                let mut outcome = Outcome::new(RunStatus::Failed)
                    .error(format!("prompt template invalid: {why}"));
                outcome.worktree = Some(WorktreeEnd::none("the run never started"));
                runs::finish_run(tx, now, request.actor, id, outcome)?;
                created.key = None;
                created.run = runs::get_run(tx, id)?.run;
            }
            Err(other) => return Err(other),
        }
        Ok(created)
    }

    /// Starts the process of a run [`Runner::create_in_tx`] committed. Does
    /// nothing for a duplicate or a run that failed to render.
    pub fn launch(&self, created: Created) {
        let Some(key) = created.key else {
            return;
        };
        if created.run.status != RunStatus::Starting || self.stopping() {
            return;
        }
        let id = created.run.id;
        let (stop, _) = watch::channel(None);
        let (done, _) = watch::channel(false);
        let control = Arc::new(Control {
            stop,
            done,
            pid: AtomicU32::new(0),
        });
        lock(&self.inner.active).insert(id, control.clone());
        let me = self.clone();
        tokio::spawn(async move { me.supervise(id, key, control).await });
    }

    // ---- the dispatcher ----

    async fn dispatch_loop(&self) {
        let mut halt = self.inner.halt.subscribe();
        let mut failing = false;
        loop {
            if self.stopping() {
                return;
            }
            let more = match self.poll_once().await {
                Ok(count) => {
                    failing = false;
                    count >= BATCH as usize
                }
                Err(StoreError::Closed) => return,
                Err(err) => {
                    if !failing {
                        tracing::warn!(error = %err, "the runner could not process events; retrying");
                    }
                    failing = true;
                    false
                }
            };
            if more {
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep(self.inner.config.options.poll_every) => {}
                _ = halt.wait_for(|stopped| *stopped) => return,
            }
        }
    }

    /// One batch. Returns how many events it saw.
    async fn poll_once(&self) -> Result<usize, StoreError> {
        let (events, triggers) = self
            .inner
            .store
            .read(|c| {
                let events = events::poll(c, CONSUMER, Some(BATCH), &EventFilter::default())?;
                let mut triggers = Vec::new();
                for event in &events {
                    triggers.extend(agents::triggers_for_event(c, event)?);
                }
                Ok((events, triggers))
            })
            .await?;
        let Some(last) = events.last().map(|e| e.id) else {
            return Ok(0);
        };
        for trigger in triggers {
            if self.stopping() {
                return Ok(0);
            }
            self.handle_trigger(trigger).await?;
        }
        self.inner
            .store
            .write_background(move |tx| {
                events::ack_background(tx, now_millis(), CONSUMER, last).map(|_| ())
            })
            .await?;
        Ok(events.len())
    }

    async fn handle_trigger(&self, trigger: Trigger) -> Result<(), StoreError> {
        let Some((project, number)) = split_issue(&trigger.issue) else {
            tracing::warn!(issue = %trigger.issue, "a trigger names no valid issue; skipped");
            return Ok(());
        };
        let request = StartRequest {
            agent_id: trigger.agent_id,
            project,
            number,
            reason: match trigger.reason {
                TriggerReason::Assigned => RunReason::Assigned,
                TriggerReason::Mentioned => RunReason::Mentioned,
            },
            trigger_event_id: Some(trigger.event_id),
            comment_id: trigger.comment_id,
            actor: trigger.actor,
            message: None,
            base_branch: None,
        };
        let runner = self.clone();
        let outcome = self
            .inner
            .store
            .write_background(move |tx| runner.create_in_tx(tx, now_millis(), &request))
            .await;
        match outcome {
            Ok(created) if created.duplicate => {
                tracing::debug!(event = trigger.event_id, "event already has its run");
            }
            Ok(created) => {
                tracing::info!(
                    run = created.run.id, issue = %trigger.issue, reason = ?trigger.reason,
                    "run created"
                );
                self.launch(created);
            }
            Err(StoreError::Rejected(Rejection::Conflict(why))) => {
                tracing::info!(event = trigger.event_id, %why, "trigger dropped");
            }
            Err(StoreError::Rejected(why)) => {
                tracing::warn!(event = trigger.event_id, %why, "trigger refused");
            }
            Err(other) => return Err(other),
        }
        Ok(())
    }

    // ---- one run ----

    async fn supervise(&self, run_id: i64, key: String, control: Arc<Control>) {
        let inner = {
            let me = self.clone();
            let control = control.clone();
            tokio::spawn(async move { me.execute(run_id, key, control).await })
        };
        if let Err(join) = inner.await {
            let why = if join.is_panic() {
                "internal error: the run task panicked"
            } else {
                "internal error: the run task was cancelled"
            };
            tracing::error!(run = run_id, why, "run task ended abnormally");
            let pid = control.pid.load(Ordering::SeqCst);
            if pid != 0 {
                platform::kill_tree(pid).await;
            }
            self.finish(run_id, None, Outcome::new(RunStatus::Failed).error(why))
                .await;
        }
        lock(&self.inner.active).remove(&run_id);
        control.done.send_replace(true);
    }

    async fn execute(&self, run_id: i64, key: String, control: Arc<Control>) {
        failpoints::hit(Point::RunAfterCreate);
        match self.drive(run_id, &key, &control).await {
            Some((mut outcome, actor)) => {
                // Looked at before the finish transaction, so the result is
                // recorded atomically with the end of the run.
                outcome.worktree = self.settle(run_id).await;
                failpoints::hit(Point::RunBeforeFinish);
                self.finish(run_id, actor, outcome).await;
            }
            None => {
                // Someone else finished the run (a cancel); its working copy
                // is still ours to look at.
                if let Some(end) = self.settle(run_id).await {
                    self.record_worktree(run_id, end).await;
                }
            }
        }
    }

    // ---- the working copy ----

    /// A recorded folder is only touched when it is where this server would
    /// have put the run's working copy (a damaged row must never point the
    /// runner at some other folder).
    fn is_planned_path(&self, run_id: i64, target: &WorktreeTarget) -> bool {
        self.inner
            .config
            .data_dir
            .to_str()
            .map(|root| runs::worktree_dir(root, run_id))
            .as_deref()
            == Some(target.path.as_str())
    }

    /// Makes the worktree a run was planned with and records the base it
    /// started from. Anything the creation made is removed again on failure.
    async fn make_worktree(&self, run_id: i64, target: &WorktreeTarget) -> Result<(), String> {
        if !self.is_planned_path(run_id, target) {
            return Err("the planned folder is not under this server's data folder".to_string());
        }
        let base = self
            .inner
            .git
            .create_worktree(
                Path::new(&target.repo),
                Path::new(&target.path),
                &target.branch,
                target.base_ref.as_deref(),
            )
            .await?;
        let recorded = self
            .inner
            .store
            .write_background(move |tx| {
                runs::set_worktree_base(tx, run_id, &base.name, &base.commit)
            })
            .await;
        recorded.map_err(|err| format!("could not record the working copy: {err}"))?;
        failpoints::hit(Point::RunAfterWorktree);
        Ok(())
    }

    /// Looks at the working copy of a run whose process has ended and removes
    /// it when it has no changes. `None` when the run has none to settle.
    /// Never turns a problem into a failure of the run: when unsure it keeps.
    async fn settle(&self, run_id: i64) -> Option<WorktreeEnd> {
        let target = match self
            .inner
            .store
            .read(move |c| runs::worktree_target(c, run_id))
            .await
        {
            Ok(Some(target)) if target.state == WorktreeState::Created => target,
            Ok(_) => return None,
            Err(err) => {
                tracing::warn!(run = run_id, error = %err, "could not read the run's working copy");
                return None;
            }
        };
        let path = Path::new(&target.path);
        if !path.exists() {
            return Some(WorktreeEnd::none("the working copy was not created"));
        }
        let found = match self
            .inner
            .git
            .inspect(path, target.base_commit.as_deref())
            .await
        {
            Ok(found) => found,
            Err(why) => {
                tracing::warn!(run = run_id, %why, "could not inspect the working copy; keeping it");
                let why = crate::gitwork::scrub(&why, &target.repo);
                return Some(WorktreeEnd::kept(
                    false,
                    0,
                    0,
                    Some(format!("could not inspect the working copy: {why}")),
                ));
            }
        };
        if found.has_changes {
            return Some(WorktreeEnd::kept(
                true,
                found.changed_files,
                found.ahead,
                None,
            ));
        }
        if !self.is_planned_path(run_id, &target) {
            return Some(WorktreeEnd::kept(
                false,
                0,
                0,
                Some("the working copy is not under this server's data folder; left alone".into()),
            ));
        }
        match self
            .inner
            .git
            .remove_worktree(
                Path::new(&target.repo),
                path,
                &target.branch,
                target.base_commit.as_deref(),
            )
            .await
        {
            Ok(()) => Some(WorktreeEnd::removed()),
            Err(why) => {
                tracing::warn!(run = run_id, %why, "could not remove the working copy; keeping it");
                let why = crate::gitwork::scrub(&why, &target.repo);
                Some(WorktreeEnd::kept(
                    false,
                    0,
                    0,
                    Some(format!("could not remove the working copy: {why}")),
                ))
            }
        }
    }

    /// For a run somebody else already finished.
    async fn record_worktree(&self, run_id: i64, end: WorktreeEnd) {
        let result = self
            .inner
            .store
            .write_background(move |tx| runs::record_worktree(tx, run_id, &end))
            .await;
        if let Err(err) = result {
            tracing::warn!(run = run_id, error = %err, "could not record the working copy of a finished run");
        }
    }

    /// Records the end of a run, retrying a transient write failure.
    async fn finish(&self, run_id: i64, actor: Option<i64>, outcome: Outcome) {
        for attempt in 1..=FINISH_ATTEMPTS {
            let this = outcome.clone();
            let result = self
                .inner
                .store
                .write_background(move |tx| runs::finish_run(tx, now_millis(), actor, run_id, this))
                .await;
            match result {
                Ok(true) => return,
                Ok(false) => {
                    // Already finished by a person's cancel: keep what was captured.
                    if outcome.status == RunStatus::Canceled {
                        let this = outcome.clone();
                        let kept = self
                            .inner
                            .store
                            .write_background(move |tx| {
                                runs::record_output(
                                    tx,
                                    run_id,
                                    this.exit_code,
                                    &this.output_tail,
                                    this.truncated,
                                )
                            })
                            .await;
                        if let Err(err) = kept {
                            tracing::warn!(run = run_id, error = %err, "could not keep the output of a canceled run");
                        }
                    }
                    if let Some(end) = outcome.worktree.clone() {
                        self.record_worktree(run_id, end).await;
                    }
                    return;
                }
                Err(StoreError::Closed) => break,
                Err(StoreError::Rejected(why)) => {
                    tracing::error!(run = run_id, %why, "could not record the end of a run");
                    return;
                }
                Err(err) => {
                    tracing::warn!(run = run_id, attempt, error = %err, "could not record the end of a run");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        tracing::error!(
            run = run_id,
            "the end of a run could not be recorded; the next start will mark it interrupted"
        );
    }

    /// Runs the process to its end. `None` when the run was already finished
    /// by someone else (canceled before it started).
    async fn drive(
        &self,
        run_id: i64,
        key: &str,
        control: &Control,
    ) -> Option<(Outcome, Option<i64>)> {
        let fail = |text: String| Some((Outcome::new(RunStatus::Failed).error(text), None));
        let (detail, agent) = match self
            .inner
            .store
            .read(move |c| {
                let detail = runs::get_run(c, run_id)?;
                let agent = agents::agent_config(c, detail.run.agent_id)?;
                Ok((detail, agent))
            })
            .await
        {
            Ok(found) => found,
            Err(err) => return fail(format!("could not load the run: {err}")),
        };
        if detail.run.status != RunStatus::Starting {
            return None;
        }
        let Some(private) = agent.private.clone() else {
            return fail("the agent's command is not available".into());
        };
        let timeout = self
            .inner
            .config
            .options
            .timeout_override
            .unwrap_or_else(|| {
                Duration::from_secs(u64::try_from(agent.timeout_seconds).unwrap_or(3600))
            });

        let mut tree = None;
        if detail.run.worktree_state == WorktreeState::Created {
            let target = match self
                .inner
                .store
                .read(move |c| runs::worktree_target(c, run_id))
                .await
            {
                Ok(Some(target)) => target,
                Ok(None) => return fail("could not create worktree: the run records none".into()),
                Err(err) => return fail(format!("could not load the run: {err}")),
            };
            if let Err(why) = self.make_worktree(run_id, &target).await {
                let why = crate::gitwork::scrub(&why, &target.repo);
                return fail(format!("could not create worktree: {why}"));
            }
            tree = Some((PathBuf::from(&target.path), target.branch));
        }

        let data_dir = self.inner.config.data_dir.clone();
        let base_url = self.inner.config.base_url.clone();
        let secret = key.to_string();
        let prepared = tokio::task::spawn_blocking(move || {
            let RunDetail { run, message, .. } = detail;
            spawn::prepare(&spawn::Spec {
                run_id,
                issue: &run.issue,
                project: &run.project,
                reason: run.reason.as_str(),
                message: &message,
                command: &private.command,
                args: &private.args,
                work_dir: private.work_dir.as_deref(),
                worktree: tree
                    .as_ref()
                    .map(|(path, branch)| (path.as_path(), branch.as_str())),
                data_dir: &data_dir,
                base_url: &base_url,
                key: &secret,
            })
        })
        .await;
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(why)) => return fail(why),
            Err(_) => return fail("preparing the process failed unexpectedly".into()),
        };
        let spawn::Prepared { mut command, stdin } = prepared;
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => return fail(format!("could not start the agent's command: {err}")),
        };
        let pid = child.id().unwrap_or(0);
        control.pid.store(pid, Ordering::SeqCst);
        failpoints::hit(Point::RunAfterSpawn);

        let tail: SharedTail = Arc::new(Mutex::new(Tail::new(OUTPUT_TAIL_BYTES)));
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(tokio::spawn(pump(out, tail.clone())));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(pump(err, tail.clone())));
        }
        if let Some(mut input) = child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                if let Some(text) = stdin {
                    // A process that does not read its input closes the pipe.
                    let _ = input.write_all(text.as_bytes()).await;
                }
                let _ = input.shutdown().await;
            });
        }

        let started = self
            .inner
            .store
            .write_background(move |tx| {
                runs::mark_running(tx, now_millis(), run_id, i64::from(pid))
            })
            .await;
        let mut stop = control.stop.subscribe();
        enum End {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Stopped(Stop),
            Lost(String),
        }
        let end = match started {
            Err(StoreError::Rejected(_)) => {
                // Canceled while starting: whoever canceled already finished it.
                platform::kill_tree(pid).await;
                let _ = child.kill().await;
                return None;
            }
            Err(err) => End::Lost(format!("could not record the start: {err}")),
            Ok(()) => tokio::select! {
                status = child.wait() => End::Exited(status),
                () = tokio::time::sleep(timeout) => End::TimedOut,
                changed = stop.wait_for(|s| s.is_some()) => match changed {
                    Ok(reason) => End::Stopped((*reason).unwrap_or(Stop::Shutdown)),
                    Err(_) => End::Stopped(Stop::Shutdown),
                },
            },
        };
        let mut code = None;
        match &end {
            End::Exited(Ok(status)) => code = status.code(),
            End::Exited(Err(_)) => {}
            End::TimedOut | End::Stopped(_) | End::Lost(_) => {
                platform::kill_tree(pid).await;
                let _ = child.kill().await;
                if let Ok(status) = child.wait().await {
                    code = status.code();
                }
            }
        }
        for reader in readers {
            let abort = reader.abort_handle();
            if tokio::time::timeout(READER_GRACE, reader).await.is_err() {
                abort.abort();
            }
        }
        let (output_tail, truncated) = lock(&tail).finish();

        let mut outcome = match &end {
            End::Exited(Ok(status)) if status.success() => Outcome::new(RunStatus::Succeeded),
            End::Exited(Ok(status)) => Outcome::new(RunStatus::Failed).error(match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "ended by a signal".to_string(),
            }),
            End::Exited(Err(err)) => Outcome::new(RunStatus::Failed)
                .error(format!("could not wait for the process: {err}")),
            End::TimedOut => Outcome::new(RunStatus::Failed)
                .error(format!("timed out after {} s", describe(timeout))),
            End::Stopped(Stop::Cancel(_)) => Outcome::new(RunStatus::Canceled),
            End::Stopped(Stop::Shutdown) => {
                Outcome::new(RunStatus::Interrupted).error("server stopped")
            }
            End::Lost(why) => Outcome::new(RunStatus::Failed).error(why.clone()),
        };
        outcome.exit_code = code.map(i64::from);
        outcome.output_tail = output_tail;
        outcome.truncated = truncated;
        let actor = match end {
            End::Stopped(Stop::Cancel(actor)) => actor,
            _ => None,
        };
        Some((outcome, actor))
    }

    // ---- cancel and shutdown ----

    /// Stops the process of a run a person canceled. The cancel itself (the
    /// run becomes `canceled`, its key is revoked) is `runs::finish_run` in the
    /// request's own transaction; this only ends the process afterwards and
    /// returns once it is gone (or after a few seconds).
    pub async fn stop_process(&self, run_id: i64, actor: Option<i64>) {
        let control = lock(&self.inner.active).get(&run_id).cloned();
        if let Some(control) = control {
            let mut done = control.done.subscribe();
            control.stop.send_replace(Some(Stop::Cancel(actor)));
            let _ = tokio::time::timeout(CANCEL_WAIT, done.wait_for(|d| *d)).await;
        }
    }

    /// Stops the dispatcher, kills the children and marks their runs
    /// `interrupted`, within the shutdown budget.
    pub async fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.inner.halt.send_replace(true);
        let controls: Vec<Arc<Control>> = lock(&self.inner.active).values().cloned().collect();
        let mut waits = Vec::new();
        for control in &controls {
            let mut done = control.done.subscribe();
            control.stop.send_replace(Some(Stop::Shutdown));
            waits.push(async move {
                let _ = done.wait_for(|d| *d).await;
            });
        }
        let all_done = async {
            for wait in waits {
                wait.await;
            }
        };
        if tokio::time::timeout(SHUTDOWN_WAIT, all_done).await.is_err() {
            tracing::warn!(
                "some runs did not stop in time; the final sweep marks them interrupted"
            );
        }
        let handle = lock(&self.inner.dispatcher).take();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
        let boot = self.inner.boot_id.clone();
        match self
            .inner
            .store
            .write_background(move |tx| runs::interrupt_boot(tx, now_millis(), &boot))
            .await
        {
            Ok(done) if !done.is_empty() => {
                tracing::info!(runs = ?done, "marked unfinished runs interrupted");
            }
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "could not mark unfinished runs interrupted"),
        }
    }

    /// How many runs have a live task, for tests.
    pub fn live_runs(&self) -> usize {
        lock(&self.inner.active).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_keys_split_at_the_last_dash() {
        assert_eq!(split_issue("TRK-12"), Some(("TRK".into(), 12)));
        assert_eq!(split_issue("A-B-3"), Some(("A-B".into(), 3)));
        assert_eq!(split_issue("TRK-0"), None);
        assert_eq!(split_issue("TRK"), None);
        assert_eq!(split_issue("TRK-x"), None);
    }

    #[test]
    fn timeouts_read_naturally() {
        assert_eq!(describe(Duration::from_secs(3600)), "3600");
        assert_eq!(describe(Duration::from_millis(1500)), "1.5");
    }
}
