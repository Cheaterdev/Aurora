-- Where each event consumer (webhook, runner, extension, live page) has read
-- up to. Kept in the same database so a cursor can never be ahead of, or
-- survive without, the events it points into.
CREATE TABLE consumer_cursor (
    name          TEXT PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 100),
    last_event_id INTEGER NOT NULL DEFAULT 0 CHECK (last_event_id >= 0),
    updated       INTEGER NOT NULL
);

-- INSERT OR REPLACE deletes the row it replaces without firing the DELETE
-- trigger from 0001, which would let an event be overwritten.
CREATE TRIGGER event_no_replace BEFORE INSERT ON event
WHEN EXISTS (SELECT 1 FROM event WHERE id = NEW.id)
BEGIN
    SELECT RAISE(ABORT, 'event is append-only');
END;
