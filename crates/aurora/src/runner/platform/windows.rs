//! Windows: PATHEXT lookup, `.cmd`/`.bat` shims, no console window, tree kill.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::Resolved;

/// `CREATE_NO_WINDOW`: the agent never gets a console window of its own.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn classify(path: &Path) -> Result<Resolved, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "exe" | "com" => Ok(Resolved {
            path: path.to_path_buf(),
            shim: false,
        }),
        // Batch files run through cmd.exe; Rust escapes their arguments for it
        // and refuses the ones it cannot escape safely.
        "bat" | "cmd" => Ok(Resolved {
            path: path.to_path_buf(),
            shim: true,
        }),
        other => Err(format!(
            "{} is not an executable (.exe, .com, .cmd or .bat), its extension is {other:?}",
            path.display()
        )),
    }
}

fn candidates(base: &Path, pathext: &[String]) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if base.extension().is_some() {
        list.push(base.to_path_buf());
    }
    for ext in pathext {
        let mut name: OsString = base.as_os_str().to_os_string();
        name.push(ext);
        list.push(PathBuf::from(name));
    }
    list
}

pub fn resolve_command(
    command: &str,
    path_var: Option<OsString>,
    pathext: Option<String>,
) -> Result<Resolved, String> {
    let pathext: Vec<String> = pathext
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|e| e.starts_with('.'))
        .map(str::to_string)
        .collect();
    let has_dir = command.contains(['\\', '/', ':']);
    let bases: Vec<PathBuf> = if has_dir {
        vec![PathBuf::from(command)]
    } else {
        std::env::split_paths(&path_var.unwrap_or_default())
            .map(|dir| dir.join(command))
            .collect()
    };
    for base in bases {
        for candidate in candidates(&base, &pathext) {
            if candidate.is_file() {
                return classify(&candidate);
            }
        }
    }
    Err(format!(
        "the command {command:?} was not found{}",
        if has_dir { "" } else { " in PATH" }
    ))
}

/// No console window; the child is not attached to the server's console.
pub fn prepare(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Best effort: `taskkill /T /F` ends the process and the processes it
/// started. TRK-45 replaces this with a job object.
pub async fn kill_tree(pid: u32) {
    let mut taskkill = tokio::process::Command::new("taskkill");
    taskkill
        .args(["/T", "/F", "/PID"])
        .arg(pid.to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match taskkill.status().await {
        Ok(_) => {}
        Err(err) => tracing::warn!(pid, %err, "could not run taskkill"),
    }
}
