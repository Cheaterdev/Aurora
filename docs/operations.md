# Operating Aurora: checking a database, crash tests, the soak

## `aurora check`: are the data consistent?

```text
aurora check --db <path> [--json]
```

Runs the invariant checks (`aurora_store::invariants`) on a database file and
prints one line per check, with the details of a failure under it. `--json`
prints the same as one JSON document (`database`, `ok`, `schema_version`,
`run_marker_present`, `checks: [{name, ok, details}]`). Without `--db` it uses
`AURORA_DB`, then `aurora.db`.

| exit code | meaning                                                        |
|-----------|----------------------------------------------------------------|
| 0         | every check passed                                             |
| 1         | at least one check failed                                      |
| 2         | the file could not be opened (missing, not a database, locked) |

The command is read-only. It opens the file the way a reader does: it never
migrates, never takes the write lock and never touches the `.running` marker. All
checks run inside one read snapshot, so it also works **beside a running
server** (SQLite WAL readers do not block the writer) and reports a consistent
view; a note says when the marker file exists. It works on an offline file and on
a copy. If another process holds the file locked so that it cannot be read, the
command exits with 2 and says so; check a copy then.

### What each check proves (and what a clean report does not)

| check | proves |
|-------|--------|
| `sqlite.integrity_check` | SQLite's full `PRAGMA integrity_check`: pages, indexes, rows are well formed |
| `sqlite.foreign_key_check` | no row refers to a row that does not exist |
| `schema.version` | migrations 1..n are recorded without holes and n is what this build expects (an older database fails with "start aurora to migrate it" and the checks that need the new tables are reported as not run) |
| `events.ids` | event ids are 1..n without gaps and the AUTOINCREMENT counter is not behind them |
| `events.immutable_triggers` | the three triggers that make the log append-only are installed |
| `events.state_agreement` | replaying the event log reproduces the tables (see below) |
| `projects.issue_numbering` | per project, the next issue number is above every number, and numbers are unique |
| `documents.revision_chain` | revisions are numbered 1..current and the newest one is the document's title and body |
| `references.same_project` | parents, types, columns, initiatives, links and document links never cross projects |
| `comments.threads` | every reply hangs off a top-level comment of the same issue |
| `consumers.cursors` | no consumer cursor is beyond the newest event |
| `request_log.rows` | every remembered request id belongs to an existing user and a `2xx` response |
| `search.index` | the full-text index holds exactly the issues, comments and documents that should be in it |
| `prompt_templates.valid` | every stored prompt template parses and uses only known variables, and its issue type belongs to its project, so a run can never start from a template that fails validation |
| `runs.no_orphans` | an active run (`starting`/`running`) has a live key of its agent's user, no finish time, and a pid once `running`; a finished run has a finish time and no live key. (Foreign keys and a unique index cover missing rows and two active runs on one issue.) Offline (`aurora check`) an active run is allowed, because nobody can say whether its process lives; the server's own check right after start-up also demands that every active run belongs to the current boot |
| `runs.queue` | a `queued` run has a queue time, no start or finish time, no process, no key, no working copy and no transcript; every run has a `run.created` event; a queued run has no `run.promoted`, `run.started` or `run.finished` in the log. How many runs are going is deliberately **not** checked against any limit: limits can be lowered while runs are going, and those finish on their own. (A second active run for an issue is impossible by the unique index, which counts `queued`.) |
| `runs.worktrees` | a run's working copy is consistent with its row: a planned, kept or removed worktree has a folder path, a branch and the repository it belongs to; `removed` never goes with changes (`has_changes`, changed files or commits ahead); a run with no working copy (or one not yet settled) reports no changes; a run that is still active is not `kept` or `removed` yet; and a finished run is no longer `created` (a canceled run may be for the moment before its process is reaped). Only the database can fail this check. A folder that is missing for a `kept` copy, or still there for a `removed` one, is reported as a note and never fails, because the disk can legitimately differ (a person removed it by hand) |
| `runs.transcripts` | a run's recorded transcript size and truncation flag make sense (nothing recorded while the run is active; truncated only with a size), and a recorded size equals the size of `<folder of the database>/transcripts/<run id>.log`: the runner records it once, after its last write and sync, so a different size means the file was edited, cut or replaced. A size of 0 means "not recorded" (a crash, or a run that never had a process) and is never compared, because a crashed run's file may hold more than anyone recorded. A missing file is only a note (a copy of the database checked elsewhere, a backup without the data folder, a person who moved it) |

