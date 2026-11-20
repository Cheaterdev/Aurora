-- Links between two issues of one project. `blocks` is directed (from blocks
-- to); `relates` is symmetric and stored once, lower issue id first, so the
-- same pair cannot be linked twice in opposite directions.

CREATE TABLE issue_link (
    id            INTEGER PRIMARY KEY,
    from_issue_id INTEGER NOT NULL REFERENCES issue (id) ON DELETE CASCADE,
    to_issue_id   INTEGER NOT NULL REFERENCES issue (id) ON DELETE CASCADE,
    kind          TEXT NOT NULL CHECK (kind IN ('blocks', 'relates')),
    created       INTEGER NOT NULL,
    created_by    INTEGER REFERENCES user (id),
    CHECK (from_issue_id != to_issue_id),
    CHECK (kind != 'relates' OR from_issue_id < to_issue_id),
    UNIQUE (kind, from_issue_id, to_issue_id)
);

CREATE INDEX issue_link_by_from ON issue_link (from_issue_id, kind);
CREATE INDEX issue_link_by_to ON issue_link (to_issue_id, kind);
