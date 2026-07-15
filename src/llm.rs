//! 可插拔的 LLM CLI 后端 —— agit 里所有「需要模型」的地方都走这里。
//!
//! 当前唯一消费者是 `reconcile` 的语义合并（session.rs）：读两边会话的 brief、
//! 合成统一上下文、判真冲突。存储/同步/文件层合并全是确定性的 git，只有这一层调模型。
//!
//! 后端选择（留给 Codex 的口子）：
//!   1. `AGIT_LLM_CMD="<任意命令>"`  —— 立刻可用；命令从 stdin 读 prompt、stdout 出结果。
//!                                      例：export AGIT_LLM_CMD="codex exec -"
//!   2. `AGIT_LLM=claude`（默认）    —— 本机 `claude -p`
//!   3. `AGIT_LLM=codex`            —— 预留：拿到 codex 的非交互调用方式后在此填入

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

enum Backend {
    Claude,
    Codex,
    /// AGIT_LLM_CMD：整条命令，经 `sh -c` 执行，prompt 走 stdin。
    Cmd(String),
}

fn backend() -> Backend {
    if let Ok(c) = std::env::var("AGIT_LLM_CMD") {
        if !c.trim().is_empty() {
            return Backend::Cmd(c);
        }
    }
    match std::env::var("AGIT_LLM").unwrap_or_default().as_str() {
        "codex" => Backend::Codex,
        "claude" | "" => Backend::Claude,
        other => Backend::Cmd(other.to_string()), // 当成命令名，如 "ollama run llama3"
    }
}

fn which(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 当前后端是否真的可用（据此决定「有模型就语义对齐，没有就退回确定性启发式」）。
pub fn available() -> bool {
    match backend() {
        Backend::Claude => which("claude"),
        Backend::Codex => false, // 口子还没接
        Backend::Cmd(c) => c.split_whitespace().next().map(which).unwrap_or(false),
    }
}

pub fn backend_name() -> &'static str {
    match backend() {
        Backend::Claude => "claude",
        Backend::Codex => "codex",
        Backend::Cmd(_) => "custom",
    }
}

/// 把 prompt 喂给后端，返回它的文本回复。
pub fn ask(prompt: &str) -> Result<String> {
    let (program, args): (&str, Vec<String>) = match backend() {
        Backend::Claude => ("claude", vec!["-p".into()]),
        Backend::Codex => bail!(
            "codex LLM 后端尚未接入（已留口子）。\n\
             拿到 codex 的非交互调用方式后在 src/llm.rs 填入，或现在直接指定：\n\
             \x20 export AGIT_LLM_CMD=\"codex exec -\"   # 或任何从 stdin 读 prompt 的命令"
        ),
        Backend::Cmd(c) => ("sh", vec!["-c".into(), c]),
    };

    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("启动 LLM 后端失败：{program}（它在 PATH 里吗？）"))?;
    child
        .stdin
        .take()
        .context("拿不到后端 stdin")?
        .write_all(prompt.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("LLM 后端返回非零");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
