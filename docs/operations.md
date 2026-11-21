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
| `runs.no_orphans` | **vacuous today**: there is no run table until TRK-15; the slot exists so the runner tasks fill it in |

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
agents (name, user, triggers, limits, allowed projects). An event kind the
replay does not know is a failure on purpose: a new kind must be taught to
`aurora_store::replay`. **Not covered**, because events do not carry it:
descriptions, comment and document body text (only the revision chain is
checked), the order of columns and of issues within a column, timestamps, the
agent's command and instructions, users, API keys, sessions. The backups
folder is not inspected.

### Adding a check (for later tasks)

Append an `Invariant` to `INVARIANTS` in `crates/aurora-store/src/invariants.rs`
with a stable name and a test in `tests_invariants.rs` that corrupts a copy of a
database and proves the check notices. TRK-15 turns `runs.no_orphans` into a real
query (a run recorded as active whose owning process is gone), and TRK-18 and
TRK-45 extend it for the supervisor and the interrupted-on-shutdown state.

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

## The crash tests

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

It prints a summary (kills, restarts, writes acknowledged, resends, checks,
duration) and fails on any violation. Memory stays bounded: only the unverified
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
