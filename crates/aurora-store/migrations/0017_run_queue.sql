-- The run queue (TRK-18).
--
-- A run can now be `queued`: it records the intent to run (who or what asked,
-- for which issue, optionally the manual message and base branch) and holds no
-- process, key, working copy or transcript. It becomes `starting` when a slot
-- is free (`run.promoted`); the first message is rendered then, so it sees the
-- issue as it is by then.
--
-- SQLite cannot change a CHECK in place, so the table is rebuilt with its rows
-- and ids kept. Nothing references `run`, so no foreign key is deferred. The
-- indexes go with the old table and are recreated; the one-active-run index
-- now counts `queued` as active (a queued run is the pending intent for its
-- issue, and a second trigger for that issue is dropped).
--
-- New columns:
--   `queued_at`            when the run was queued; kept after it was promoted
--                          (NULL for a run that started at once).
--   `trigger_comment_id`   the comment the template may refer to, kept until
--                          the first message is rendered at promotion.
--   `requested_base_branch` the manual base branch override, until promotion.
CREATE TEMP TABLE run_old AS SELECT * FROM run;

DROP TABLE run;

CREATE TABLE run (
    id                    INTEGER PRIMARY KEY,
    agent_id              INTEGER NOT NULL REFERENCES agent (id),
    issue_id              INTEGER NOT NULL REFERENCES issue (id),
    project_id            INTEGER NOT NULL REFERENCES project (id),
    trigger_event_id      INTEGER REFERENCES event (id),
    reason                TEXT NOT NULL CHECK (reason IN ('assigned', 'mentioned', 'manual')),
    status                TEXT NOT NULL
        CHECK (status IN ('queued', 'starting', 'running', 'succeeded', 'failed', 'interrupted',
                          'canceled')),
    pid                   INTEGER,
    boot_id               TEXT NOT NULL,
    message               TEXT NOT NULL,
    template_source       TEXT,
    template_id           INTEGER,
    edited                INTEGER NOT NULL DEFAULT 0 CHECK (edited IN (0, 1)),
    key_id                INTEGER REFERENCES api_key (id),
    exit_code             INTEGER,
    error                 TEXT,
    output_tail           TEXT NOT NULL DEFAULT '',
    output_truncated      INTEGER NOT NULL DEFAULT 0 CHECK (output_truncated IN (0, 1)),
    started               INTEGER,
    finished              INTEGER,
    created               INTEGER NOT NULL,
    created_by            INTEGER REFERENCES user (id),
    worktree_path         TEXT,
    worktree_repo         TEXT,
    branch                TEXT,
    base_ref              TEXT,
    base_commit           TEXT,
    worktree_state        TEXT NOT NULL DEFAULT 'none'
        CHECK (worktree_state IN ('none', 'created', 'removed', 'kept')),
    has_changes           INTEGER NOT NULL DEFAULT 0 CHECK (has_changes IN (0, 1)),
    changed_files         INTEGER NOT NULL DEFAULT 0,
    ahead                 INTEGER NOT NULL DEFAULT 0,
    worktree_note         TEXT,
    transcript_bytes      INTEGER NOT NULL DEFAULT 0 CHECK (transcript_bytes >= 0),
    transcript_truncated  INTEGER NOT NULL DEFAULT 0 CHECK (transcript_truncated IN (0, 1)),
    queued_at             INTEGER,
    trigger_comment_id    INTEGER,
    requested_base_branch TEXT,
    CHECK (status <> 'queued' OR queued_at IS NOT NULL)
);

INSERT INTO run (id, agent_id, issue_id, project_id, trigger_event_id, reason, status, pid,
                 boot_id, message, template_source, template_id, edited, key_id, exit_code,
                 error, output_tail, output_truncated, started, finished, created, created_by,
                 worktree_path, worktree_repo, branch, base_ref, base_commit, worktree_state,
                 has_changes, changed_files, ahead, worktree_note, transcript_bytes,
                 transcript_truncated)
SELECT id, agent_id, issue_id, project_id, trigger_event_id, reason, status, pid,
       boot_id, message, template_source, template_id, edited, key_id, exit_code,
       error, output_tail, output_truncated, started, finished, created, created_by,
       worktree_path, worktree_repo, branch, base_ref, base_commit, worktree_state,
       has_changes, changed_files, ahead, worktree_note, transcript_bytes,
       transcript_truncated
FROM run_old;

DROP TABLE run_old;

CREATE UNIQUE INDEX run_once_per_trigger ON run (trigger_event_id, agent_id);

CREATE UNIQUE INDEX run_one_active_per_issue ON run (issue_id)
    WHERE status IN ('queued', 'starting', 'running');

CREATE INDEX run_by_issue ON run (issue_id, id);
CREATE INDEX run_by_agent ON run (agent_id, id);
CREATE INDEX run_by_status ON run (status, id);
CREATE INDEX run_by_changes ON run (has_changes, id) WHERE has_changes = 1;
CREATE INDEX run_queue ON run (queued_at, id) WHERE status = 'queued';
