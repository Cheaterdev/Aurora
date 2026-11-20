-- issue_type becomes per project. SQLite cannot add a NOT NULL column that
-- references another table in place, so the table is rebuilt with ids kept.
-- Migrations run inside a transaction, where PRAGMA foreign_keys cannot be
-- switched off. Deferring lets issue.type_id dangle while the table is
-- recreated; the old rows are re-inserted into the table that keeps the name
-- issue_type (not renamed into it), because only inserting parent rows clears
-- the deferred violations that DROP TABLE records. Anything still violated at
-- COMMIT fails the migration.
PRAGMA defer_foreign_keys = ON;

-- A type goes to the project of the first issue that uses it, otherwise to
-- the lowest-id project. With no project at all no issue can exist either, so
-- such a type has no owner and is dropped.
CREATE TEMP TABLE issue_type_old AS
SELECT t.id AS id,
       t.name AS name,
       COALESCE(
           (SELECT i.project_id FROM issue i WHERE i.type_id = t.id ORDER BY i.id LIMIT 1),
           (SELECT MIN(p.id) FROM project p)
       ) AS project_id
FROM issue_type t;

DROP TABLE issue_type;

CREATE TABLE issue_type (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project (id),
    name       TEXT NOT NULL,
    UNIQUE (project_id, name)
);

INSERT INTO issue_type (id, project_id, name)
SELECT id, project_id, name FROM issue_type_old WHERE project_id IS NOT NULL;

DROP TABLE issue_type_old;

CREATE TABLE label (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project (id),
    name       TEXT NOT NULL,
    color      TEXT NOT NULL,
    UNIQUE (project_id, name)
);

CREATE TABLE issue_label (
    issue_id INTEGER NOT NULL REFERENCES issue (id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES label (id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);

CREATE INDEX issue_label_by_label ON issue_label (label_id);
