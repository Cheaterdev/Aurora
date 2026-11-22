//! Failpoints: named places in the write path where a test can kill the
//! process (as power loss would) to prove that no acknowledged change is lost
//! and no half-applied change is visible. This module is the one list of them.
//!
//! | name                           | where it fires                                                            | what a restart must show                                        |
//! |--------------------------------|---------------------------------------------------------------------------|-----------------------------------------------------------------|
//! | `before_commit`                | writer thread, change and event written, `COMMIT` not yet issued          | nothing of the change; a retry with the same request id applies |
//! | `after_commit`                 | writer thread, right after `COMMIT`, before the caller is told            | the whole change and its event, request id recorded             |
//! | `after_commit_before_response` | HTTP layer, change committed, response not yet written                    | same as `after_commit`; a retry is replayed, not applied twice  |
//! | `before_event_delivery`        | live stream / consumer poll: events read, none sent to the client yet     | events still in the log; a consumer that never acked sees them again |
//! | `mid_migration`                | inside a migration's transaction, SQL run, version row not recorded       | old schema intact; the next start migrates once, backup exists  |
//! | `mid_ack`                      | consumer ack transaction, cursor updated, `COMMIT` not yet issued         | the cursor where it was; the events are delivered again         |
//!
//! | `run_after_create`             | runner, run row committed as `starting`, process not spawned              | the run is `interrupted`, key revoked, no second run for the event |
//! | `run_after_worktree`           | runner, working copy made and recorded, process not spawned               | the run is `interrupted`, working copy `kept` and still on disk |
//! | `run_after_spawn`              | runner, process spawned, run not yet marked `running`                     | same; the stray process is for TRK-45 to reap                   |
//! | `run_before_finish`            | runner, process exited, the finish not yet committed                      | same                                                            |
//!
//! `before_commit` and `after_commit` are in [`crate::Store::write`], so they
//! cover every write of the server except the start-up and hourly purge of old
//! request ids (maintenance, deliberately left out so that a hit count counts
//! writes a client asked for).
//!
//! # Arming
//!
//! Set `AURORA_TEST_FAILPOINT=<name>` to abort on the first hit, or
//! `<name>:<n>` to abort on the n-th hit (n counts only hits of that point in
//! this process, starting at 1). Only one failpoint is armed per process. The
//! process ends with `std::process::abort`: no destructors, no flushing.
//!
//! # Not in release builds
//!
//! [`hit`] does something only when this crate is built for its own tests or
//! with the `failpoints` cargo feature (the `aurora` crate enables it for its
//! dev-dependency, i.e. for `cargo test`). A plain `cargo build` or
//! `cargo build --release` compiles `hit` to nothing and the environment
//! variable name is not in the binary (see docs/operations.md for the check).

/// The places a test can kill the process; see the table in the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    BeforeCommit,
    AfterCommit,
    AfterCommitBeforeResponse,
    BeforeEventDelivery,
    MidMigration,
    MidAck,
    RunAfterCreate,
    RunAfterWorktree,
    RunAfterSpawn,
    RunBeforeFinish,
}

impl Point {
    pub const ALL: [Point; 10] = [
        Point::BeforeCommit,
        Point::AfterCommit,
        Point::AfterCommitBeforeResponse,
        Point::BeforeEventDelivery,
        Point::MidMigration,
        Point::MidAck,
        Point::RunAfterCreate,
        Point::RunAfterWorktree,
        Point::RunAfterSpawn,
        Point::RunBeforeFinish,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Point::BeforeCommit => "before_commit",
            Point::AfterCommit => "after_commit",
            Point::AfterCommitBeforeResponse => "after_commit_before_response",
            Point::BeforeEventDelivery => "before_event_delivery",
            Point::MidMigration => "mid_migration",
            Point::MidAck => "mid_ack",
            Point::RunAfterCreate => "run_after_create",
            Point::RunAfterWorktree => "run_after_worktree",
            Point::RunAfterSpawn => "run_after_spawn",
            Point::RunBeforeFinish => "run_before_finish",
        }
    }

    pub fn parse(name: &str) -> Option<Point> {
        Point::ALL.into_iter().find(|point| point.name() == name)
    }
}

/// `name` or `name:n` (n of 1 or more) to the point and the hit that kills.
pub fn parse_spec(spec: &str) -> Option<(Point, u64)> {
    let (name, hit) = match spec.split_once(':') {
        Some((name, count)) => (name, count.parse::<u64>().ok().filter(|n| *n >= 1)?),
        None => (spec, 1),
    };
    Some((Point::parse(name)?, hit))
}

/// `true` when this build can kill the process at a failpoint.
pub const fn compiled_in() -> bool {
    cfg!(any(test, feature = "failpoints"))
}

#[cfg(any(test, feature = "failpoints"))]
mod armed {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Point, parse_spec};

    /// The environment variable that arms a failpoint, see the module docs.
    pub const ENV: &str = "AURORA_TEST_FAILPOINT";

    static ARMED: OnceLock<Option<(Point, u64)>> = OnceLock::new();
    static HITS: AtomicU64 = AtomicU64::new(0);

    pub fn hit(point: Point) {
        let armed = ARMED.get_or_init(|| std::env::var(ENV).ok().and_then(|s| parse_spec(&s)));
        let Some((wanted, nth)) = *armed else {
            return;
        };
        if wanted == point && HITS.fetch_add(1, Ordering::SeqCst) + 1 == nth {
            std::process::abort();
        }
    }
}

/// Kills the process here if `point` is armed and this is the hit asked for.
#[cfg(any(test, feature = "failpoints"))]
pub fn hit(point: Point) {
    armed::hit(point);
}

#[cfg(not(any(test, feature = "failpoints")))]
#[inline(always)]
pub fn hit(_point: Point) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_name_a_point_and_an_optional_hit() {
        assert_eq!(
            parse_spec("after_commit"),
            Some((Point::AfterCommit, 1)),
            "no count means the first hit"
        );
        assert_eq!(parse_spec("mid_ack:17"), Some((Point::MidAck, 17)));
        assert_eq!(parse_spec("mid_ack:0"), None);
        assert_eq!(parse_spec("mid_ack:x"), None);
        assert_eq!(parse_spec("nope"), None);
        assert_eq!(parse_spec(""), None);
    }

    #[test]
    fn every_point_round_trips_through_its_name() {
        for point in Point::ALL {
            assert_eq!(Point::parse(point.name()), Some(point));
        }
    }

    #[test]
    fn the_module_docs_list_every_point() {
        let source = include_str!("failpoints.rs");
        for point in Point::ALL {
            assert!(
                source.contains(&format!("| `{}`", point.name())),
                "{point:?}"
            );
        }
    }
}
