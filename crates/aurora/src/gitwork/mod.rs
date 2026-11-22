//! Git for the runner (TRK-16): checking a project's repository, making a
//! worktree per run and inspecting and removing it when the run ends.
//!
//! Everything goes through the `git` command line, started with an argument
//! vector (never a shell), so no value is ever interpreted by a shell.
//!
//! * **Every invocation has a time limit** ([`Git::timeout`], 60 s) and its
//!   standard output and error are read with a cap; an error message carries a
//!   bounded part of what git said on standard error.
//! * **Operations that change a repository are serialised per repository**:
//!   one async mutex per canonical repository path ([`lock_repo`]). Concurrent
//!   `git worktree add` calls on one repository can fail on its locks, so the
//!   runner makes one at a time per repository (different repositories run in
//!   parallel). Reads of a worktree's own status are not locked.
//! * **Nothing is deleted that has changes.** [`Git::remove_worktree`] uses
//!   `git worktree remove` *without* `--force`, which refuses a worktree with
//!   modified or untracked files; only after a fresh inspection finds it clean
//!   does it fall back to deleting a folder that lingers, and only a folder
//!   shaped like `<data folder>/worktrees/<run id>`.
//! * `AURORA_GIT` names the git executable (default: `git` on `PATH`).
//!
//! Platform differences (hiding the console window, `\\?\` prefixes, git's
//! forward slashes, long paths, case-insensitive lock keys) live in `platform`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::OwnedMutexGuard;

#[cfg_attr(windows, path = "platform/windows.rs")]
#[cfg_attr(not(windows), path = "platform/other.rs")]
mod platform;

pub use platform::{canonical, from_git_output, plain};

/// What every git call may take.
pub const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STDOUT: usize = 4 * 1024 * 1024;
const MAX_STDERR: usize = 16 * 1024;
const MAX_MESSAGE_CHARS: usize = 400;
const REMOVE_ATTEMPTS: u32 = 4;

/// The message of a missing git, shown as it is.
pub const GIT_MISSING: &str = "git was not found on PATH";

macro_rules! argv {
    ($($arg:expr),* $(,)?) => { vec![$(OsString::from($arg)),*] };
}

/// The branch a working copy starts from, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// The branch name, without `refs/heads/` or `origin/`.
    pub name: String,
    /// The full SHA it pointed at.
    pub commit: String,
}

/// What a run left in its working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// Uncommitted and untracked files (a rename counts once).
    pub changed_files: i64,
    /// Commits beyond the base commit.
    pub ahead: i64,
    pub has_changes: bool,
}

struct Output {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl Output {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// The git command line, with its program and time limit.
#[derive(Debug, Clone)]
pub struct Git {
    program: OsString,
    timeout: Duration,
}

type LockMap = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// Waits for the turn of `repo`; held while a change to the repository runs.
pub async fn lock_repo(repo: &Path) -> OwnedMutexGuard<()> {
    static LOCKS: OnceLock<LockMap> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let entry = {
        let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(platform::lock_key(repo)).or_default().clone()
    };
    entry.lock_owned().await
}

/// A run's folder under the data folder is `<root>/worktrees/<digits>`; the
/// only kind of folder this module ever deletes.
pub fn is_run_worktree(path: &Path) -> bool {
    let name_ok = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()));
    let parent_ok = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "worktrees");
    name_ok && parent_ok
}

/// The number of changed files in `git status --porcelain=v1 -z` output.
pub fn count_status(raw: &[u8]) -> i64 {
    let mut count = 0;
    let mut parts = raw.split(|byte| *byte == 0).filter(|part| !part.is_empty());
    while let Some(entry) = parts.next() {
        count += 1;
        let renamed = entry.len() >= 2
            && (matches!(entry[0], b'R' | b'C') || matches!(entry[1], b'R' | b'C'));
        if renamed {
            // The original path follows as an entry of its own.
            parts.next();
        }
    }
    count
}

/// Replaces the repository's path (in either slash spelling) in `text` with
/// "the repository": git's messages name it, and run errors are visible to
/// agents, who must not learn where the repository is.
pub fn scrub(text: &str, repo: &str) -> String {
    let mut out = text.to_string();
    for spelling in [repo.to_string(), repo.replace('\\', "/")] {
        if !spelling.is_empty() {
            out = out.replace(&spelling, "the repository");
        }
    }
    out
}

