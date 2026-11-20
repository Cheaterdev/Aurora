-- Initiatives group the issues of one project and report progress. An issue
-- belongs to at most one initiative; that it is one of the same project is
-- enforced by the store, which also unassigns the issues before it deletes an
-- initiative. `target_date` is a calendar date (YYYY-MM-DD, no time zone).

CREATE TABLE initiative (
    id          INTEGER PRIMARY KEY,
    project_id  INTEGER NOT NULL REFERENCES project (id),
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'planned'
        CHECK (status IN ('planned', 'active', 'paused', 'completed', 'canceled')),
    target_date TEXT
        CHECK (target_date IS NULL
               OR target_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

ALTER TABLE issue ADD COLUMN initiative_id INTEGER REFERENCES initiative (id);

CREATE INDEX issue_by_initiative ON issue (initiative_id);
