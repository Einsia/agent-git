//! Scope 与双库发现。
//!
//! PRD 的核心：被版本化的对象是两个 git 库 + 一个配对。
//!   Environment = 用户现有的代码仓库（原封不动）
//!   Agent Store = .agit/agent/ —— 独立 git 仓库，装 AgentState
//!
//!   agit <git-args>     = agit -e <git-args>  → git 作用在 Environment
//!   agit -a <git-args>                        → git 作用在 Agent Store（同构操作）

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 默认：作用在代码仓库上。必须是透明 git wrapper。
    Environment,
    /// -a：作用在 Agent Store 上。
    Agent,
}

/// Agent Store 相对代码仓库根的位置。写进代码仓库的 .gitignore。
pub const AGENT_DIR: &str = ".agit/agent";
/// WorkspaceRevision 日志所在。刻意放在两个 git worktree 之外，避免写配对时移动 agent ref。
pub const WORKSPACE_DIR: &str = ".agit/workspace";

/// Environment（代码仓库）的工作树根。
pub fn env_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("无法执行 git")?;
    if !out.status.success() {
        bail!("当前目录不在一个 git 仓库里。agit 的 Environment 就是你的代码仓库。");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim(),
    ))
}

/// Agent Store 的工作树根（可能还不存在）。
pub fn agent_root() -> Result<PathBuf> {
    Ok(env_root()?.join(AGENT_DIR))
}

pub fn workspace_dir() -> Result<PathBuf> {
    Ok(env_root()?.join(WORKSPACE_DIR))
}

/// 某个 scope 对应库的工作树根。
pub fn root_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::Environment => env_root(),
        Scope::Agent => {
            let a = agent_root()?;
            if !a.join(".git").exists() {
                bail!(
                    "Agent Store 还不存在（{}）。先跑 `agit init`。",
                    a.display()
                );
            }
            Ok(a)
        }
    }
}

pub fn agent_exists() -> bool {
    agent_root()
        .map(|p| p.join(".git").exists())
        .unwrap_or(false)
}

/// 在给定的库工作树里跑一条 git，要求成功，返回 stdout。
pub fn git_in(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("无法执行 git")?;
    if !out.status.success() {
        bail!(
            "git -C {} {} 失败:\n{}",
            root.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// 允许失败版：返回 (退出码, stdout)。
pub fn git_in_status(root: &Path, args: &[&str]) -> (i32, String) {
    match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        ),
        Err(_) => (-1, String::new()),
    }
}
