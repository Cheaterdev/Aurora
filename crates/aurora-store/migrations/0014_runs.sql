-- Runs: one start of an agent process on one issue.
--
-- `message`, `output_tail` and `error` may hold secrets (a person's edited
-- message, whatever the process printed): they live in this row only and are
-- never copied into an event.
--
-- `boot_id` names the server process lifetime that owns an active run. A run
-- that is still `starting` or `running` under another boot id was orphaned by a
-- crash and is marked `interrupted` at the next start.
--
-- `key_id` is the short-lived API key minted for the run; it is revoked in the
-- transaction that finishes the run.

CREATE TABLE run (
    id               INTEGER PRIMARY KEY,
    agent_id         INTEGER NOT NULL REFERENCES agent (id),
    issue_id         INTEGER NOT NULL REFERENCES issue (id),
    project_id       INTEGER NOT NULL REFERENCES project (id),
    -- The event that asked for the run; NULL for a manual start.
    trigger_event_id INTEGER REFERENCES event (id),
    reason           TEXT NOT NULL CHECK (reason IN ('assigned', 'mentioned', 'manual')),
    status           TEXT NOT NULL
        CHECK (status IN ('starting', 'running', 'succeeded', 'failed', 'interrupted', 'canceled')),
    pid              INTEGER,
    boot_id          TEXT NOT NULL,
    message          TEXT NOT NULL,
    template_source  TEXT,
    template_id      INTEGER,
    edited           INTEGER NOT NULL DEFAULT 0 CHECK (edited IN (0, 1)),
    key_id           INTEGER REFERENCES api_key (id),
    exit_code        INTEGER,
    error            TEXT,
    output_tail      TEXT NOT NULL DEFAULT '',
    output_truncated INTEGER NOT NULL DEFAULT 0 CHECK (output_truncated IN (0, 1)),
    started          INTEGER,
    finished         INTEGER,
    created          INTEGER NOT NULL,
    created_by       INTEGER REFERENCES user (id)
);

-- An event redelivered to the runner never makes a second run. NULLs (manual
-- runs) are distinct in a unique index.
CREATE UNIQUE INDEX run_once_per_trigger ON run (trigger_event_id, agent_id);

-- One active run per issue, enforced by the database.
CREATE UNIQUE INDEX run_one_active_per_issue ON run (issue_id)
    WHERE status IN ('starting', 'running');

CREATE INDEX run_by_issue ON run (issue_id, id);
CREATE INDEX run_by_agent ON run (agent_id, id);
CREATE INDEX run_by_status ON run (status, id);
