//! WorkspaceRevision —— JointVersionControl。
//!
//! PRD：「agit commit 固定 EnvironmentRevision，agit -a commit 固定 AgentRevision。
//! 任一 ref 移动后，agit 自动生成 WorkspaceRevision，记录当前 Agent、当前 Environment 和连边。」
//!
//! 存为 .agit/workspace/ 下的 append-only 日志，**刻意放在两个 git worktree 之外** ——
//! 否则「写配对」这个动作本身会移动 agent ref，触发再写一条，无限递归。

use crate::environment::{self, EnvironmentRevision};
use crate::scope::{self, Scope};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRevision {
    /// 什么操作触发的（"env:commit" / "agent:merge" …）。
    pub trigger: String,
    /// 当前 AgentRevision（Agent Store 的 HEAD）。可能为空（还没有 agent 提交）。
    pub agent_rev: String,
    /// 当前 EnvironmentRevision。
    pub env: EnvironmentRevision,
    /// Agent↔Environment、Agent↔Agent 的连边。MVP 先留占位。
    pub relations: Vec<String>,
}

fn now_iso() -> String {
    // 不引入系统时间到测试路径：用 git 拿一个稳定可复现的时间戳成本更高，
    // 这里直接用 chrono。WorkspaceRevision 是运行期产物，不进 golden test。
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn agent_head() -> String {
    match scope::agent_root() {
        Ok(root) if root.join(".git").exists() => {
            scope::git_in_status(&root, &["rev-parse", "HEAD"]).1
        }
        _ => String::new(),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(9).collect()
}

/// 这次配对的连边：Agent↔Environment（当前配对本身）+ Agent↔Agent（若 agent HEAD 是 merge，
/// 记下并入的父提交）。从 git 拓扑现算 —— 不再是永远空的占位，`workspace` 能看到真实的图。
fn relations_for(agent_rev: &str, env: &EnvironmentRevision) -> Vec<String> {
    let mut rels = vec![format!(
        "agent~env:{}@{}",
        if agent_rev.is_empty() { "∅".into() } else { short(agent_rev) },
        short(&env.head_commit)
    )];
    if let Ok(root) = scope::agent_root() {
        if root.join(".git").exists() {
            // rev-list --parents -n1 HEAD → "HEAD p1 p2 …"；>2 段即 merge 提交。
            let line = scope::git_in_status(&root, &["rev-list", "--parents", "-n", "1", "HEAD"]).1;
            let toks: Vec<&str> = line.split_whitespace().collect();
            if toks.len() > 2 {
                let parents: Vec<String> = toks[1..].iter().map(|p| short(p)).collect();
                rels.push(format!("agent-merge:{}", parents.join("+")));
            }
        }
    }
    rels
}

/// 生成并追加一条 WorkspaceRevision。任一库 ref 移动后由 agit 自动调用。
pub fn record(trigger: &str) -> Result<WorkspaceRevision> {
    let env = environment::capture_current()?;
    let agent_rev = agent_head();
    let relations = relations_for(&agent_rev, &env);
    let rev = WorkspaceRevision {
        trigger: trigger.to_string(),
        agent_rev,
        env,
        relations,
    };

    let dir = scope::workspace_dir()?;
    std::fs::create_dir_all(&dir).context("无法创建 .agit/workspace")?;

    let mut line = serde_json::to_string(&serde_json::json!({
        "ts": now_iso(),
        "trigger": rev.trigger,
        "agent_rev": rev.agent_rev,
        "env": rev.env,
        "relations": rev.relations,
    }))?;
    line.push('\n');

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log.jsonl"))
        .context("无法写 workspace 日志")?;
    f.write_all(line.as_bytes())?;

    std::fs::write(
        dir.join("HEAD.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "ts": now_iso(),
            "trigger": rev.trigger,
            "agent_rev": rev.agent_rev,
            "env": rev.env,
            "relations": rev.relations,
        }))?,
    )?;

    Ok(rev)
}

/// 一个 git 子命令是否移动了 ref（据此决定要不要生成 WorkspaceRevision）。
pub fn moves_ref(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "commit" | "merge" | "reset" | "checkout" | "switch" | "cherry-pick" | "pull" | "rebase" | "revert" | "am"
    )
}

/// 供路由层调用：scope + 子命令 → trigger 字符串。
pub fn trigger_label(scope: Scope, subcommand: &str) -> String {
    let s = match scope {
        Scope::Environment => "env",
        Scope::Agent => "agent",
    };
    format!("{s}:{subcommand}")
}
