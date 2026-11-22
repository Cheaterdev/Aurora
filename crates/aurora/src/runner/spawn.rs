//! Turning an agent's registry entry and a run into a process to start: the
//! command, its arguments, working folder, environment and the MCP config.
//! Blocking file work; call it from a blocking thread. The platform-specific
//! parts (finding the executable, the console window, killing a tree) live in
//! `platform`.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::json;
use tokio::process::Command;

use super::platform;

/// Everything one start needs.
pub struct Spec<'a> {
    pub run_id: i64,
    /// `KEY-n`.
    pub issue: &'a str,
    pub project: &'a str,
    /// `assigned`, `mentioned` or `manual`.
    pub reason: &'a str,
    pub message: &'a str,
    pub command: &'a str,
    pub args: &'a [String],
    pub work_dir: Option<&'a str>,
    /// The run's own git worktree and its branch; they replace the working
    /// folder (an explicit `work_dir` never has one).
    pub worktree: Option<(&'a Path, &'a str)>,
    pub data_dir: &'a Path,
    pub base_url: &'a str,
    /// The per-run key's secret.
    pub key: &'a str,
}

pub struct Prepared {
    pub command: Command,
    /// Written to the process's standard input, then it is closed. `None` when
    /// the prompt travels in a `{prompt}` argument.
    pub stdin: Option<String>,
}

/// The folder of one run under the data folder.
pub fn run_dir(data_dir: &Path, run_id: i64) -> PathBuf {
    data_dir.join("runs").join(run_id.to_string())
}

fn substitute(arg: &str, spec: &Spec<'_>, mcp: Option<&Path>) -> String {
    let mut out = arg
        .replace("{prompt}", spec.message)
        .replace("{issue}", spec.issue)
        .replace("{run_id}", &spec.run_id.to_string());
    if let Some(path) = mcp {
        out = out.replace("{mcp_config}", &path.display().to_string());
    }
    out
}

/// Written to a temporary name, flushed, then renamed into place.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

fn is_claude(command: &str) -> bool {
    Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("claude"))
}

/// Builds the command. `Err` is a short reason the run fails with; nothing has
/// been started then.
pub fn prepare(spec: &Spec<'_>) -> Result<Prepared, String> {
    let dir = run_dir(spec.data_dir, spec.run_id);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    let cwd = match (spec.worktree, spec.work_dir) {
        (Some((tree, _)), _) => {
            if !tree.is_dir() {
                return Err(format!(
                    "the working copy {} does not exist",
                    tree.display()
                ));
            }
            tree.to_path_buf()
        }
        (None, Some(work)) => {
            let path = PathBuf::from(work);
            if !path.is_dir() {
                return Err(format!("the agent's work_dir {work} does not exist"));
            }
            path
        }
        (None, None) => {
            // Not the run folder itself: the MCP config with the key is there.
            let path = dir.join("work");
            std::fs::create_dir_all(&path)
                .map_err(|err| format!("could not create {}: {err}", path.display()))?;
            path
        }
    };

    let resolved = platform::resolve_command(
        spec.command,
        std::env::var_os("PATH"),
        std::env::var("PATHEXT").ok(),
    )?;
    let uses_prompt = spec.args.iter().any(|a| a.contains("{prompt}"));
    let uses_mcp = spec.args.iter().any(|a| a.contains("{mcp_config}"));
    if uses_prompt && resolved.shim {
        return Err(format!(
            "{} is a .cmd/.bat file and the {{prompt}} argument cannot be passed to it safely \
             (cmd.exe quoting cannot carry arbitrary text); send the prompt on standard input \
             instead, or point the agent at the real executable",
            spec.command
        ));
    }
    let has_mcp_flag = spec.args.iter().any(|a| a.contains("--mcp-config"));
    let add_mcp_flag = is_claude(spec.command) && !has_mcp_flag && !uses_mcp;
    let mcp_path = if uses_mcp || add_mcp_flag {
        let path = dir.join("mcp.json");
        let config = json!({ "mcpServers": { "aurora": {
            "type": "http",
            "url": format!("{}/mcp", spec.base_url),
            "headers": { "Authorization": format!("Bearer {}", spec.key) },
        } } });
        write_private(&path, &config.to_string())
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Some(path)
    } else {
        None
    };

    let mut args: Vec<String> = spec
        .args
        .iter()
        .map(|a| substitute(a, spec, mcp_path.as_deref()))
        .collect();
    if let (true, Some(path)) = (add_mcp_flag, &mcp_path) {
        args.push("--mcp-config".to_string());
        args.push(path.display().to_string());
    }

    let mut command = Command::new(&resolved.path);
    command.args(&args).current_dir(&cwd);
    command.env_clear();
    for (name, value) in std::env::vars_os() {
        let upper: OsString = name.to_ascii_uppercase();
        if !upper.to_string_lossy().starts_with("AURORA_") {
            command.env(name, value);
        }
    }
    command
        .env("AURORA_URL", spec.base_url)
        .env("AURORA_API_KEY", spec.key)
        .env("AURORA_ISSUE", spec.issue)
        .env("AURORA_PROJECT", spec.project)
        .env("AURORA_RUN_ID", spec.run_id.to_string())
        .env("AURORA_TRIGGER", spec.reason)
        .env("AURORA_WORKDIR", &cwd)
        .env("AURORA_MCP_URL", format!("{}/mcp", spec.base_url))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some((_, branch)) = spec.worktree {
        command.env("AURORA_BRANCH", branch);
    }
    platform::prepare(&mut command);
    Ok(Prepared {
        command,
        stdin: (!uses_prompt).then(|| spec.message.to_string()),
    })
}
