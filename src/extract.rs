//! SessionIR → AgentState（写进 Agent Store 的 state/）。
//!
//! 只写确定性的部分：目标、artifact、证据候选池（对齐当前代码基线）。
//! 证据池里 file: 证据当场重算摘要（安全，只读文件）；cmd: 证据**只记不跑**
//! —— session 里的命令可能有副作用，import 不该执行它们（与 verify 默认不跑 cmd 一致）。
//!
//! 从「证据池」到「结论（fact）」那一步需要模型归纳，留给 Summarizer（暂缺），不在这里做。

use crate::adapter::SessionIR;
use crate::claim::Evidence;
use crate::evidence;
use anyhow::{Context, Result};
use std::path::Path;

pub struct Summary {
    pub prompts: usize,
    pub reads_captured: usize,
    pub reads_skipped: usize,
    pub commands: usize,
    pub artifacts: usize,
}

/// 把绝对路径转成相对 env_root 的路径；不在 env_root 下则返回 None。
fn relativize(abs: &str, env_root: &Path) -> Option<String> {
    let p = Path::new(abs);
    p.strip_prefix(env_root)
        .ok()
        .map(|r| r.to_string_lossy().into_owned())
        .or_else(|| {
            // 已经是相对路径就原样接受
            if p.is_relative() {
                Some(abs.to_string())
            } else {
                None
            }
        })
}

pub fn write_state(ir: &SessionIR, state_dir: &Path, env_root: &Path) -> Result<Summary> {
    std::fs::create_dir_all(state_dir.join("facts"))?;

    // ── 目标：来自用户 prompt ──
    let mut goals = String::from("# 目标\n\n");
    if ir.prompts.is_empty() {
        goals.push_str("_（session 里没有可提取的用户 prompt）_\n");
    } else {
        for p in &ir.prompts {
            let one = p.lines().next().unwrap_or("").trim();
            goals.push_str(&format!("- {one}\n"));
        }
    }
    std::fs::write(state_dir.join("goals.md"), goals)?;

    // ── 进度：最后一段 agent 文本（草稿，待 Summarizer 收敛）──
    let progress = ir
        .agent_texts
        .iter()
        .rev()
        .find(|t| t.trim().len() > 20)
        .map(|t| {
            let t = t.trim();
            let head: String = t.chars().take(600).collect();
            format!("# 进度\n\n_（抽取自 session 的最后一段 agent 文本，待归纳）_\n\n{head}\n")
        })
        .unwrap_or_else(|| "# 进度\n".to_string());
    std::fs::write(state_dir.join("progress.md"), progress)?;

    // ── Artifact：来自 Write/Edit ──
    let mut art = String::from("# Artifact 引用\n\n");
    let mut artifacts = 0;
    for w in &ir.writes {
        if let Some(rel) = relativize(w, env_root) {
            art.push_str(&format!("- `{rel}`\n"));
            artifacts += 1;
        }
    }
    std::fs::write(state_dir.join("artifacts.md"), art)?;

    // ── 证据候选池：file: 当场对齐基线，cmd: 只记不跑 ──
    let mut pool = String::from(
        "# 证据候选池\n\n\
         > 由 adapter 从 session 的 Read/Bash 调用确定性提取，对齐当前代码基线。\n\
         > 这是「结论（fact）」的原材料，不是结论本身。用 `agit -a new` 把它们提炼成带证据的 fact。\n\n\
         ## 读过的文件（file: 证据，已对齐基线）\n\n",
    );
    let (mut captured, mut skipped) = (0usize, 0usize);
    let mut seen = std::collections::HashSet::new();

    for r in &ir.reads {
        let Some(rel) = relativize(&r.path, env_root) else {
            skipped += 1;
            continue;
        };
        // 行范围：Read 的 offset/limit → 行号区间
        let (start, end) = match (r.offset, r.limit) {
            (Some(o), Some(l)) => (o.max(1), o.max(1) + l.saturating_sub(1)),
            (Some(o), None) => (o.max(1), o.max(1)),
            _ => (1, 1),
        };
        let locator = format!("file:{rel}:{start}-{end}");
        if !seen.insert(locator.clone()) {
            continue;
        }
        match locator.parse::<Evidence>().and_then(|e| evidence::capture(env_root, e)) {
            Ok(ev) => {
                pool.push_str(&format!("- `{ev}`\n"));
                captured += 1;
            }
            Err(_) => {
                // 文件没了 / 在 denylist 上 / 越界 —— 记 locator 但标未对齐
                pool.push_str(&format!("- `{locator}`  _(未对齐：源已变或被拦)_\n"));
                skipped += 1;
            }
        }
    }

    pool.push_str("\n## 跑过的命令（cmd: 证据，只记不跑）\n\n");
    for c in &ir.commands {
        let one = c.lines().next().unwrap_or("").trim();
        if one.is_empty() {
            continue;
        }
        pool.push_str(&format!("- `cmd:{one}`\n"));
    }
    std::fs::write(state_dir.join("_evidence_pool.md"), pool)?;

    // ── 溯源：这份 AgentState 从哪个 session、对齐到哪个基线 ──
    let env_head = crate::environment::capture(env_root).ok();
    let session_json = serde_json::json!({
        "runtime": ir.runtime,
        "session_id": ir.session_id,
        "cwd": ir.cwd,
        "git_branch": ir.git_branch,
        "environment_baseline": env_head,
    });
    std::fs::write(
        state_dir.join("_session.json"),
        serde_json::to_string_pretty(&session_json)?,
    )
    .context("写 _session.json 失败")?;

    Ok(Summary {
        prompts: ir.prompts.len(),
        reads_captured: captured,
        reads_skipped: skipped,
        commands: ir.commands.len(),
        artifacts,
    })
}
