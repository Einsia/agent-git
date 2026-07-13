//! Runtime Adapter —— session → AgentState 的抽取，以及跨 runtime 的 import/export。
//!
//! PRD：「Codex、ClaudeCode 等 runtime 只需实现 export、import 和 validate Adapter。」
//!
//! 设计要点：抽取分两层。
//!   1. 确定性层（本模块 + 各 adapter 的 export）：把 session 解析成 runtime-neutral 的
//!      SessionIR，再确定性地导出能确定的部分 —— 目标（来自 prompt）、artifact（来自
//!      Write/Edit）、**证据候选池**（来自 Read/Bash，且当场对齐到当前代码基线算摘要）。
//!   2. 语义层（可插拔的 Summarizer，暂缺）：把证据池 + agent 文本归纳成「结论（fact）」
//!      与「决定」。这一层需要模型，留一个清晰的 seam，MVP 不在闭环里跑它。
//!
//! 这样「模型编造出处」在构造上不可能：facts 的证据只能来自 Read/Bash 真实发生过的调用。

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
    /// 真实用户 prompt（已剔除命令注入、caveat、工具结果）。
    pub prompts: Vec<String>,
    /// agent 读过的文件 —— 证据候选池的 file: 部分。
    pub reads: Vec<FileRead>,
    /// agent 跑过的命令 —— 证据候选池的 cmd: 部分。
    pub commands: Vec<String>,
    /// agent 改动过的文件 —— artifact。
    pub writes: Vec<String>,
    /// assistant 的文本块 —— 交给 Summarizer 归纳进度/决定用。
    pub agent_texts: Vec<String>,
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
