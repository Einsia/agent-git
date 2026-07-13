//! 原始 session dump 管理（新模型:不蒸馏 fact,直接版本化 agent 的完整会话)。
//!
//! Claude Code 自己把整个会话 dump 到 ~/.claude/projects/<slug>/:
//!   <uuid>.jsonl              完整转录
//!   <uuid>/subagents/*.jsonl  子 agent 转录
//!   <uuid>/tool-results/*.txt 大工具结果
//!   memory/                   记忆
//! `agit -a sync` 把这坨镜像进 Agent Store 的 sessions/<runtime>/,之后 commit/push/pull 照旧。

use crate::adapter::claude_code;
use crate::scope::{self, Scope};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const SESSIONS_SUBDIR: &str = "sessions";

/// 定位当前项目的 runtime session dump 目录。
fn source_dir(runtime: &str, cwd: &Path) -> Result<PathBuf> {
    match runtime {
        "claude-code" | "claude" | "cc" => {
            let dir = claude_code::projects_dir()?.join(claude_code::slug_for(cwd));
            if !dir.exists() {
                bail!(
                    "找不到本项目的 Claude Code session 目录:{}\n\
                     (这个项目还没在 Claude Code 里跑过?)",
                    dir.display()
                );
            }
            Ok(dir)
        }
        other => bail!("runtime `{other}` 的 session dump 还没接(见 src/session.rs)"),
    }
}

/// `agit -a sync [--from <runtime>]` —— 把 runtime 的 session dump 镜像进 Agent Store。
pub fn sync(runtime: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let src = source_dir(runtime, &env)?;
    let dst = agent.join(SESSIONS_SUBDIR).join(&rt);

    std::fs::create_dir_all(&dst)?;
    let stats = mirror(&src, &dst)?;

    // 落盘前扫一遍密钥 —— dump 全部 session = agent cat 过的一切都在里面
    let hits = crate::scan::scan_tree(&dst)?;

    println!("已镜像 {} 的 session dump:", rt);
    println!("  来源  : {}", src.display());
    println!("  写入  : {}", dst.display());
    println!("  文件  : {} 个({} 更新 / {} 新增),{} 字节", stats.total, stats.updated, stats.added, stats.bytes);
    if hits > 0 {
        eprintln!("  ⚠ 扫到 {hits} 处疑似密钥 —— session 转录里带着 agent 见过的敏感内容。");
        eprintln!("     push 前会再拦一次;先 `agit -a scan` 看看,或从转录里清掉。");
    }
    println!("\n  提交: agit -a add -A && agit -a commit -m 'sync {rt} sessions'");
    Ok(0)
}

fn normalize(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
}

struct Stats {
    total: usize,
    added: usize,
    updated: usize,
    bytes: u64,
}

/// 递归镜像 src → dst(只按大小+mtime 判断是否需要覆盖,够用)。
fn mirror(src: &Path, dst: &Path) -> Result<Stats> {
    let mut st = Stats { total: 0, added: 0, updated: 0, bytes: 0 };
    mirror_into(src, dst, &mut st)?;
    Ok(st)
}

fn mirror_into(src: &Path, dst: &Path, st: &mut Stats) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("读 {}", src.display()))? {
        let entry = entry?;
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            mirror_into(&sp, &dp, st)?;
        } else {
            let smeta = entry.metadata()?;
            match std::fs::metadata(&dp) {
                Err(_) => {
                    std::fs::copy(&sp, &dp)?;
                    st.added += 1;
                }
                Ok(dmeta) if dmeta.len() != smeta.len() => {
                    std::fs::copy(&sp, &dp)?;
                    st.updated += 1;
                }
                Ok(_) => {} // 未变,跳过
            }
            st.total += 1;
            st.bytes += smeta.len();
        }
    }
    Ok(())
}
