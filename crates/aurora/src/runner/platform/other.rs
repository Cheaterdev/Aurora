//! Unix: PATH lookup, a process group per agent, group kill.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::Resolved;

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

/// Best effort: kills the agent's process group with `kill`.
pub async fn kill_tree(pid: u32) {
    let status = tokio::process::Command::new("kill")
        .args(["-KILL", "--"])
        .arg(format!("-{pid}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    if let Err(err) = status {
        tracing::warn!(pid, %err, "could not run kill");
    }
}