About `events.ids`: `AUTOINCREMENT` guarantees ids only grow and are never
reused, not that they are contiguous. SQLite updates its counter inside the
writing transaction, though, so a rollback gives the ids back and a committed log
has no holes. A hole therefore means an event was lost, and the check fails on
it. If events are ever pruned on purpose, this check changes with that decision.

**Event/state agreement** compares, per entity kind, the model rebuilt from the
log with the tables: projects (key, name), columns (names), issue types,
labels, issues (project, number, title, type, column, assignee, parent,
initiative, priority, labels, archived), initiatives (name, status, target
date), links, comments (issue, parent, author, mentions, edited, deleted),
documents (parent, title, archived, revision, body length), document links and
agents (name, user, triggers, limits, allowed projects), the length of each
project's rules text and each agent's prompt template overrides (project,
issue type, text length). An event kind the
replay does not know is a failure on purpose: a new kind must be taught to
`aurora_store::replay`. **Not covered**, because events do not carry it:
descriptions, comment and document body text (only the revision chain is
checked), the text of prompt templates and project rules (only their length),
the order of columns and of issues within a column, timestamps, the
agent's command and instructions, users, API keys, sessions. The backups
folder is not inspected.

### Adding a check (for later tasks)

Append an `Invariant` to `INVARIANTS` in `crates/aurora-store/src/invariants.rs`
with a stable name and a test in `tests_invariants.rs` that corrupts a copy of a
database and proves the check notices. TRK-15 made `runs.no_orphans` real (see
the table), and TRK-18 and TRK-45 extend it for the supervisor and for processes
that outlive the server.

## Failpoints (development and tests only)

Failpoints kill the process (`std::process::abort`, as power loss would) at a
named place in the write path. They are listed in one place,
`crates/aurora-store/src/failpoints.rs`.

| name | where it fires | what a restart must show |
|------|----------------|--------------------------|
| `before_commit` | writer thread, change and event written, `COMMIT` not issued | nothing of the change; a retry with the same request id applies it once |
| `after_commit` | writer thread, right after `COMMIT` | the whole change and its event; the request id is recorded |
| `after_commit_before_response` | HTTP layer, committed, response not written | as above; a retry is replayed, not applied twice |
| `before_event_delivery` | live stream or consumer poll: events read, none sent | events still in the log; a consumer that did not acknowledge gets them again (at least once) |
| `mid_migration` | inside a migration's transaction, SQL run, version not recorded | old schema intact; the next start backs up and migrates once |
| `mid_ack` | consumer ack transaction, cursor updated, not committed | the cursor where it was; the events are delivered again |
| `run_after_create` | runner, run row committed as `starting`, process not spawned | the run is `interrupted` ("server restarted"), its key revoked, no second run for the event |
| `run_after_spawn` | runner, process spawned, run not yet marked `running` | same; the stray process is for TRK-45 to reap |
| `run_before_finish` | runner, process exited, the finish not yet committed | same |
| `run_after_worktree` | runner, the run's git worktree and branch made, process not spawned | the run is `interrupted`, its working copy is `kept` (never claimed removed, never deleted), the folder and the branch are still there, no second run; the leftover is for a person to remove (see "Working copies") |
| `run_after_transcript_write` | runner, a batch of output appended to the run's transcript, the process still running | the run is `interrupted` ("server restarted"), no second run, `aurora check` passes; the transcript file exists and is readable up to its last complete line (a half-written last line is ignored, never an error); its size is not recorded (`transcript_bytes` 0) and no end marker was written |
| `run_after_queue` | runner, a run committed as `queued` (a limit was reached), nothing else done | after the restart the run is still `queued` (no key, no process, no worktree), it is promoted and runs exactly once, the redelivered trigger makes no second run, `aurora check` passes |
| `run_after_promote` | scheduler, a queued run committed as `starting` (key minted, working copy planned, message written), process not spawned | the run is `interrupted` ("server restarted") like any run that was active, its key is revoked, nothing starts twice, `aurora check` passes |

Arm one with `AURORA_TEST_FAILPOINT=<name>` (first hit) or `<name>:<n>` (the n-th
hit of that point in this process). `before_commit` and `after_commit` cover
every write of the server except the start-up/hourly purge of old request ids.

