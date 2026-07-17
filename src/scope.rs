//! Scope and dual-repo discovery.
//!
//! The core of the PRD: the versioned objects are two git repos + a pairing.
//!   Environment = the user's existing code repository (left untouched)
//!   Agent Store = a standalone git repository holding AgentState, found by IDENTITY (`crate::agent`)
//!
//!   agit <git-args>        = agit -e <git-args>  → git operates on the Environment
//!   agit agent <git-args>  (alias: agit a)       → git operates on the Agent Store (isomorphic operation)
//!
//! The store is **not** a location this module decides. An agent is a memory that outlives any one
//! repo, so it lives at `$AGIT_HOME/agents/<aid>/` and is reached by resolving *which agent* — never
//! by walking to a path relative to the code repo.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Default: operates on the code repository. Must be a transparent git wrapper.
    Environment,
    /// `agit agent` / `agit a` (or the deprecated `-a`): operates on the Agent Store.
    Agent,
}

/// Where a store lived **before** agent identity, relative to the code repo root.
///
/// This is no longer a place agit resolves a store — it is only the place agit *recognizes* one, so
/// that a repo predating identity gets an actionable "run `agit a import`" from every agent-scoped
/// command instead of a fresh empty store. See `crate::agent::legacy_store`.
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

/// The working-tree root of the resolved agent's store: `$AGIT_HOME/agents/<aid>/`.
///
/// There is exactly ONE answer, and identity is what gives it (`crate::agent::resolve`: `--agent` →
/// `$AGIT_AGENT` → the active pointer → `.agit.toml [defaults]` → an actionable error).
///
/// The three rungs this replaced — `$AGIT_AGENT_DIR`, the `.agit/store` pointer file, and the nested
/// `<env>/.agit/agent` — all answered "where", and none of them answered "whose". A store found by
/// location is welded to one code repo, which is what made PRD #3 (an agent carrying its memory into
/// another repo) impossible. The pointer file failed the rule that keeps a local file honest — *its
/// absence must be fully recoverable from committed state* — because deleting it left nothing on earth
/// able to say where the store had gone.
pub fn agent_root() -> Result<PathBuf> {
    Ok(crate::agent::resolve(None)?.store)
}

pub fn workspace_dir() -> Result<PathBuf> {
    Ok(env_root()?.join(WORKSPACE_DIR))
}

/// agit's own home, for state that spans repos: `$AGIT_HOME` when set and non-empty, else `$HOME/.agit`.
///
/// The ONLY place that reads `$AGIT_HOME`. Tests point it (and `$HOME`) at a temp dir per invocation, so
/// a test run can never reach the developer's real stores.
pub fn agit_home() -> Result<PathBuf> {
    agit_home_from(
        std::env::var("AGIT_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The resolution itself, taking the environment as arguments so it is testable without `set_var`
/// (which is process-global and races with parallel tests).
fn agit_home_from(agit_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = agit_home.map(str::trim).filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = home
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .context("could not read $HOME, and $AGIT_HOME is not set")?;
    Ok(PathBuf::from(home).join(".agit"))
}

/// The working-tree root of the repository for a given scope.
///
/// `Scope::Agent` still means "run git on the agent's store" — `agit a log`, `agit a commit` — it just
/// reaches it by identity now.
pub fn root_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::Environment => env_root(),
        Scope::Agent => {
            let a = agent_root()?;
            if !a.join(".git").exists() {
                bail!(
                    "{} carries an agent's identity but is not a git repository.\n\
                     \x20      The store is damaged; if it has a remote, re-clone it with `agit a track <url>`.",
                    a.display()
                );
            }
            Ok(a)
        }
    }
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

#[cfg(test)]
mod agit_home_tests {
    use super::agit_home_from;
    use std::path::PathBuf;

    #[test]
    fn agit_home_prefers_the_var_then_falls_back_to_home() {
        assert_eq!(
            agit_home_from(Some("/x/store"), Some("/home/dev")).unwrap(),
            PathBuf::from("/x/store"),
            "$AGIT_HOME wins when set"
        );
        assert_eq!(
            agit_home_from(None, Some("/home/dev")).unwrap(),
            PathBuf::from("/home/dev/.agit"),
            "unset → $HOME/.agit"
        );
        // An empty/blank var must not resolve to the *relative* `.agit`, which would silently plant a
        // store in whatever cwd agit happened to run from.
        for blank in ["", "   "] {
            assert_eq!(
                agit_home_from(Some(blank), Some("/home/dev")).unwrap(),
                PathBuf::from("/home/dev/.agit"),
                "blank $AGIT_HOME is treated as unset"
            );
        }
        assert!(
            agit_home_from(None, None).is_err(),
            "no $AGIT_HOME and no $HOME must fail loudly, not yield a relative path"
        );
    }
}
