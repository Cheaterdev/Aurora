CREATE TABLE user (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL,
    email    TEXT UNIQUE,
    kind     TEXT NOT NULL CHECK (kind IN ('human', 'agent')),
    disabled INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1))
);

CREATE TABLE project (
    id                INTEGER PRIMARY KEY,
    key               TEXT NOT NULL UNIQUE,
    name              TEXT NOT NULL,
    created           INTEGER NOT NULL,
    next_issue_number INTEGER NOT NULL DEFAULT 1 CHECK (next_issue_number >= 1)
);

CREATE TABLE "column" (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project (id),
    name       TEXT NOT NULL,
    state_type TEXT NOT NULL
        CHECK (state_type IN ('backlog', 'unstarted', 'started', 'completed', 'canceled')),
    position   INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

CREATE TABLE issue_type (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE issue (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES project (id),
    number      INTEGER NOT NULL CHECK (number >= 1),
    type_id     INTEGER NOT NULL REFERENCES issue_type (id),
    column_id   INTEGER NOT NULL REFERENCES "column" (id),
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    assignee_id INTEGER REFERENCES user (id),
    parent_id   INTEGER REFERENCES issue (id),
    position    REAL NOT NULL,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    UNIQUE (project_id, number)
);

CREATE INDEX issue_by_column ON issue (column_id, position);
CREATE INDEX issue_by_parent ON issue (parent_id);
CREATE INDEX issue_by_assignee ON issue (assignee_id);

CREATE TABLE event (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    time         INTEGER NOT NULL,
    actor_id     INTEGER REFERENCES user (id),
    project_id   INTEGER REFERENCES project (id),
    entity_kind  TEXT NOT NULL,
    entity_id    INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json))
);

CREATE INDEX event_by_entity ON event (entity_kind, entity_id);
CREATE INDEX event_by_project ON event (project_id, id);

CREATE TRIGGER event_no_update BEFORE UPDATE ON event
BEGIN
    SELECT RAISE(ABORT, 'event is append-only');
END;

CREATE TRIGGER event_no_delete BEFORE DELETE ON event
BEGIN
    SELECT RAISE(ABORT, 'event is append-only');
END;
