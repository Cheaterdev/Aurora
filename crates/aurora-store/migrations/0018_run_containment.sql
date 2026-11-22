-- Process containment and start-up reconciliation of runs (TRK-45).
--
--   `pid_started_at`  when the run's process was created, as the operating
--                     system says (Windows: the 100 ns FILETIME of its creation;
--                     Linux: the start time in clock ticks since boot). Written
--                     together with `pid` when the run becomes `running`. The
--                     next start-up only kills a leftover process whose creation
--                     time still equals it, so a reused pid is never touched.
--                     NULL for rows made before this migration and when the
--                     system would not say.
--   `reaped`          what start-up reconciliation found of the process of a run
--                     it marked `interrupted`: "killed leftover process <pid>",
--                     "process already gone", "pid reused, left alone" or "pid
--                     unknown". NULL for every other run.
ALTER TABLE run ADD COLUMN pid_started_at INTEGER;
ALTER TABLE run ADD COLUMN reaped TEXT;
