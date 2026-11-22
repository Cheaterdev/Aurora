-- A working copy per run (TRK-16).
--
-- The repository belongs to the project: `repo_path` is the absolute path of an
-- existing local git repository (its top-level folder), `base_branch` the branch
-- runs start from. NULL base_branch means "the repository's default", resolved
-- when a run starts and recorded on the run. Neither is ever copied into an
-- event (the events say only that they changed) and neither is shown to agents.
ALTER TABLE project ADD COLUMN repo_path TEXT;
ALTER TABLE project ADD COLUMN base_branch TEXT;

-- What a run knows about its working copy.
--
-- `worktree_state`: 'none' (no working copy: an explicit agent work_dir, a
-- project without a repository, or a start that failed before one existed),
-- 'created' (planned or created, not yet settled: the run is still going or the
-- server stopped before it could look), 'removed' (the run ended without
-- changes and the folder and branch were deleted), 'kept' (changes, or not
-- sure: the folder and branch stay for a person).
--
-- `worktree_repo` is the repository the working copy belongs to, copied from the
-- project when the run was created, so cleaning up still works after the
-- project's setting changed. It is not part of the run JSON.
-- `base_ref` is the branch name asked for or resolved; `base_commit` the full
-- SHA the working copy started from. `has_changes`, `changed_files` and
-- `ahead` are what the run left behind (uncommitted or untracked files; commits
-- beyond `base_commit`).
ALTER TABLE run ADD COLUMN worktree_path TEXT;
ALTER TABLE run ADD COLUMN worktree_repo TEXT;
ALTER TABLE run ADD COLUMN branch TEXT;
ALTER TABLE run ADD COLUMN base_ref TEXT;
ALTER TABLE run ADD COLUMN base_commit TEXT;
ALTER TABLE run ADD COLUMN worktree_state TEXT NOT NULL DEFAULT 'none'
    CHECK (worktree_state IN ('none', 'created', 'removed', 'kept'));
ALTER TABLE run ADD COLUMN has_changes INTEGER NOT NULL DEFAULT 0
    CHECK (has_changes IN (0, 1));
ALTER TABLE run ADD COLUMN changed_files INTEGER NOT NULL DEFAULT 0;
ALTER TABLE run ADD COLUMN ahead INTEGER NOT NULL DEFAULT 0;
ALTER TABLE run ADD COLUMN worktree_note TEXT;

CREATE INDEX run_by_changes ON run (has_changes, id) WHERE has_changes = 1;
