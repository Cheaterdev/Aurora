//! Tests of the git layer against real repositories made in temporary folders.
//! They need `git` on `PATH` and fail loudly without it.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use super::*;

/// Runs git synchronously for setup; panics with git's own words on failure.
pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("git is required for these tests and could not run: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if args.first() == Some(&"branch") && args.contains(&"--list") {
        // `git branch` marks branches checked out in a worktree with "+ ".
        return text
            .lines()
            .map(|line| line.trim_start_matches(['+', '*', ' ']))
            .collect::<Vec<_>>()
            .join("\n");
    }
    text
}

/// A repository on branch `main` with one commit and local identity.
pub(crate) fn make_repo(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    sh(dir, &["init", "-q", "-b", "main"]);
    sh(dir, &["config", "user.name", "Test"]);
    sh(dir, &["config", "user.email", "test@example.com"]);
    sh(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    sh(dir, &["add", "."]);
    sh(dir, &["commit", "-q", "-m", "first"]);
    canonical(dir).unwrap()
}

fn git() -> Git {
    Git::system()
}

#[test]
fn status_output_is_counted_per_file_and_a_rename_counts_once() {
    assert_eq!(count_status(b""), 0);
    assert_eq!(count_status(b" M a.txt\0"), 1);
    assert_eq!(count_status(b" M a.txt\0?? b.txt\0A  c.txt\0"), 3);
    assert_eq!(count_status(b"R  new.txt\0old.txt\0?? x\0"), 2);
    assert_eq!(count_status(b"RM new.txt\0old.txt\0"), 1);
    assert_eq!(count_status(b"C  copy.txt\0orig.txt\0 D gone\0"), 2);
    assert_eq!(count_status(b"?? with space.txt\0"), 1);
}

#[test]
fn counts_and_shas_are_parsed_strictly() {
    assert_eq!(parse_count("3\n"), Some(3));
    assert_eq!(parse_count("0"), Some(0));
    assert_eq!(parse_count("-1"), None);
    assert_eq!(parse_count("x"), None);
    assert!(is_sha("0123456789abcdef0123456789abcdef01234567"));
    assert!(!is_sha("0123456789abcdef"));
    assert!(!is_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
}

#[test]
fn a_command_is_described_by_its_first_words() {
    let args = argv!["-C", "x", "worktree", "add", "-b", "branch", "path"];
    assert_eq!(describe(&args), "worktree add");
    let args = argv!["-C", "x", "status", "--porcelain"];
    assert_eq!(describe(&args), "status");
}

#[test]
fn only_folders_shaped_like_a_runs_worktree_may_be_deleted() {
    assert!(is_run_worktree(Path::new("/data/worktrees/12")));
    assert!(is_run_worktree(Path::new(r"C:\data\worktrees\7")));
    assert!(!is_run_worktree(Path::new("/data/worktrees/x")));
    assert!(!is_run_worktree(Path::new("/data/work/12")));
    assert!(!is_run_worktree(Path::new("/12")));
    assert!(!is_run_worktree(Path::new("/data/worktrees")));
}

#[test]
fn the_repository_path_is_scrubbed_from_messages() {
    assert_eq!(
        scrub(r"fatal: 'C:\r\x' and C:/r/x", r"C:\r\x"),
        "fatal: 'the repository' and the repository"
    );
    assert_eq!(scrub("nothing here", "/r"), "nothing here");
    assert_eq!(scrub("keep", ""), "keep");
}

#[test]
fn error_text_is_bounded() {
    let long = "x".repeat(5000);
    assert!(clip(&long).chars().count() <= MAX_MESSAGE_CHARS + 3);
    assert_eq!(clip("  short \n"), "short");
}

#[tokio::test]
async fn a_missing_git_is_reported_in_plain_words() {
    let missing = Git::with_program("aurora-no-such-git-program");
    let dir = tempfile::tempdir().unwrap();
    let why = missing
        .check_repository(dir.path().to_str().unwrap())
        .await
        .unwrap_err();
    assert_eq!(why, GIT_MISSING);
}

#[tokio::test]
async fn a_repository_is_checked_before_a_project_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let top = git()
        .check_repository(repo.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(Path::new(&top), repo);

    // A subfolder names its repository's top level.
    let sub = repo.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let top = git().check_repository(sub.to_str().unwrap()).await.unwrap();
    assert_eq!(Path::new(&top), repo);

    let plain_dir = dir.path().join("plain");
    std::fs::create_dir_all(&plain_dir).unwrap();
    let file = dir.path().join("file.txt");
    std::fs::write(&file, "x").unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    sh(&empty, &["init", "-q", "-b", "main"]);
    let bare = dir.path().join("bare.git");
    std::fs::create_dir_all(&bare).unwrap();
    sh(&bare, &["init", "-q", "--bare", "-b", "main"]);
    let missing = dir.path().join("nope");

    let cases = [
        ("relative/path".to_string(), "absolute"),
        (missing.display().to_string(), "does not exist"),
        (file.display().to_string(), "not a folder"),
        (plain_dir.display().to_string(), "not a git repository"),
        (bare.display().to_string(), "bare"),
        (empty.display().to_string(), "no commits"),
    ];
    for (path, expect) in cases {
        let why = git().check_repository(&path).await.unwrap_err();
        assert!(why.contains(expect), "{path}: {why}");
    }
}

#[tokio::test]
async fn a_base_branch_must_be_a_valid_name_of_a_branch_that_exists() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    sh(&repo, &["branch", "release/1.0"]);
    git().check_branch(&repo, "main").await.unwrap();
    git().check_branch(&repo, "release/1.0").await.unwrap();
    for bad in [
        "no space", "-x", "a..b", "@{-1}", "x.lock", "a~b", "", "ends/",
    ] {
        let why = git().check_branch(&repo, bad).await.unwrap_err();
        assert!(why.contains("not a valid branch name"), "{bad:?}: {why}");
    }
    let why = git().check_branch(&repo, "ghost").await.unwrap_err();
    assert!(why.contains("does not exist"), "{why}");
}

#[tokio::test]
async fn the_default_base_is_origin_head_then_the_checked_out_branch_then_main() {
    let dir = tempfile::tempdir().unwrap();
    let origin = make_repo(&dir.path().join("origin"));
    let clone = dir.path().join("clone");
    sh(
        dir.path(),
        &["clone", "-q", origin.to_str().unwrap(), "clone"],
    );
    sh(&clone, &["checkout", "-q", "-b", "feature"]);
    let clone = canonical(&clone).unwrap();
    // origin/HEAD says main although `feature` is checked out.
    let base = git().resolve_base(&clone, None).await.unwrap();
    assert_eq!(base.name, "main");
    assert_eq!(base.commit, sh(&clone, &["rev-parse", "main"]));

    // Without a remote the checked-out branch is the default.
    let solo = make_repo(&dir.path().join("solo"));
    sh(&solo, &["checkout", "-q", "-b", "trunk"]);
    let base = git().resolve_base(&solo, None).await.unwrap();
    assert_eq!(base.name, "trunk");

    // A detached HEAD falls back to main.
    sh(&solo, &["checkout", "-q", "main"]);
    sh(&solo, &["checkout", "-q", "--detach"]);
    let base = git().resolve_base(&solo, None).await.unwrap();
    assert_eq!(base.name, "main");

    // A configured branch wins, and a remote-only branch is found.
    sh(&origin, &["branch", "only-remote"]);
    sh(&clone, &["fetch", "-q"]);
    let base = git()
        .resolve_base(&clone, Some("only-remote"))
        .await
        .unwrap();
    assert_eq!(base.name, "only-remote");
    let why = git().resolve_base(&clone, Some("ghost")).await.unwrap_err();
    assert!(why.contains("does not exist"), "{why}");
}

#[tokio::test]
async fn a_worktree_is_made_inspected_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let data = canonical(dir.path()).unwrap();
    let path = data.join("worktrees").join("7");
    let base = git()
        .create_worktree(&repo, &path, "aurora/TRK-1-run7", None)
        .await
        .unwrap();
    assert_eq!(base.name, "main");
    assert!(path.join("README.md").is_file());
    assert_eq!(
        sh(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "aurora/TRK-1-run7"
    );
    assert_eq!(sh(&path, &["rev-parse", "HEAD"]), base.commit);

    let clean = git().inspect(&path, Some(&base.commit)).await.unwrap();
    assert_eq!(
        clean,
        Inspection {
            changed_files: 0,
            ahead: 0,
            has_changes: false
        }
    );
    git()
        .remove_worktree(&repo, &path, "aurora/TRK-1-run7", Some(&base.commit))
        .await
        .unwrap();
    assert!(!path.exists());
    assert_eq!(sh(&repo, &["branch", "--list", "aurora/*"]), "");
    assert!(
        !sh(&repo, &["worktree", "list", "--porcelain"]).contains("worktrees"),
        "no stale registration"
    );
}

#[tokio::test]
async fn untracked_modified_and_committed_work_counts_as_changes_and_blocks_removal() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let data = canonical(dir.path()).unwrap();
    let path = data.join("worktrees").join("8");
    let base = git()
        .create_worktree(&repo, &path, "aurora/TRK-1-run8", None)
        .await
        .unwrap();

    std::fs::write(path.join("new.txt"), "x").unwrap();
    std::fs::write(path.join("README.md"), "changed\n").unwrap();
    let found = git().inspect(&path, Some(&base.commit)).await.unwrap();
    assert_eq!(
        (found.changed_files, found.ahead, found.has_changes),
        (2, 0, true)
    );

    // Removal refuses and leaves everything in place.
    let why = git()
        .remove_worktree(&repo, &path, "aurora/TRK-1-run8", Some(&base.commit))
        .await
        .unwrap_err();
    assert!(why.contains("has changes"), "{why}");
    assert!(path.join("new.txt").is_file());
    assert_eq!(
        sh(&repo, &["branch", "--list", "aurora/*"]),
        "aurora/TRK-1-run8"
    );

    sh(&path, &["add", "."]);
    sh(&path, &["commit", "-q", "-m", "work"]);
    let found = git().inspect(&path, Some(&base.commit)).await.unwrap();
    assert_eq!(
        (found.changed_files, found.ahead, found.has_changes),
        (0, 1, true)
    );
    let why = git()
        .remove_worktree(&repo, &path, "aurora/TRK-1-run8", Some(&base.commit))
        .await
        .unwrap_err();
    assert!(why.contains("has changes"), "a commit is a change: {why}");
    assert_eq!(
        sh(&repo, &["branch", "--list", "aurora/*"]),
        "aurora/TRK-1-run8"
    );
    assert!(path.is_dir());
}

