//! git 的薄封装。
//!
//! 刻意 shell out 到 canonical git，而不是链接 libgit2 / gitoxide：
//! 我们的全部价值在 merge driver，而绑定库是 git 的**重新实现**，
//! 不保证执行 `.git/config` 里的 `merge.<name>.driver` 外部命令。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 跑一条 git 命令，要求成功，返回 stdout。
pub fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("无法执行 git，它在 PATH 里吗？")?;
    if !out.status.success() {
        bail!(
            "git {} 失败:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// 跑一条 git 命令，允许失败，返回 (退出码, stdout)。
pub fn git_status(args: &[&str]) -> Result<(i32, String)> {
    let out = Command::new("git").args(args).output()?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
    ))
}

/// 把 git 的输出直接透到终端（用于 push / pull / log 这些要看实时输出的命令）。
pub fn git_passthrough(args: &[String]) -> Result<i32> {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("无法执行 git")?;
    Ok(status.code().unwrap_or(-1))
}

pub fn repo_root() -> Result<PathBuf> {
    let s = git(&["rev-parse", "--show-toplevel"]).context("当前目录不在一个 git 仓库里")?;
    Ok(PathBuf::from(s))
}

pub fn git_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])?))
}

/// 这个路径是否被 .gitignore 忽略？被忽略的路径不采集证据快照内容。
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

/// 当前处于冲突（unmerged）状态的路径。
pub fn conflicted_paths() -> Result<Vec<String>> {
    let out = git(&["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(|s| s.to_string()).collect())
}

/// 已暂存的路径。pre-commit hook 用它来决定扫描范围。
pub fn staged_paths() -> Result<Vec<String>> {
    let out = git(&["diff", "--cached", "--name-only", "--diff-filter=ACM"])?;
    Ok(out.lines().map(|s| s.to_string()).collect())
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("无法定位 agit 自身的可执行文件路径")
}
