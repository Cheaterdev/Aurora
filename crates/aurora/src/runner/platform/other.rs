//! Unix: PATH lookup, a process group per agent, group kill.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{Inspect, Resolved};

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn resolve_command(
    command: &str,
    path_var: Option<OsString>,
    _pathext: Option<String>,
) -> Result<Resolved, String> {
    let has_dir = command.contains('/');
    let found: Option<PathBuf> = if has_dir {
        Some(PathBuf::from(command)).filter(|p| is_executable(p))
    } else {
        std::env::split_paths(&path_var.unwrap_or_default())
            .map(|dir| dir.join(command))
            .find(|p| is_executable(p))
    };
    found
        .map(|path| Resolved { path, shim: false })
        .ok_or_else(|| {
            format!(
                "the command {command:?} was not found{}",
                if has_dir { "" } else { " in PATH" }
            )
        })
}

/// The agent leads its own process group, so the group can be killed whole.
pub fn prepare(command: &mut tokio::process::Command) {
    command.process_group(0);
}

/// How one run's process tree is held: the agent leads a process group (set
/// by [`prepare`]) and the group is killed whole. Unlike a Windows job this
/// does not die with the server (no `PR_SET_PDEATHSIG`, no cgroup) and a
/// descendant that calls `setsid` or double-forks leaves the group. That is
/// what start-up reconciliation (`runner::reap`) is for, on a best-effort
/// basis. **Not run on the machine this was written on (Windows).**
pub struct Contained;

impl Contained {
    /// For logs and tests.
    pub fn kind(&self) -> &'static str {
        "process group"
    }

    pub async fn kill(&self, pid: u32) {
        kill_tree(pid).await;
    }

    /// The agent ended by itself. Nothing is done: the group may be empty and
    /// its number reusable by then, so signalling it blindly is not safe.
    pub fn release(&self) {}
}

pub fn spawn(
    command: &mut tokio::process::Command,
) -> std::io::Result<(tokio::process::Child, Contained)> {
    Ok((command.spawn()?, Contained))
}

/// What is at `pid`, from `/proc` (Linux). A system without `/proc` cannot
/// tell, which makes start-up reconciliation leave the process alone.
pub fn inspect(pid: u32) -> Inspect {
    if !Path::new("/proc/self/stat").exists() {
        return Inspect::Unreadable;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => match super::reap::parse_proc_stat(&text) {
            Some(('Z' | 'X', _)) => Inspect::Gone,
            Some((_, start)) => i64::try_from(start).map_or(Inspect::Unreadable, Inspect::Alive),
            None => Inspect::Unreadable,
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Inspect::Gone,
        Err(_) => Inspect::Unreadable,
    }
}

async fn signal(group: &str, name: &str) -> bool {
    tokio::process::Command::new("kill")
        .arg(name)
        .arg("--")
        .arg(group)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Best effort: asks the agent's process group to end (`SIGTERM`), gives it a
/// moment, then kills what is left (`SIGKILL`), with the `kill` program (no
/// shell, no libc).
pub async fn kill_tree(pid: u32) {
    let group = format!("-{pid}");
    if !signal(&group, "-TERM").await {
        // No such group (or no `kill`): try the plain process once.
        let _ = signal(&pid.to_string(), "-KILL").await;
        return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = signal(&group, "-KILL").await;
}