They exist only in builds made for tests: `aurora-store` compiles them when built
for its own tests or with the `failpoints` cargo feature, which `aurora` enables
for its dev-dependency, so `cargo test` builds a failpoint-capable binary and
`cargo build` / `cargo build --release` do not. To check a release binary, search
it for the variable name; there must be no match:

```powershell
cargo build --release
Select-String -Path target\release\aurora.exe -Pattern AURORA_TEST_FAILPOINT -Quiet   # False
```

## The runner (runs)

The server starts one run when an issue is assigned to a registered agent's user
or a comment mentions it (and the agent is enabled, allowed on the project and
has that trigger switched on), or when a person starts one (`POST
/api/v1/agents/{id}/runs`, or "Run on an issue now" on the agent page). The
rules are in `docs/api.md` (Runs); this is what runs where.

- **Dispatcher.** An event consumer named `runner`, registered at the newest
  event the first time (history is never replayed into runs), polled twice a
  second and acknowledged after each batch. Delivery is at least once:
  `UNIQUE(trigger_event_id, agent_id)` makes a repeat harmless, and the partial
  unique index `run_one_active_per_issue` allows one `starting`/`running` run per
  issue. A trigger for an issue that already has an active run (`queued`,
  `starting` or `running`) is dropped (logged at info): a queued run is the
  pending intent for its issue.
- **A run's life.** Optionally `queued` (see "Limits and the queue"), then
  `starting` (row, message, key and `run.created` event in one transaction) then
  `running` (`run.started`, with the pid) then one of
  `succeeded`, `failed` (exit code other than 0, timeout, command not found, bad
  working folder), `canceled` (a person) or `interrupted` (the server stopped or
  was killed). The end, the revocation of the run's key and `run.finished` are one
  transaction. A start-up pass marks every run left active by a previous boot
  `interrupted` ("server restarted") and revokes its key; a clean shutdown stops
  the children, waits up to 7 seconds and marks what is left `interrupted`
  ("server stopped"). **`interrupted` is the outcome of a crash**: it says "the
  server went away while this run was active", which is not the same as `failed`
  (the process ran and went wrong), and is what "runs left active by a crash are
  marked failed on start" means here. A timeout is a `failed` run with the error
  `timed out after N s`; the process tree is killed and the transcript ends with
  `run timed out`.
- **The process.** The agent's command, as registered, is launched as a plain
  child process: no shell, so quoting in `args` is not interpreted. The first
  message goes to standard input, or into every `{prompt}` argument. `{issue}`,
  `{run_id}` and `{mcp_config}` are substituted too. The environment adds
  `AURORA_URL`, `AURORA_MCP_URL`, `AURORA_API_KEY` (a write key created for this
  run), `AURORA_RUN_ID`, `AURORA_ISSUE`, `AURORA_PROJECT` and `AURORA_TRIGGER`.
  The working folder is, in this order: the run's own git worktree (see "Working
  copies" below), the agent's `work_dir` (it must exist; no isolation) or, when
  neither applies, a fresh `runs/<id>/work/` next to the database. The
  environment also carries `AURORA_WORKDIR` (that folder) and, for a worktree,
  `AURORA_BRANCH`. `runs/<id>/` also holds
  `mcp.json` for a command named `claude` (or when `{mcp_config}` is used); it
  is written to a temporary name, flushed and renamed, and holds the key, so it
  is as private as the data folder. Standard output and error are read as they
  arrive into the transcript (see "Transcripts"); the last 64 KiB are also stored
  on the run row.
- **Windows.** The command is looked up on `PATH` with `PATHEXT` and may be an
  `.exe`, `.cmd` or `.bat`. A `.cmd`/`.bat` shim is run through the command
  interpreter with the arguments quoted for it, and an argument holding `{prompt}`
  is refused for a shim (a prompt cannot be quoted safely for `cmd.exe`: use
  standard input). No console window is opened. Stopping a run (cancel, timeout,
  shutdown) ends the whole process tree with `taskkill /T /F`; on other systems
  the child leads its own process group and the group gets the signal.
  Nothing besides the aurora binary and the agent's command is needed, except
  `git` when a project has a repository (see "Working copies").
- **Not yet.** A process is not tied to the server: if the server is killed the
  agent keeps running until it ends by itself, and its key is already revoked at
  the next start. Job objects / process groups that die with the server, and
  reaping strays, are TRK-45.

## Limits and the queue

How many runs go at once is bounded, and what does not fit waits in a visible
queue (TRK-18). The rules and the JSON are in `docs/api.md` (Runs, `GET /queue`);
this is how to set and read it.

