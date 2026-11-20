-- Users sign in with a password (agents have none), get sessions, and own API
-- keys. Only SHA-256 hashes of session tokens and API keys are ever stored.

ALTER TABLE user ADD COLUMN password_hash TEXT;
ALTER TABLE user ADD COLUMN role TEXT NOT NULL DEFAULT 'member'
    CHECK (role IN ('owner', 'member', 'agent'));

-- Rows from before this migration only have `kind`.
UPDATE user SET role = 'agent' WHERE kind = 'agent';

-- Sign-in is by name, so names must not collide.
CREATE UNIQUE INDEX user_name_unique ON user (name COLLATE NOCASE);
-- Version 1 is single-owner; the database refuses a second owner outright.
CREATE UNIQUE INDEX user_single_owner ON user (role) WHERE role = 'owner';

CREATE TABLE session (
    id        TEXT PRIMARY KEY CHECK (length(id) = 64),
    user_id   INTEGER NOT NULL REFERENCES user (id),
    created   INTEGER NOT NULL,
    expires   INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);

CREATE INDEX session_by_user ON session (user_id);
CREATE INDEX session_by_expires ON session (expires);

CREATE TABLE api_key (
    id        INTEGER PRIMARY KEY,
    user_id   INTEGER NOT NULL REFERENCES user (id),
    name      TEXT NOT NULL,
    prefix    TEXT NOT NULL,
    key_hash  TEXT NOT NULL UNIQUE CHECK (length(key_hash) = 64),
    scope     TEXT NOT NULL CHECK (scope IN ('read', 'write')),
    created   INTEGER NOT NULL,
    last_used INTEGER,
    revoked   INTEGER
);

CREATE INDEX api_key_by_user ON api_key (user_id);
