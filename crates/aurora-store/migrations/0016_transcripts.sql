-- Live transcript (TRK-17).
--
-- The agent's output is stored in full (up to a cap) in an append-only file per
-- run, `<data folder>/transcripts/<run id>.log`, written by the runner while the
-- process runs. The database does not hold it (it is large and changes all the
-- time) and no event is written per chunk. It only records what the file held
-- when the run ended:
--
-- `transcript_bytes`: the size of the file once the runner finished it (and
-- synced it). 0 means "not recorded": the run produced no file, or the server
-- stopped hard before it could record the size (the file, if there, is still
-- readable up to its last complete line).
-- `transcript_truncated`: the cap was reached and the rest of the output was
-- not kept in the file (the `output_tail` on the run still has the last bytes).
ALTER TABLE run ADD COLUMN transcript_bytes INTEGER NOT NULL DEFAULT 0
    CHECK (transcript_bytes >= 0);
ALTER TABLE run ADD COLUMN transcript_truncated INTEGER NOT NULL DEFAULT 0
    CHECK (transcript_truncated IN (0, 1));