| setting | flag | default | range |
|---------|------|---------|-------|
| `AURORA_MAX_RUNS` | `--max-runs <n>` | 4 | 1 to 64: runs `starting` or `running`, all agents together |
| `AURORA_MAX_QUEUED_PER_AGENT` | `--max-queued-per-agent <n>` | 20 | 0 to 1000: queued runs one agent may have; a trigger beyond it is dropped and counted |
| `AURORA_MAX_QUEUE_WAIT_SECS` | `--max-queue-wait-secs <n>` | 0 (never) | 0 to 604800: a queued run that waited this long is canceled with "queue wait exceeded" |

The per-agent limit is the agent's `max_concurrent` (1 to 16, default 1) and the
timeout is the agent's `timeout_seconds` (60 to 86400, default 3600, always set,
so there is no separate global default: a change of the stored default is a
change of what new agents get). The timeout starts when the process does; a
queued run never times out while it waits.

- **Where the decision is made.** In the single writer, in the transaction that
  creates the run: it counts the runs that are `starting`/`running`, so two
  triggers cannot both take the last slot. A new run goes to the queue also when
  an older queued run could start (first come, first served). A queued run has no
  process, key, working copy or transcript; it keeps the trigger (reason, event,
  person, comment, manual message and base branch).
- **The scheduler.** One task, woken when a run ends, a run is queued or
  canceled, an agent is changed, and by a 5-second tick. It first cancels queued
  runs that may no longer start (agent or its user disabled, project access
  removed, issue archived, queue wait exceeded), then promotes the oldest queued
  runs while the overall limit has room; a run whose agent is at its limit is
  skipped, not waited for. Promotion is one transaction: `queued -> starting`,
  the key, the working copy plan, the first message and `run.promoted`. A panic in
  the scheduler is logged and the loop starts again after a second.
- **Restarts.** Queued runs survive a restart and are promoted by the first
  scheduler pass; active runs are marked `interrupted` first, so their slots are
  free. `run_after_queue` and `run_after_promote` (see Failpoints) are the crash
  tests of both sides.
- **Lowering a limit** never stops a run that is going: the queue just waits until
  the count is under the new limit.
- **A cancel** frees the slot when it commits; the canceled process tree may take
  a few seconds to go, so for that moment more processes than the limit can exist.
- **Seeing it.** `GET /api/v1/queue`; the issue page's Runs section (a "Queued
  (position n)" chip and a Cancel button); the run page ("waiting for a free
  slot"); and the "Queue" panel of the agent page (running against the limits and
  the agent's queued runs; it shows the state at page load).
  `queue_full_drops` in the API counts dropped triggers since the server started.

## Transcripts (live output)

The output of a run is written, as it arrives, to `<data folder>/transcripts/<run
id>.log` (the data folder is the database's folder; the folder is created on
demand). It is an append-only file of JSON lines `{"t","s","d"}`, with `s` of `o`
(stdout), `e` (stderr) or `x` (a note from Aurora); the exact format, the
scrubbing of the run's key and the repository path, the flush rule (every 250 ms
or 8 KiB) and the reading rules are in `docs/api.md` (Runs) and at the top of
`crates/aurora/src/transcript.rs`, the one module that knows the format.

- **Cap.** 8 MiB per run by default; `AURORA_TRANSCRIPT_MAX_BYTES` (or
  `--transcript-max-bytes`) sets it, from 1024 bytes to 1 GiB. At the cap the file
  gets one `transcript truncated at N bytes` note and no more output; the process
  is not stopped.
- **Durability.** An append log: no rename-into-place, one `sync_data` when the run
  ends. A crash loses at most the last batch (250 ms or 8 KiB) and can leave a
  half-written last line, which readers ignore. The database records the file's
  size (`run.transcript_bytes`) and the truncation flag once, when the run ends
  (for a run canceled by a person, a moment after the cancel); no event is written
  per chunk. Reconciliation at start-up never touches the file. `aurora check`
  (`runs.transcripts`) fails when a recorded size does not match the file and only
  notes a missing file.
- **End notes.** The file ends with `run canceled`, `run timed out`, `server
  stopped` or `process exited with code N` when the server was alive to write it;
  a successful exit adds none, and a crash leaves none.
