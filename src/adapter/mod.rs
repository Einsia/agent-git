//! Runtime Adapter —— 把一个 runtime 的原始 session 解析成 runtime-neutral 的 SessionIR。
//!
//! session 模型下,adapter 只做一件确定性的事:读 session 文件 → SessionIR。
//! 唯一消费者是 `reconcile` 的 `brief()`(session.rs):从 IR 里取 prompt / 最后几段 agent 文本 /
//! 改动文件,压成紧凑摘要喂给合并用的 LLM。
//!
//! （旧的"证据候选池 → Summarizer → fact"两层抽取已随 fact 模型删除;这里不再蒸馏结论。）

pub mod claude_code;
pub mod codex;

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// 一条 Read 调用（agent 实际看过的文件片段）。
#[derive(Debug, Clone)]
pub struct FileRead {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// runtime-neutral 的规范化 session。每个 adapter 的 export 先产出它。
#[derive(Debug, Default)]
pub struct SessionIR {
    pub runtime: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// 真实用户 prompt（已剔除命令注入、caveat、工具结果）。brief 用。
    pub prompts: Vec<String>,
    /// agent 读过的文件。当前 brief 不消费,保留以备摘要增强。
    pub reads: Vec<FileRead>,
    /// agent 跑过的命令。当前 brief 不消费,保留以备摘要增强。
    pub commands: Vec<String>,
    /// agent 改动过的文件 —— brief 里作为"改动的文件"列出。
    pub writes: Vec<String>,
    /// assistant 的文本块 —— brief 取最后几段作为"结论/进展"。
    pub agent_texts: Vec<String>,
    /// tool_use 块总数（含未归类的工具）—— Hub 渲染"N 次工具调用"。
    pub tool_uses: usize,
}

/// PRD 指定的三方法。Codex 与 ClaudeCode 都实现它。
pub trait Adapter {
    fn name(&self) -> &'static str;

    /// runtime session → 规范化 IR。确定性、不调模型。reconcile 的 brief 靠它读会话。
    /// `session` 为 None 时由 adapter 自行定位当前项目的最新 session。
    fn export(&self, session: Option<&Path>, cwd: &Path) -> Result<SessionIR>;

    /// 校验一个 session 文件对本 runtime 是否格式合法。
    fn validate(&self, session: &Path) -> Result<()>;

    /// 定位当前项目的默认（最新）session。没有 session 概念的 adapter 可返回错误。
    fn locate_default(&self, cwd: &Path) -> Result<PathBuf>;
}

/// 按名字取 adapter。新 runtime 在这里登记。
pub fn get(runtime: &str) -> Result<Box<dyn Adapter>> {
    match runtime {
        "claude-code" | "claude" | "cc" => Ok(Box::new(claude_code::ClaudeCode)),
        "codex" => Ok(Box::new(codex::Codex)),
        other => bail!("未知 runtime `{other}`。已注册: claude-code, codex（桩）"),
    }
}

pub fn list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude-code", "Claude Code —— 解析 ~/.claude/projects/<slug>/<session>.jsonl（已实现）"),
        ("codex", "Codex —— 接口已预留，export/validate 待实现（桩）"),
    ]
}
