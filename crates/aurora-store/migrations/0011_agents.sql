-- The agent registry: how an agent user is run (command, arguments, model,
-- limits), when (assign and mention triggers) and where (allowed projects).
--
-- An agent row never holds a secret. Environment variables with secrets are
-- out of scope; the agent's API keys are separate rows in `api_key`.
--
-- `agent_project` is an explicit allow-list: an agent with no rows there may
-- access NO project. Users with role 'agent' that have no `agent` row stay
-- valid users; they are simply not registered and not restricted.

CREATE TABLE agent (
    id              INTEGER PRIMARY KEY,
    user_id         INTEGER NOT NULL UNIQUE REFERENCES user (id),
    description     TEXT NOT NULL DEFAULT '',
    command         TEXT NOT NULL CHECK (length(command) BETWEEN 1 AND 500),
    args            TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(args) AND json_type(args) = 'array'),
    model           TEXT,
    instructions    TEXT NOT NULL DEFAULT '',
    work_dir        TEXT,
    max_concurrent  INTEGER NOT NULL DEFAULT 1 CHECK (max_concurrent BETWEEN 1 AND 16),
    timeout_seconds INTEGER NOT NULL DEFAULT 3600 CHECK (timeout_seconds BETWEEN 60 AND 86400),
    on_assign       INTEGER NOT NULL DEFAULT 1 CHECK (on_assign IN (0, 1)),
    on_mention      INTEGER NOT NULL DEFAULT 1 CHECK (on_mention IN (0, 1)),
    enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created         INTEGER NOT NULL,
    updated         INTEGER NOT NULL
);

CREATE TABLE agent_project (
    agent_id   INTEGER NOT NULL REFERENCES agent (id) ON DELETE CASCADE,
    project_id INTEGER NOT NULL REFERENCES project (id),
    PRIMARY KEY (agent_id, project_id)
) WITHOUT ROWID;

CREATE INDEX agent_project_by_project ON agent_project (project_id);

CREATE TRIGGER agent_user_must_be_an_agent BEFORE INSERT ON agent
WHEN (SELECT role FROM user WHERE id = NEW.user_id) IS NOT 'agent'
BEGIN
    SELECT RAISE(ABORT, 'an agent row needs a user with role agent');
END;