- **Reading and the page.** `GET /api/v1/runs/{id}/transcript` (polled with
  `after=next`) and `.../transcript.txt`; the run page and the issue's Runs
  section show a live Output panel that polls every second (every 3 seconds once
  nothing new arrives) until the run is done, with Follow and Download. There are
  no sockets: a slow or vanished browser costs the server nothing. Windows opens
  the file for the writer with std's default sharing, so a reader can open it
  while it is appended to.
- **Cancel.** The Cancel button (run page and Runs section; owners and members)
  calls `POST /api/v1/runs/{id}/cancel`: the process tree is ended
  (`taskkill /T /F` on Windows), the run becomes `canceled`, its key is revoked,
  the transcript keeps everything up to the kill and its `run canceled` note.
- **Backups.** Transcripts live under the data folder, outside the database. A
  backup that is meant to bring runs back with their output must include the
  `transcripts/` folder (the built-in database backups do not). Nothing deletes a
  transcript; retention is a later task.
- **Not in MCP.** Agents cannot read transcripts through MCP or with their key: the
  output of one run can hold what another run's agent must not see.

## Working copies (git worktrees)

A project with a repository (`repo_path`, set by the owner; see `docs/api.md`)
gives every run of an agent without a `work_dir` its own git worktree. Aurora
runs the `git` it finds on `PATH` (or the program named by `AURORA_GIT`); on
Windows that is Git for Windows and nothing else is needed, WSL included.

- **Layout.** `<data folder>/worktrees/<run id>/`, a checkout of a new branch
  `aurora/<KEY>-<n>-run<id>` that starts at the base branch's commit (recorded in
  the run as `base_ref` and `base_commit`). The repository's own folder is never
  used as a working folder and its checked-out branch is never switched.
- **What git is asked.** Only `git` with an argument list (never a shell and never
  a command line built from text): `rev-parse`, `symbolic-ref`,
  `check-ref-format`, `worktree add`/`remove`/`prune`, `branch -D`,
  `status --porcelain --untracked-files=all` and `rev-list --count`. (`worktree
  remove --force` is used only to undo a worktree that was made a moment ago and
  failed to start, never on one a run has used.)
  Every call has a 60 second timeout (the process is killed and the step
  reported), at most 16 KiB of its error text is kept, prompts are off
  (`GIT_TERMINAL_PROMPT=0`), `GIT_OPTIONAL_LOCKS=0` is set and the language is
  fixed so the output can be read. All calls for one repository are serialised
  inside the server, because `git worktree add` and `remove` take the repository's
  lock; on Windows the repository's path is compared without regard to case. The
  repository's folder is removed from the error text stored on the run, because
  agents can read run errors.
