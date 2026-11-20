-- Markdown documents of one project, nested under a parent, with an
-- append-only revision history and links to issues and initiatives. That a
-- parent, an issue or an initiative belongs to the document's project is
-- enforced by the store. `position` orders siblings (same scheme as issues);
-- `revision` is the number of the newest row in `document_revision`, which
-- holds every title and body the document has had.

CREATE TABLE document (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES project (id),
    parent_id  INTEGER REFERENCES document (id),
    title      TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    position   REAL NOT NULL,
    revision   INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created    INTEGER NOT NULL,
    updated    INTEGER NOT NULL,
    created_by INTEGER REFERENCES user (id),
    updated_by INTEGER REFERENCES user (id),
    archived   INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1))
);

CREATE INDEX document_by_project ON document (project_id, parent_id, position);
CREATE INDEX document_by_parent ON document (parent_id);

CREATE TABLE document_revision (
    id          INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL REFERENCES document (id),
    number      INTEGER NOT NULL CHECK (number >= 1),
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    author_id   INTEGER REFERENCES user (id),
    created     INTEGER NOT NULL,
    note        TEXT,
    UNIQUE (document_id, number)
);

CREATE TABLE document_link (
    id            INTEGER PRIMARY KEY,
    document_id   INTEGER NOT NULL REFERENCES document (id),
    issue_id      INTEGER REFERENCES issue (id),
    initiative_id INTEGER REFERENCES initiative (id),
    created       INTEGER NOT NULL,
    created_by    INTEGER REFERENCES user (id),
    CHECK ((issue_id IS NULL) <> (initiative_id IS NULL)),
    UNIQUE (document_id, issue_id),
    UNIQUE (document_id, initiative_id)
);

CREATE INDEX document_link_by_issue ON document_link (issue_id);
CREATE INDEX document_link_by_initiative ON document_link (initiative_id);
