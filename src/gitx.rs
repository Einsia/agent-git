//! A thin wrapper around git.
//!
//! We deliberately shell out to canonical git rather than linking libgit2 / gitoxide:
//! all of our value lives in the merge driver, and those binding libraries are
//! **reimplementations** of git that don't guarantee running the external
//! `merge.<name>.driver` command from `.git/config`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run a git command, require success, and return stdout.
pub fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("failed to run git; is it on your PATH?")?;
    if !out.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run a git command, allowing failure, and return (exit code, stdout).
pub fn git_status(args: &[&str]) -> Result<(i32, String)> {
    let out = Command::new("git").args(args).output()?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
    ))
}

/// Pass git's output straight through to the terminal (for commands like push / pull / log where you want to see live output).
pub fn git_passthrough(args: &[String]) -> Result<i32> {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git")?;
    Ok(status.code().unwrap_or(-1))
}

pub fn repo_root() -> Result<PathBuf> {
    let s = git(&["rev-parse", "--show-toplevel"]).context("the current directory is not inside a git repository")?;
    Ok(PathBuf::from(s))
}

pub fn git_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])?))
}

/// Is this path ignored by .gitignore? We don't capture evidence snapshot content for ignored paths.
pub fn is_ignored(path: &Path) -> bool {
    matches!(
        git_status(&["check-ignore", "-q", &path.to_string_lossy()]),
        Ok((0, _))
    )
}

pub fn config_set(key: &str, value: &str) -> Result<()> {
    git(&["config", key, value]).map(|_| ())
}

pub fn config_get(key: &str) -> Option<String> {
    match git_status(&["config", "--get", key]) {
        Ok((0, v)) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Paths currently in a conflicted (unmerged) state.
pub fn conflicted_paths() -> Result<Vec<String>> {
    let out = git(&["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(|s| s.to_string()).collect())
}

/// Staged paths. The pre-commit hook uses this to decide the scan scope.
pub fn staged_paths() -> Result<Vec<String>> {
    let out = git(&["diff", "--cached", "--name-only", "--diff-filter=ACM"])?;
    Ok(out.lines().map(|s| s.to_string()).collect())
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("failed to locate agit's own executable path")
}
