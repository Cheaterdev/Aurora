-- Prompt templates (TRK-50): the first message of a run, editable.
--
-- An agent's DEFAULT template is `agent.instructions` and is not copied here.
-- This table only holds OVERRIDES: one per (agent, project) and one per
-- (agent, project, issue type). Issue types belong to a project, so an
-- override always names a project; there is no agent-wide override row (that is
-- what `instructions` is). The unique index treats "no issue type" as one
-- value, which a plain UNIQUE constraint would not (NULLs are all distinct).
--
-- The body is template text and may hold anything the owner pasted, so events
-- never carry it (only its length), like the agent's instructions.
--
-- `project.rules` is the project's own rules text, shown to agents by the
-- default template. NULL means none; an empty string is never stored.

ALTER TABLE project ADD COLUMN rules TEXT
    CHECK (rules IS NULL OR (length(rules) BETWEEN 1 AND 10000));

CREATE TABLE prompt_template (
    id            INTEGER PRIMARY KEY,
    agent_id      INTEGER NOT NULL REFERENCES agent (id) ON DELETE CASCADE,
    project_id    INTEGER NOT NULL REFERENCES project (id),
    issue_type_id INTEGER REFERENCES issue_type (id),
    body          TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 20000),
    created       INTEGER NOT NULL,
    updated       INTEGER NOT NULL
);

CREATE UNIQUE INDEX prompt_template_scope
    ON prompt_template (agent_id, project_id, COALESCE(issue_type_id, 0));

CREATE INDEX prompt_template_by_type ON prompt_template (issue_type_id);
CREATE INDEX prompt_template_by_project ON prompt_template (project_id);

CREATE TRIGGER prompt_template_type_in_project_insert BEFORE INSERT ON prompt_template
WHEN NEW.issue_type_id IS NOT NULL
 AND (SELECT project_id FROM issue_type WHERE id = NEW.issue_type_id) IS NOT NEW.project_id
BEGIN
    SELECT RAISE(ABORT, 'the issue type belongs to another project');
END;

CREATE TRIGGER prompt_template_type_in_project_update BEFORE UPDATE ON prompt_template
WHEN NEW.issue_type_id IS NOT NULL
 AND (SELECT project_id FROM issue_type WHERE id = NEW.issue_type_id) IS NOT NEW.project_id
BEGIN
    SELECT RAISE(ABORT, 'the issue type belongs to another project');
END;
