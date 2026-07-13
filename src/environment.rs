//! EnvironmentState 捕获。
//!
//! EnvironmentState = repo identity + HEAD commit + stash
//! 其中 stash **必须覆盖 staged / unstaged / untracked**（PRD 明确要求）——
//! 因为 Agent 的判断是基于当时那个工作树的，脱离基线的结论不可信。
//!
//! 实现全程走 plumbing + 临时 GIT_INDEX_FILE，绝不动用户的工作区和暂存区。

use crate::scope;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentRevision {
    /// 仓库身份：优先 origin remote URL，退回首个提交的 hash（无 remote 时也能对齐）。
    pub repo_identity: String,
    /// HEAD 指向的提交。
    pub head_commit: String,
    /// 覆盖 staged+unstaged+untracked 的工作树快照 tree 对象。工作树干净时与 HEAD^{tree} 相同。
    pub stash_tree: String,
    /// 工作树是否有未提交改动（stash_tree ≠ HEAD 的 tree）。
    pub dirty: bool,
}

fn git_out(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("无法执行 git")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn repo_identity(root: &Path) -> String {
    if let Ok(url) = git_out(root, &["config", "--get", "remote.origin.url"]) {
        if !url.is_empty() {
            return url;
        }
    }
    // 无 remote：用首个提交的 hash 作为稳定身份
    git_out(root, &["rev-list", "--max-parents=0", "HEAD"])
        .map(|s| {
            s.lines()
                .next()
                .map(|x| format!("root:{x}"))
                .unwrap_or_else(|| "unknown".into())
        })
        .unwrap_or_else(|_| "unknown".into())
}

/// 构造一个覆盖 staged+unstaged+untracked 的 tree，不碰用户的 index / worktree。
fn snapshot_tree(root: &Path) -> Result<String> {
    let tmp = tempfile::Builder::new()
        .prefix("agit-index-")
        .tempfile()
        .context("无法创建临时 index")?;
    let idx = tmp.path().to_string_lossy().to_string();

    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_INDEX_FILE", &idx)
            .output()
            .context("无法执行 git")?;
        if !out.status.success() {
            anyhow::bail!(
                "git {} 失败: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };

    // 从 HEAD 起（无提交则空 index），再把工作树里所有跟踪修改 + untracked 加进来。
    let _ = run(&["read-tree", "HEAD"]);
    // --all 含未跟踪；受 .gitignore 约束（被忽略的密钥文件不会进快照）。
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

/// 捕获当前 Environment（默认从 cwd 的代码仓库）。
pub fn capture_current() -> Result<EnvironmentRevision> {
    capture(&scope::env_root()?)
}