#[tokio::test]
async fn an_unknown_base_commit_cannot_be_inspected() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let why = git().inspect(&repo, None).await.unwrap_err();
    assert!(why.contains("not recorded"), "{why}");
}

#[tokio::test]
async fn existing_folders_and_branches_are_never_overwritten_or_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let data = canonical(dir.path()).unwrap();

    let taken = data.join("worktrees").join("9");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("mine.txt"), "keep me").unwrap();
    let why = git()
        .create_worktree(&repo, &taken, "aurora/TRK-1-run9", None)
        .await
        .unwrap_err();
    assert!(why.contains("already exists"), "{why}");
    assert!(taken.join("mine.txt").is_file());
    assert_eq!(sh(&repo, &["branch", "--list", "aurora/*"]), "");

    sh(&repo, &["branch", "aurora/TRK-1-run10"]);
    let free = data.join("worktrees").join("10");
    let why = git()
        .create_worktree(&repo, &free, "aurora/TRK-1-run10", None)
        .await
        .unwrap_err();
    assert!(
        why.contains("branch aurora/TRK-1-run10 already exists"),
        "{why}"
    );
    assert!(!free.exists());
    assert_eq!(
        sh(&repo, &["branch", "--list", "aurora/*"]),
        "aurora/TRK-1-run10"
    );
}