fn parse_count(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok().filter(|n| *n >= 0)
}

fn is_sha(text: &str) -> bool {
    text.len() >= 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn clip(text: &str) -> String {
    let text = text.trim();
    let mut cut: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    if text.chars().count() > MAX_MESSAGE_CHARS {
        cut.push_str("...");
    }
    cut
}

/// `worktree add` of `git -C x worktree add ...`: the words that say what ran.
fn describe(args: &[OsString]) -> String {
    let mut words = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        let text = arg.to_string_lossy();
        if text == "-C" || text == "-c" {
            skip = true;
        } else if !text.starts_with('-') && words.len() < 2 {
            words.push(text.into_owned());
        }
    }
    words.join(" ")
}

async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> Vec<u8> {
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let _ = (&mut reader).take(cap as u64).read_to_end(&mut kept).await;
    // Keep draining so a chatty git is never blocked on a full pipe.
    let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    kept
}

impl Git {
    /// `AURORA_GIT` or `git`, with the usual time limit.
    pub fn system() -> Git {
        Git::with_program(std::env::var_os("AURORA_GIT").unwrap_or_else(|| "git".into()))
    }

    pub fn with_program(program: impl Into<OsString>) -> Git {
        Git {
            program: program.into(),
            timeout: TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Git {
        self.timeout = timeout;
        self
    }

    async fn run(&self, args: Vec<OsString>) -> Result<Output, String> {
        let mut command = Command::new(&self.program);
        command.args(platform::config_args()).args(&args);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        platform::no_window(&mut command);
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                GIT_MISSING.to_string()
            } else {
                format!("could not start git: {err}")
            }
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let work = async {
            let (out, err, status) = tokio::join!(
                read_capped(stdout, MAX_STDOUT),
                read_capped(stderr, MAX_STDERR),
                child.wait()
            );
            (out, err, status)
        };
        match tokio::time::timeout(self.timeout, work).await {
            Ok((out, err, status)) => {
                let status = status.map_err(|err| format!("could not wait for git: {err}"))?;
                Ok(Output {
                    success: status.success(),
                    stdout: out,
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                })
            }
            Err(_) => {
                let _ = child.kill().await;
                Err(format!(
                    "git {} did not finish within {} s",
                    describe(&args),
                    self.timeout.as_secs()
                ))
            }
        }
    }

    /// Runs git and turns a non-zero exit into an error that says why.
    async fn ok(&self, args: Vec<OsString>) -> Result<Output, String> {
        let what = describe(&args);
        let output = self.run(args).await?;
        if output.success {
            return Ok(output);
        }
        let said = if output.stderr.trim().is_empty() {
            output.text()
        } else {
            output.stderr.clone()
        };
        Err(format!("git {what} failed: {}", clip(&said)))
    }

    // ---- checking a project's settings ----

    /// Checks that `path` is an absolute path to a local git repository with a
    /// working tree and at least one commit, and returns its top-level folder
    /// (canonical, in the platform's own spelling): what the project stores.
    pub async fn check_repository(&self, path: &str) -> Result<String, String> {
        let given = Path::new(path);
        if !given.is_absolute() {
            return Err("repo_path must be an absolute path".to_string());
        }
        match std::fs::metadata(given) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("{path} is not a folder")),
            Err(_) => return Err(format!("{path} does not exist")),
        }
        let inside = self
            .run(argv!["-C", given, "rev-parse", "--is-inside-work-tree"])
            .await?;
        if !inside.success {
            return Err(format!("{path} is not a git repository"));
        }
        if inside.text() != "true" {
            return Err(format!(
                "{path} is a bare repository or the inside of a .git folder; it needs a working tree"
            ));
        }
        let top = self
            .ok(argv!["-C", given, "rev-parse", "--show-toplevel"])
            .await?;
        let top = canonical(&from_git_output(&top.text()))
            .map_err(|err| format!("could not read the repository folder: {err}"))?;
        let head = self
            .run(argv![
                "-C",
                &top,
                "rev-parse",
                "--verify",
                "--quiet",
                "HEAD"
            ])
            .await?;
        if !head.success {
            return Err(format!(
                "the repository {path} has no commits yet; make a first commit"
            ));
        }
        top.to_str()
            .map(str::to_string)
            .ok_or_else(|| "the repository path is not valid UTF-8".to_string())
    }

    /// Checks that `name` is a valid branch name that exists in `repo`
    /// (locally or as `origin/<name>`).
    pub async fn check_branch(&self, repo: &Path, name: &str) -> Result<(), String> {
        if name.starts_with('-') || name.contains(['\0', '\n', '\r']) {
            return Err(format!("{name:?} is not a valid branch name"));
        }
        let valid = self
            .run(argv!["-C", repo, "check-ref-format", "--branch", name])
            .await?;
        // `--branch` also accepts forms like `@{-1}` and prints what they mean.
        if !valid.success || valid.text() != name {
            return Err(format!("{name:?} is not a valid branch name"));
        }
        match self.branch_commit(repo, name).await? {
            Some(_) => Ok(()),
            None => Err(format!(
                "the branch {name} does not exist in the repository"
            )),
        }
    }

    // ---- making a worktree ----

    async fn commit_of(&self, repo: &Path, refname: &str) -> Result<Option<String>, String> {
        let found = self
            .run(argv![
                "-C",
                repo,
                "rev-parse",
                "--verify",
                "--quiet",
                format!("{refname}^{{commit}}")
            ])
            .await?;
        let text = found.text();
        Ok((found.success && is_sha(&text)).then_some(text))
    }

    /// The commit of the local branch `name`, else of `origin/<name>`.
    async fn branch_commit(&self, repo: &Path, name: &str) -> Result<Option<String>, String> {
        if let Some(commit) = self.commit_of(repo, &format!("refs/heads/{name}")).await? {
            return Ok(Some(commit));
        }
        self.commit_of(repo, &format!("refs/remotes/origin/{name}"))
            .await
    }

    /// The base a run starts from: `configured` when given, else the
    /// repository's default (`origin/HEAD`, the branch checked out, `main`,
    /// `master`, in that order).
    pub async fn resolve_base(
        &self,
        repo: &Path,
        configured: Option<&str>,
    ) -> Result<Base, String> {
        if let Some(name) = configured {
            return match self.branch_commit(repo, name).await? {
                Some(commit) => Ok(Base {
                    name: name.to_string(),
                    commit,
                }),
                None => Err(format!("the base branch {name} does not exist")),
            };
        }
        let mut names = Vec::new();
        let remote = self
            .run(argv![
                "-C",
                repo,
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD"
            ])
            .await?;
        if remote.success
            && let Some(name) = remote.text().strip_prefix("origin/")
        {
            names.push(name.to_string());
        }
        let current = self
            .run(argv![
                "-C",
                repo,
                "symbolic-ref",
                "--quiet",
                "--short",
                "HEAD"
            ])
            .await?;
        if current.success && !current.text().is_empty() {
            names.push(current.text());
        }
        names.extend(["main".to_string(), "master".to_string()]);
        for name in names {
            if let Some(commit) = self.branch_commit(repo, &name).await? {
                return Ok(Base { name, commit });
            }
        }
        Err("the repository has no default branch to start from (no origin/HEAD, no checked-out branch, no main or master); set the project's base branch".to_string())
    }

    /// Makes the worktree `path` on the new branch `branch`, started from the
    /// resolved base. One at a time per repository. Refuses (touching nothing)
    /// a folder or branch that already exists; when git itself fails, what
    /// this call created is removed again, best effort.
    pub async fn create_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        configured: Option<&str>,
    ) -> Result<Base, String> {
        let _turn = lock_repo(repo).await;
        let base = self.resolve_base(repo, configured).await?;
        if path.exists() {
            return Err(format!("{} already exists", path.display()));
        }
        if self
            .commit_of(repo, &format!("refs/heads/{branch}"))
            .await?
            .is_some()
        {
            return Err(format!("the branch {branch} already exists"));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        }
        let added = self
            .ok(argv![
                "-C",
                repo,
                "worktree",
                "add",
                "-b",
                branch,
                path,
                &base.commit
            ])
            .await;
        if let Err(why) = added {
            self.discard_half_made(repo, path, branch).await;
            return Err(why);
        }
        Ok(base)
    }

    /// After a failed `worktree add`: what it left is ours (the folder and the
    /// branch did not exist before), and has no changes.
    async fn discard_half_made(&self, repo: &Path, path: &Path, branch: &str) {
        let _ = self
            .run(argv!["-C", repo, "worktree", "remove", "--force", path])
            .await;
        if path.exists() && is_run_worktree(path) {
            let _ = delete_folder(path).await;
        }
        let _ = self.run(argv!["-C", repo, "worktree", "prune"]).await;
        let _ = self.run(argv!["-C", repo, "branch", "-D", branch]).await;
    }

    // ---- looking at a worktree ----

    /// What the worktree at `path` holds beyond `base_commit`. An error when
    /// git could not say (the caller keeps the worktree then).
    pub async fn inspect(
        &self,
        path: &Path,
        base_commit: Option<&str>,
    ) -> Result<Inspection, String> {
        let Some(base) = base_commit else {
            return Err("the commit the working copy started from is not recorded".to_string());
        };
        let status = self
            .ok(argv![
                "-C",
                path,
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all"
            ])
            .await?;
        let changed_files = count_status(&status.stdout);
        let counted = self
            .ok(argv![
                "-C",
                path,
                "rev-list",
                "--count",
                format!("{base}..HEAD")
            ])
            .await?;
        let ahead = parse_count(&counted.text())
            .ok_or_else(|| "git rev-list printed no commit count".to_string())?;
        // HEAD moved to another history entirely (a reset, a checkout of an
        // unrelated branch) also leaves work behind.
        let head = self
            .ok(argv!["-C", path, "rev-parse", "HEAD"])
            .await?
            .text();
        let moved = head != base;
        Ok(Inspection {
            changed_files,
            ahead,
            has_changes: changed_files > 0 || ahead > 0 || moved,
        })
    }

    // ---- removing a worktree ----

    /// Removes the worktree at `path` and its branch, only when it has no
    /// changes: it is inspected here first (anything else is an error and
    /// nothing is touched). Then plain `git worktree remove`, which itself
    /// refuses unclean state; if that fails, a fresh inspection must still find
    /// it clean before a lingering folder is deleted. An error leaves whatever
    /// remains for a person.
    pub async fn remove_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        base_commit: Option<&str>,
    ) -> Result<(), String> {
        let _turn = lock_repo(repo).await;
        if path.exists() {
            match self.inspect(path, base_commit).await {
                Ok(found) if !found.has_changes => {}
                Ok(_) => return Err("the working copy has changes, so it is kept".to_string()),
                Err(why) => return Err(format!("the working copy could not be inspected: {why}")),
            }
        }
        let mut last = String::new();
        for attempt in 1..=REMOVE_ATTEMPTS {
            match self.ok(argv!["-C", repo, "worktree", "remove", path]).await {
                Ok(_) => {
                    last.clear();
                    break;
                }
                Err(why) => last = why,
            }
            if !path.exists() {
                last.clear();
                break;
            }
            // Files may stay locked for a moment after their process was killed.
            if attempt < REMOVE_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        if path.exists() {
            let still_clean = matches!(
                self.inspect(path, base_commit).await,
                Ok(found) if !found.has_changes
            );
            if !still_clean {
                return Err(format!(
                    "the working copy is not clean any more, so it is kept ({})",
                    if last.is_empty() {
                        "no git error"
                    } else {
                        &last
                    }
                ));
            }
            if !is_run_worktree(path) {
                return Err(format!(
                    "{} is not a run's folder; left alone",
                    path.display()
                ));
            }
            delete_folder(path)
                .await
                .map_err(|err| format!("could not delete {}: {err}", path.display()))?;
        }
        let _ = self.run(argv!["-C", repo, "worktree", "prune"]).await;
        let exists = self
            .commit_of(repo, &format!("refs/heads/{branch}"))
            .await?
            .is_some();
        if exists {
            self.ok(argv!["-C", repo, "branch", "-D", branch]).await?;
        }
        Ok(())
    }
}

/// Deletes a folder tree, retrying for a moment (Windows keeps files locked
/// briefly after their process ended). Read-only files, which git makes in
/// object stores, are made writable first.
async fn delete_folder(path: &Path) -> std::io::Result<()> {
    let path: PathBuf = path.to_path_buf();
    let mut last = None;
    for _ in 0..REMOVE_ATTEMPTS {
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&target))
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("the delete task failed")));
        match result {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => last = Some(err),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("could not delete the folder")))
}

#[cfg(test)]
mod tests;
