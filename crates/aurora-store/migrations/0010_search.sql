-- Full-text search over issues, live comments and documents: one FTS5 table
-- kept current by triggers, so every write path (and any future one) updates
-- the index in the same transaction as the change.
--
-- One row per searchable thing. The rowid is `ref_id * 4 + kind code`
-- (0 issue, 1 comment, 2 document), which makes the triggers' deletes and
-- replaces direct rowid lookups: FTS5 has no index on its UNINDEXED columns.
-- `ident` holds an issue's identifier (`TRK-12`), so `trk 12` finds it by
-- words; the store adds an exact lookup for the `KEY-n` shape on top.
-- `archived` is kept so archived issues and documents can be left out by
-- default; the comments of an archived issue follow its flag.
-- A comment is indexed while it is live (`deleted IS NULL`). The indexed text
-- has the control characters 0x01 and 0x02 replaced by spaces: the search
-- code uses them as match markers around snippets and highlights.
-- `search_rebuild` in the store repeats the three INSERT statements below.

CREATE VIRTUAL TABLE search_index USING fts5(
    kind UNINDEXED,
    project_id UNINDEXED,
    ref_id UNINDEXED,
    archived UNINDEXED,
    title,
    body,
    ident,
    tokenize = 'unicode61 remove_diacritics 2',
    prefix = '2 3'
);

-- ---- issues ----

CREATE TRIGGER issue_search_insert AFTER INSERT ON issue
BEGIN
    INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
    SELECT NEW.id * 4, 'issue', NEW.project_id, NEW.id, NEW.archived,
           replace(replace(NEW.title, char(1), ' '), char(2), ' '),
           replace(replace(NEW.description, char(1), ' '), char(2), ' '),
           p.key || '-' || NEW.number
    FROM project p WHERE p.id = NEW.project_id;
END;

CREATE TRIGGER issue_search_update
AFTER UPDATE OF title, description, archived, project_id, number ON issue
WHEN OLD.title IS NOT NEW.title OR OLD.description IS NOT NEW.description
    OR OLD.archived IS NOT NEW.archived OR OLD.project_id IS NOT NEW.project_id
    OR OLD.number IS NOT NEW.number
BEGIN
    INSERT OR REPLACE INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
    SELECT NEW.id * 4, 'issue', NEW.project_id, NEW.id, NEW.archived,
           replace(replace(NEW.title, char(1), ' '), char(2), ' '),
           replace(replace(NEW.description, char(1), ' '), char(2), ' '),
           p.key || '-' || NEW.number
    FROM project p WHERE p.id = NEW.project_id;
END;

CREATE TRIGGER issue_search_scope AFTER UPDATE OF archived, project_id ON issue
WHEN OLD.archived IS NOT NEW.archived OR OLD.project_id IS NOT NEW.project_id
BEGIN
    UPDATE search_index SET archived = NEW.archived, project_id = NEW.project_id
    WHERE rowid IN (SELECT id * 4 + 1 FROM comment WHERE issue_id = NEW.id AND deleted IS NULL);
END;

CREATE TRIGGER issue_search_delete BEFORE DELETE ON issue
BEGIN
    DELETE FROM search_index
    WHERE rowid IN (SELECT id * 4 + 1 FROM comment WHERE issue_id = OLD.id);
    DELETE FROM search_index WHERE rowid = OLD.id * 4;
END;

-- ---- comments ----

CREATE TRIGGER comment_search_insert AFTER INSERT ON comment
WHEN NEW.deleted IS NULL
BEGIN
    INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
    SELECT NEW.id * 4 + 1, 'comment', i.project_id, NEW.id, i.archived, '',
           replace(replace(NEW.body, char(1), ' '), char(2), ' '), ''
    FROM issue i WHERE i.id = NEW.issue_id;
END;

CREATE TRIGGER comment_search_update AFTER UPDATE OF body, deleted, issue_id ON comment
WHEN NEW.deleted IS NULL
    AND (OLD.body IS NOT NEW.body OR OLD.deleted IS NOT NULL OR OLD.issue_id IS NOT NEW.issue_id)
BEGIN
    INSERT OR REPLACE INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
    SELECT NEW.id * 4 + 1, 'comment', i.project_id, NEW.id, i.archived, '',
           replace(replace(NEW.body, char(1), ' '), char(2), ' '), ''
    FROM issue i WHERE i.id = NEW.issue_id;
END;

CREATE TRIGGER comment_search_remove AFTER UPDATE OF deleted ON comment
WHEN NEW.deleted IS NOT NULL
BEGIN
    DELETE FROM search_index WHERE rowid = NEW.id * 4 + 1;
END;

CREATE TRIGGER comment_search_delete AFTER DELETE ON comment
BEGIN
    DELETE FROM search_index WHERE rowid = OLD.id * 4 + 1;
END;

-- ---- documents ----

CREATE TRIGGER document_search_insert AFTER INSERT ON document
BEGIN
    INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
    VALUES (NEW.id * 4 + 2, 'document', NEW.project_id, NEW.id, NEW.archived,
            replace(replace(NEW.title, char(1), ' '), char(2), ' '),
            replace(replace(NEW.body, char(1), ' '), char(2), ' '), '');
END;

CREATE TRIGGER document_search_update
AFTER UPDATE OF title, body, archived, project_id ON document
WHEN OLD.title IS NOT NEW.title OR OLD.body IS NOT NEW.body
    OR OLD.archived IS NOT NEW.archived OR OLD.project_id IS NOT NEW.project_id
BEGIN
    INSERT OR REPLACE INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
    VALUES (NEW.id * 4 + 2, 'document', NEW.project_id, NEW.id, NEW.archived,
            replace(replace(NEW.title, char(1), ' '), char(2), ' '),
            replace(replace(NEW.body, char(1), ' '), char(2), ' '), '');
END;

CREATE TRIGGER document_search_delete AFTER DELETE ON document
BEGIN
    DELETE FROM search_index WHERE rowid = OLD.id * 4 + 2;
END;

-- ---- backfill ----

INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
SELECT i.id * 4, 'issue', i.project_id, i.id, i.archived,
       replace(replace(i.title, char(1), ' '), char(2), ' '),
       replace(replace(i.description, char(1), ' '), char(2), ' '),
       p.key || '-' || i.number
FROM issue i JOIN project p ON p.id = i.project_id;

INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
SELECT c.id * 4 + 1, 'comment', i.project_id, c.id, i.archived, '',
       replace(replace(c.body, char(1), ' '), char(2), ' '), ''
FROM comment c JOIN issue i ON i.id = c.issue_id
WHERE c.deleted IS NULL;

INSERT INTO search_index (rowid, kind, project_id, ref_id, archived, title, body, ident)
SELECT d.id * 4 + 2, 'document', d.project_id, d.id, d.archived,
       replace(replace(d.title, char(1), ' '), char(2), ' '),
       replace(replace(d.body, char(1), ' '), char(2), ' '), ''
FROM document d;
