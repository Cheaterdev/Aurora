-- Threaded comments on issues. A reply points at a top-level comment of the
-- same issue (one level of nesting, enforced by the store). Deleting is soft:
-- `deleted` is set and reads blank the body, so replies stay attached.
-- `comment_mention` holds the users a body mentions, resolved when the comment
-- was written or last edited; `position` is the order of first appearance.

CREATE TABLE comment (
    id        INTEGER PRIMARY KEY,
    issue_id  INTEGER NOT NULL REFERENCES issue (id) ON DELETE CASCADE,
    parent_id INTEGER REFERENCES comment (id),
    author_id INTEGER NOT NULL REFERENCES user (id),
    body      TEXT NOT NULL,
    created   INTEGER NOT NULL,
    edited    INTEGER,
    deleted   INTEGER,
    CHECK (parent_id IS NULL OR parent_id != id)
);

CREATE INDEX comment_by_issue ON comment (issue_id, id);
CREATE INDEX comment_by_parent ON comment (parent_id);

CREATE TABLE comment_mention (
    comment_id INTEGER NOT NULL REFERENCES comment (id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES user (id),
    position   INTEGER NOT NULL,
    PRIMARY KEY (comment_id, user_id)
);

CREATE INDEX comment_mention_by_user ON comment_mention (user_id);
