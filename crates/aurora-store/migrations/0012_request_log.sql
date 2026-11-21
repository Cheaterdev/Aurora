-- Request ids (TRK-43). One row per successful write that came with a request
-- id, written in the same transaction as the change it records. A repeated id
-- returns `status` and `response_body` instead of applying the change again.
-- `response_body` is NULL when the original response was too large to keep.
-- Rows older than the retention period are purged in batches.
CREATE TABLE request_log (
    principal_user_id INTEGER NOT NULL,
    request_id        TEXT    NOT NULL,
    fingerprint       TEXT    NOT NULL,
    status            INTEGER NOT NULL,
    response_body     TEXT,
    created           INTEGER NOT NULL,
    PRIMARY KEY (principal_user_id, request_id)
);

CREATE INDEX request_log_created ON request_log (created);
