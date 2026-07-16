//! Scope and dual-repo discovery.
//!
//! The core of the PRD: the versioned objects are two git repos + a pairing.
//!   Environment = the user's existing code repository (left untouched)
//!   Agent Store = .agit/agent/ -- a standalone git repository holding AgentState
//!
//!   agit <git-args>     = agit -e <git-args>  → git operates on the Environment
//!   agit -a <git-args>                        → git operates on the Agent Store (isomorphic operation)

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Default: operates on the code repository. Must be a transparent git wrapper.
    Environment,
    /// -a: operates on the Agent Store.
    Agent,
}

/// The Agent Store's location relative to the code repository root. Written into the code repository's .gitignore.
pub const AGENT_DIR: &str = ".agit/agent";
/// Where the WorkspaceRevision log lives. Deliberately placed outside both git worktrees, to avoid moving the agent ref when writing a pairing.
pub const WORKSPACE_DIR: &str = ".agit/workspace";

/// The working-tree root of the Environment (the code repository).
pub fn env_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to execute git")?;
    if !out.status.success() {
        bail!("The current directory is not inside a git repository. agit's Environment is your code repository.");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim(),
    ))
}

/// The working-tree root of the Agent Store (may not exist yet).
pub fn agent_root() -> Result<PathBuf> {
    Ok(env_root()?.join(AGENT_DIR))
}

pub fn workspace_dir() -> Result<PathBuf> {
    Ok(env_root()?.join(WORKSPACE_DIR))
}

/// The working-tree root of the repository for a given scope.
pub fn root_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::Environment => env_root(),
        Scope::Agent => {
            let a = agent_root()?;
            if !a.join(".git").exists() {
                bail!(
                    "The Agent Store does not exist yet ({}). Run `agit init` first.",
                    a.display()
                );
            }
            Ok(a)
        }
    }
}

/// Whether the cwd is in the code repository or the Agent Store, always returns the root of the **Environment (the code repository)**.
///
/// A fact's evidence `file:` pointer is resolved relative to the Environment, but the merge driver runs inside the Agent Store,
/// and verify may be invoked from either place -- all of them need a stable way to obtain the code repository root.
/// The Agent Store is always at `<env>/.agit/agent`, so we back up two levels accordingly.
pub fn environment_root() -> Result<PathBuf> {
    let top = env_root()?;
    if top.ends_with(AGENT_DIR) {
        Ok(top
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or(top))
    } else {
        Ok(top)
    }
}

pub fn agent_exists() -> bool {
    agent_root()
        .map(|p| p.join(".git").exists())
        .unwrap_or(false)
}

/// Run a single git command in the given repository's working tree, require success, and return stdout.
pub fn git_in(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("failed to execute git")?;
    if !out.status.success() {
        bail!(
            "git -C {} {} failed:\n{}",
            root.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Failure-allowed variant: returns (exit code, stdout).
pub fn git_in_status(root: &Path, args: &[&str]) -> (i32, String) {
    match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        ),
        Err(_) => (-1, String::new()),
    }
}

/// Inherited-stdio variant: git's progress, interactive credential prompts, and real stderr all go straight to the terminal.
/// Remote operations (clone/fetch) must use this -- capturing stdout would swallow git's error messages and also block credential input.
pub fn git_in_inherit(root: &Path, args: &[&str]) -> i32 {
    use std::process::Stdio;
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .unwrap_or(-1)
}