- **Windows.** `git` runs with `core.longpaths=true` and without a console window.
  A `\\?\` prefix on a path (from canonicalisation) is stripped, and the forward
  slashes git prints are turned into backslashes, before a path is stored or
  compared. Keep the data folder path short anyway: a deep folder plus a deep
  repository can still exceed what other tools accept.
- **What is kept.** When the run ends, the worktree is inspected before the run's
  end is committed. If `git status` lists anything (untracked files too) or the
  worktree's `HEAD` is not `base_commit`, the folder and the branch are **kept**
  and never deleted by Aurora, on any path: success, failure, cancel, timeout,
  shutdown. If it is untouched, the worktree and its branch are removed (a plain
  `git worktree remove`, without `--force`, so git itself refuses if it disagrees;
  Aurora then deletes only a folder it can prove is `…/worktrees/<digits>` and
  clean). When anything is uncertain the copy is kept with a note on the run.
  Problems while cleaning up never fail the run.
- **Crashes.** If the server dies after a worktree was made, the next start marks
  the run `interrupted` and its working copy `kept`, without touching the disk
  (`run_after_worktree` is the crash test). A copy left by a crash can therefore
  be a stray, and nothing deletes it: it is for a person. (Reporting kept and
  unknown copies when the server starts is TRK-45's job; it must report them and
  still never delete one automatically.)
- **Looking at and removing a kept copy.** The run page and the issue's Runs
  section show the branch, folder, base and the number of changed files. To
  remove one by hand once its work is no longer needed:

  ```powershell
  git -C <repository> worktree remove <worktree folder>      # add --force for a dirty one
  git -C <repository> branch -D aurora/<KEY>-<n>-run<id>
  git -C <repository> worktree prune                          # if the folder was deleted by hand
  ```

  `aurora check` (`runs.worktrees`) reports a kept copy whose folder is gone, or a
  removed one whose folder is still there, as a note, never as a failure.
- **Not exposed to agents.** The repository's folder (`repo_path`) is not in the
  prompt, the environment, an MCP tool or an agent's view of the project or of a
  run. Agents learn only the worktree's folder, `AURORA_WORKDIR`, and branch.

## The crash tests

`crates/aurora/tests/runner_crash.rs` kills the real binary at each `run_*`
failpoint while the fake agent (`crates/aurora/examples/fake_agent.rs`) is being
launched, restarts it and checks that the run is `interrupted` exactly once, its
key is revoked, redelivery of the assignment makes no second run and `aurora
check` passes. `run_after_worktree` is tested against a project with a real
(temporary) git repository: the working copy and its branch survive the crash and
the restart. `run_after_transcript_write` kills the server while the agent is
still running and has printed part of its output: after the restart the run is
interrupted, the transcript file is readable (a torn last line is ignored) and
the checks pass. `run_after_queue` and `run_after_promote` are tested with the
global limit set to 1: a queued run survives a crash and is promoted exactly once
after the restart; a run killed between its promotion and its spawn is
`interrupted` and nothing starts twice. The rest of this section is about the
write-path matrix.

`crates/aurora/tests/crash_matrix.rs` runs the real binary built by `cargo test`.
For every failpoint above and several hit counts (1, 2, 5, 17 where there are
enough writes), it starts a server on a temporary data folder, runs a script of
about 26 API writes (projects, types, labels, agent, issues, comments, links,
documents, initiatives, a consumer that polls and acknowledges) against a server
armed to die at that hit, and then:

1. checks the crashed files (library and `aurora check`),
2. restarts on the same folder, which must notice the crash,
3. checks again, and verifies over HTTP that every change the client saw
   acknowledged is there,
4. retries the request that was in flight with its original `Idempotency-Key`
   and proves it was applied exactly once (and that repeating it is a replay),
5. verifies a consumer is given every event after its cursor, once.

Further cases: the live event stream with `before_event_delivery` (the event is
delivered after the restart), and `mid_migration` on a database with data at the
previous schema (killed, then migrated by the restart, with a backup), and on new
databases at the first, a middle and the last migration. Run them with `cargo test
-p aurora --test crash_matrix` (about 15 seconds).

## The kill-loop soak

`crates/aurora/tests/soak.rs` is ignored by default. It runs the real binary on
a temporary folder under load from four client threads (create issues, comments,
moves, links, documents, archives, immediate repeats of a request with the same
`Idempotency-Key`) and a consumer thread polling and acknowledging, and every 2 to
10 seconds (random) kills the server process and starts it again. After **every**
restart it:

* runs the invariant checks on the crashed files and again after the restart
  (the library each time, the `aurora check` command every fifth round),
* verifies that everything the clients saw acknowledged since the last round, and
  a random sample of older facts, is present,
* compares the number of soak issues in the database with the creates
  acknowledged (no loss, no duplicate), and that no title exists twice,
* clients resend a request that got no answer with the same `Idempotency-Key`.
* a run thread starts runs of a fake agent (which sleeps two seconds) on fresh
  issues, faster than they finish, with the overall limit at 2
  (`AURORA_MAX_RUNS=2`), cancels some of what waits, and watches
  `GET /api/v1/queue`: never more than 2 runs started at once and queue
  positions 1, 2, 3 ... in order. After every restart the check also finds no
  more than 2 runs `starting`/`running` and no run promoted or finished twice,
  and the invariant checks cover the rest (`runs.queue`, `runs.no_orphans`,
  event replay). Run `cargo build --examples -p aurora` first: the agent is an
  example binary. A killed server leaves its fake agent running until its two
  seconds are over; stopping agent processes with the server is not done yet.

It prints a summary (kills, restarts, writes acknowledged, resends, checks,
runs by status, duration) and fails on any violation. Memory stays bounded: only the unverified
acknowledgements and a reservoir of 2000 older facts are kept.

```powershell
# two minutes (default)
cargo test -p aurora --test soak -- --ignored --nocapture
# the one-hour acceptance run
$env:AURORA_SOAK_SECONDS = "3600"
cargo test -p aurora --test soak -- --ignored --nocapture
```

Only the soak's own child process is killed. Leave the machine alone while it
runs (it uses a temporary folder and a random localhost port). The test is
considered passed when the process exits successfully and the summary shows
`violations: 0`.