#[tokio::test]
async fn a_failed_start_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let data = canonical(dir.path()).unwrap();
    let path = data.join("worktrees").join("11");
    let why = git()
        .create_worktree(&repo, &path, "aurora/TRK-1-run11", Some("ghost"))
        .await
        .unwrap_err();
    assert!(why.contains("ghost"), "{why}");
    assert!(!path.exists());
    assert_eq!(sh(&repo, &["branch", "--list", "aurora/*"]), "");
}

#[tokio::test]
async fn simultaneous_worktrees_in_one_repository_all_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = make_repo(&dir.path().join("repo"));
    let data = canonical(dir.path()).unwrap();
    let mut tasks = Vec::new();
    for id in 20..28 {
        let repo = repo.clone();
        let path = data.join("worktrees").join(id.to_string());
        tasks.push(tokio::spawn(async move {
            git()
                .create_worktree(&repo, &path, &format!("aurora/TRK-{id}-run{id}"), None)
                .await
                .map(|_| path)
        }));
    }
    for task in tasks {
        let path = task.await.unwrap().expect("every worktree is made");
        assert!(path.join("README.md").is_file());
    }
    assert_eq!(
        sh(&repo, &["branch", "--list", "aurora/*"]).lines().count(),
        8
    );
}

#[tokio::test]
async fn the_lock_is_per_repository_and_ignores_the_spelling_of_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let first = lock_repo(&a).await;
    // A different repository does not wait.
    let other = tokio::time::timeout(Duration::from_secs(5), lock_repo(&b)).await;
    assert!(other.is_ok());
    // The same one does.
    let same = tokio::time::timeout(Duration::from_millis(200), lock_repo(&a)).await;
    assert!(same.is_err());
    drop(first);
    let again = tokio::time::timeout(Duration::from_secs(5), lock_repo(&a)).await;
    assert!(again.is_ok());
}
