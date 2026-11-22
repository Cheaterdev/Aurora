//! Finding and killing the processes a crashed server left behind (TRK-45).
//!
//! At start-up every run that was still `starting` or `running` has a pid and,
//! when the system would say, the time that process was created
//! (`pid_started_at`). A process is only killed when it exists **and** its
//! creation time still equals the recorded one: a pid is reused by the system
//! sooner or later, and killing whatever has the number now would be a bug that
//! looks like a feature. Nothing here ever looks at a process name.
//!
//! The decision is a pure function ([`decide`]) so it is tested without any
//! process; the platform modules only answer "what is at this pid" and "kill
//! the tree at this pid".

use std::time::Duration;

use super::{Inspect, platform};

/// What is to be done about one leftover process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No usable pid, no way to tell the process from a stranger, or the
    /// process cannot be inspected: left alone.
    Unknown,
    Gone,
    /// The pid is in use by a process created at another time.
    Reused,
    Kill(u32),
}

/// What became of it, as recorded on the run (`run.reaped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    Killed(u32),
    /// Verified to be the run's process and still alive after the kill.
    Stuck(u32),
    Gone,
    Reused,
    Unknown,
}

impl Reaped {
    /// The text stored in `run.reaped` and quoted in the issue comment.
    pub fn note(self) -> String {
        match self {
            Reaped::Killed(pid) => format!("killed leftover process {pid}"),
            Reaped::Stuck(pid) => format!("could not kill leftover process {pid}"),
            Reaped::Gone => "process already gone".to_string(),
            Reaped::Reused => "pid reused, left alone".to_string(),
            Reaped::Unknown => "pid unknown".to_string(),
        }
    }
}

/// The rules, given what the system says about the pid (`found` is asked only
/// when there is a pid to ask about).
pub fn decide(
    pid: Option<i64>,
    recorded_start: Option<i64>,
    found: impl FnOnce(u32) -> Inspect,
) -> Decision {
    let Some(pid) = pid
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|p| *p > 0)
    else {
        return Decision::Unknown;
    };
    match found(pid) {
        Inspect::Gone => Decision::Gone,
        Inspect::Unreadable => Decision::Unknown,
        Inspect::Alive(now) => match recorded_start {
            Some(then) if then == now => Decision::Kill(pid),
            Some(_) => Decision::Reused,
            None => Decision::Unknown,
        },
    }
}

/// How long a killed process gets to disappear before it is called stuck.
const GONE_WITHIN: Duration = Duration::from_secs(5);

/// Looks at the run's process and, when it is verifiably the run's own, kills
/// it and whatever it started (the tree, as far as the system can say).
pub async fn reap(pid: Option<i64>, recorded_start: Option<i64>) -> Reaped {
    match decide(pid, recorded_start, platform::inspect) {
        Decision::Unknown => Reaped::Unknown,
        Decision::Gone => Reaped::Gone,
        Decision::Reused => Reaped::Reused,
        Decision::Kill(pid) => {
            platform::kill_tree(pid).await;
            let waited = tokio::time::Instant::now();
            loop {
                if !matches!(platform::inspect(pid), Inspect::Alive(_)) {
                    return Reaped::Killed(pid);
                }
                if waited.elapsed() > GONE_WITHIN {
                    return Reaped::Stuck(pid);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// `/proc/<pid>/stat` to the process state letter and its start time (field
/// 22, clock ticks since boot). The command name is in parentheses and may
/// contain spaces and parentheses itself, so fields are counted after the
/// *last* closing one. Pure, so it is tested on every platform even though
/// only Linux reads such a file.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_proc_stat(text: &str) -> Option<(char, u64)> {
    let after = &text[text.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // `after` starts at field 3 (state); starttime is field 22.
    let start = fields.nth(22 - 3 - 1)?.parse().ok()?;
    Some((state, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(at: i64) -> impl FnOnce(u32) -> Inspect {
        move |_| Inspect::Alive(at)
    }

    #[test]
    fn a_process_is_killed_only_when_its_start_time_matches() {
        assert_eq!(decide(Some(40), Some(7), alive(7)), Decision::Kill(40));
        assert_eq!(decide(Some(40), Some(7), alive(8)), Decision::Reused);
    }

    #[test]
    fn what_cannot_be_verified_is_left_alone() {
        assert_eq!(decide(Some(40), None, alive(7)), Decision::Unknown);
        assert_eq!(
            decide(Some(40), Some(7), |_| Inspect::Unreadable),
            Decision::Unknown
        );
        for pid in [None, Some(0), Some(-3), Some(i64::from(u32::MAX) + 1)] {
            assert_eq!(
                decide(pid, Some(7), |_| panic!("no pid, nothing to ask")),
                Decision::Unknown,
                "{pid:?}"
            );
        }
    }

    #[test]
    fn a_missing_process_is_gone_with_or_without_a_recorded_start() {
        assert_eq!(decide(Some(40), Some(7), |_| Inspect::Gone), Decision::Gone);
        assert_eq!(decide(Some(40), None, |_| Inspect::Gone), Decision::Gone);
    }

    #[test]
    fn notes_are_the_documented_texts() {
        assert_eq!(Reaped::Killed(5).note(), "killed leftover process 5");
        assert_eq!(Reaped::Gone.note(), "process already gone");
        assert_eq!(Reaped::Reused.note(), "pid reused, left alone");
        assert_eq!(Reaped::Unknown.note(), "pid unknown");
        assert_eq!(Reaped::Stuck(5).note(), "could not kill leftover process 5");
    }

    #[test]
    fn proc_stat_is_read_after_the_last_parenthesis() {
        let stat = "1234 (my (odd) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 100 18446744073709551615";
        assert_eq!(parse_proc_stat(stat), Some(('S', 987_654)));
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("1 (x) S 1 2"), None);
    }
}
