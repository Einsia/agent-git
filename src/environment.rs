//! EnvironmentState capture.
//!
//! EnvironmentState = repo identity + HEAD commit + stash
//! where the stash **must cover staged / unstaged / untracked** (an explicit PRD requirement) --
//! because the Agent's judgment is based on the working tree as it was at that moment, and conclusions detached from that baseline can't be trusted.
//!
//! The implementation works entirely through plumbing + a temporary GIT_INDEX_FILE, and never touches the user's working directory or staging area.

use crate::scope;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentRevision {
    /// Repository identity: prefer the origin remote URL, fall back to the first commit's hash (so it still aligns when there is no remote).
    pub repo_identity: String,
    /// The commit that HEAD points to.
    pub head_commit: String,
    /// Working-tree snapshot tree object covering staged+unstaged+untracked. Identical to HEAD^{tree} when the working tree is clean.
    pub stash_tree: String,
    /// Whether the working tree has uncommitted changes (stash_tree ≠ HEAD's tree).
    pub dirty: bool,
}

fn git_out(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("failed to run git")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn repo_identity(root: &Path) -> String {
    if let Ok(url) = git_out(root, &["config", "--get", "remote.origin.url"]) {
        if !url.is_empty() {
            return url;
        }
    }
    // No remote: use the first commit's hash as a stable identity
    git_out(root, &["rev-list", "--max-parents=0", "HEAD"])
        .map(|s| {
            s.lines()
                .next()
                .map(|x| format!("root:{x}"))
                .unwrap_or_else(|| "unknown".into())
        })
        .unwrap_or_else(|_| "unknown".into())
}

/// Build a tree covering staged+unstaged+untracked without touching the user's index / worktree.
fn snapshot_tree(root: &Path) -> Result<String> {
    let tmp = tempfile::Builder::new()
        .prefix("agit-index-")
        .tempfile()
        .context("failed to create temporary index")?;
    let idx = tmp.path().to_string_lossy().to_string();

    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_INDEX_FILE", &idx)
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };

    // Start from HEAD (empty index if there are no commits), then add every tracked change + untracked file from the working tree.
    let _ = run(&["read-tree", "HEAD"]);
    // --all includes untracked files; subject to .gitignore (ignored secret files won't enter the snapshot).
    run(&["add", "--all", "."])?;
    run(&["write-tree"])
}

pub fn capture(root: &Path) -> Result<EnvironmentRevision> {
    let head_commit = git_out(root, &["rev-parse", "HEAD"]).unwrap_or_default();
    let stash_tree = snapshot_tree(root)?;
    let head_tree = git_out(root, &["rev-parse", "HEAD^{tree}"]).unwrap_or_default();
    Ok(EnvironmentRevision {
        repo_identity: repo_identity(root),
        head_commit,
        stash_tree: stash_tree.clone(),
        dirty: !head_tree.is_empty() && stash_tree != head_tree,
    })
}

/// Capture the current Environment (defaults to the repository at cwd).
pub fn capture_current() -> Result<EnvironmentRevision> {
    capture(&scope::env_root()?)
}
