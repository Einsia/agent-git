//! ConversationIR —— 无损的会话中间表示,专用于**格式转换**(convert)。
//!
//! 与 SessionIR(有损摘要,给 reconcile 的 brief 用)刻意分开:convert 要能重建一份
//! 目标 runtime 能 resume 的会话,不能靠一袋 prompt/命令字符串。
//!
//! 核心设计:每个 Event 同时带
//!   - `raw`:**逐字的原始 jsonl 行**(不是 parse 回来的 Value —— serde_json 会打乱 key 顺序/格式)。
//!            同 runtime 转换直接重放 raw → 字节级还原。
//!   - `kinds`:语义叠加,只有跨 runtime 合成时才用。
//!
//! 保真度分层:同 vendor = 字节级(重放 raw);跨 vendor = 内容级(从 kinds 重建可见回合,
//! 丢加密 CoT、合成缺失的 system prompt、把工具活动叙述成文本)。见
//! docs/plans/2026-07-15-claude-codex-conversion-design.md。

use crate::adapter::{claude_code, codex};
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::Path;

/// 无损会话表示。
#[derive(Debug, Default, Clone)]
pub struct ConversationIR {
    pub source_runtime: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// codex 的 base_instructions;claude 无(转录里不含 system prompt)。
    pub system_prompt: Option<String>,
    pub events: Vec<Event>,
}

/// 一条源记录:逐字 raw + 语义叠加。
#[derive(Debug, Clone)]
pub struct Event {
    /// 逐字的原始 jsonl 行(用于同 vendor 字节级重放)。
    pub raw: String,
    /// 从这条记录抽出的语义项(可 0..n 个;reasoning/meta 等抽不出就为空)。
    pub kinds: Vec<EventKind>,
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub timestamp: Option<String>,
}

/// 跨 vendor 合成会消费的语义项。加密 reasoning、meta、token_count 等**不产生** kind
/// (只活在 raw 里,跨 vendor 时被丢弃)。
#[derive(Debug, Clone)]
pub enum EventKind {
    UserPrompt(String),
    AssistantText(String),
    ToolCall { call_id: Option<String>, name: String, input: Value },
    ToolResult { call_id: Option<String>, output: String },
    FileEdit { paths: Vec<String> },
}

/// convert 选项。
#[derive(Debug, Clone)]
pub struct ConvertOpts {
    /// 覆盖目标会话的 cwd(决定 resume 落到哪个项目)。默认沿用源 cwd。
    pub cwd: Option<String>,
    /// 分配给产物的新 session id(convert 决不复用源 id)。
    pub new_id: String,
}

impl Default for ConvertOpts {
    fn default() -> Self {
        ConvertOpts { cwd: None, new_id: String::new() }
    }
}

/// runtime 名归一(和 adapter::get 的 key 对齐)。
pub fn normalize_runtime(rt: &str) -> &'static str {
    match rt {
        "claude" | "cc" | "claude-code" => "claude-code",
        "codex" => "codex",
        _ => "",
    }
}

/// 读一份源 session 文本 → ConversationIR。
pub fn read_conversation(runtime: &str, text: &str) -> Result<ConversationIR> {
    match normalize_runtime(runtime) {
        "claude-code" => Ok(claude_code::read_conversation(text)),
        "codex" => Ok(codex::read_conversation(text)),
        _ => bail!("未知源 runtime `{runtime}`(支持 claude-code / codex)"),
    }
}

/// ConversationIR → 目标 runtime 的 session 文本。
pub fn write_conversation(runtime: &str, ir: &ConversationIR, opts: &ConvertOpts) -> Result<String> {
    match normalize_runtime(runtime) {
        "claude-code" => Ok(claude_code::write_conversation(ir, opts)),
        "codex" => Ok(codex::write_conversation(ir, opts)),
        _ => bail!("未知目标 runtime `{runtime}`(支持 claude-code / codex)"),
    }
}

/// 读源 → IR → 写目标。返回 (目标 session 文本, IR 供调用方拿 meta)。
pub fn convert(
    src_path: &Path,
    src_runtime: &str,
    target_runtime: &str,
    opts: &ConvertOpts,
) -> Result<(String, ConversationIR)> {
    let text = std::fs::read_to_string(src_path)
        .map_err(|e| anyhow::anyhow!("读源 session {} 失败: {e}", src_path.display()))?;
    let ir = read_conversation(src_runtime, &text)?;
    if ir.events.is_empty() {
        bail!("源 session 为空。");
    }
    // 跨 vendor 靠重建可见回合;源里一个语义项都没有(如只有 session_meta 的残缺 rollout)
    // 会合成出近乎空的会话、目标 CLI 拒绝 resume。提前拦下,给清楚的信息。
    if is_cross_vendor(src_runtime, target_runtime) {
        let semantic: usize = ir.events.iter().map(|e| e.kinds.len()).sum();
        if semantic == 0 {
            bail!("源里没有可重建的可见回合(prompt/回复/工具);无法跨 vendor 转换。");
        }
    }
    // 不在这里改写 ir.cwd —— 同 vendor 重放要拿**原始** cwd 才能字符串替换。
    // 目标 cwd 由 writer 从 opts.cwd 决定(缺省沿用 ir.cwd)。
    let out = write_conversation(target_runtime, &ir, opts)?;
    Ok((out, ir))
}

/// 是否跨 vendor(源与目标 runtime 不同)。
pub fn is_cross_vendor(src: &str, target: &str) -> bool {
    normalize_runtime(src) != normalize_runtime(target)
}

/// 同 vendor 重放时替换 id / cwd:把 raw 行里 **JSON 引号包裹**的 `"old"` 换成 `"new"`。
/// 引号锚定 → 不会误伤把 old 当子串/前缀的其它内容(如 /a/app 命中 /a/application),
/// old 为空或 old==new 时原样返回(避免 replace("", …) 的逐字符炸开)。
pub fn swap_quoted(raw: &str, old: &str, new: &str) -> String {
    if old.is_empty() || old == new {
        return raw.to_string();
    }
    raw.replace(&format!("\"{old}\""), &format!("\"{new}\""))
}

/// 按 char 截断,超长加标记(不在字节中间切,避免 UTF-8 panic)。
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…[truncated]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_SRC: &str = "{\"type\":\"user\",\"sessionId\":\"S1\",\"uuid\":\"u1\",\"parentUuid\":null,\"cwd\":\"/p\",\"gitBranch\":\"main\",\"message\":{\"role\":\"user\",\"content\":\"HELLO_PROMPT\"}}\n{\"type\":\"assistant\",\"sessionId\":\"S1\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"SECRETSIG\"},{\"type\":\"text\",\"text\":\"WORLD_REPLY\"},{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo test\"}}]}}\n";

    #[test]
    fn claude_to_codex_preserves_visible_drops_reasoning() {
        let opts = ConvertOpts { cwd: None, new_id: "NEWID".into() };
        let ir = read_conversation("claude-code", CLAUDE_SRC).unwrap();
        let out = write_conversation("codex", &ir, &opts).unwrap();
        // visible content survives the cross-vendor hop
        assert!(out.contains("HELLO_PROMPT"));
        assert!(out.contains("WORLD_REPLY"));
        assert!(out.contains("cargo test"), "tool narrated: {out}");
        // fresh id in session_meta; encrypted reasoning dropped
        assert!(out.contains("NEWID"));
        assert!(!out.contains("SECRETSIG"), "encrypted reasoning must not carry over");
        // re-read the codex output → the prompt is recoverable
        let ir2 = read_conversation("codex", &out).unwrap();
        let prompts: Vec<String> = ir2
            .events
            .iter()
            .flat_map(|e| &e.kinds)
            .filter_map(|k| if let EventKind::UserPrompt(s) = k { Some(s.clone()) } else { None })
            .collect();
        assert!(prompts.iter().any(|p| p == "HELLO_PROMPT"), "{prompts:?}");
    }

    #[test]
    fn convert_refuses_source_with_no_turns() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("rollout.jsonl");
        // only a session_meta, no turns
        std::fs::write(&f, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"S1\",\"cwd\":\"/p\"}}\n").unwrap();
        let opts = ConvertOpts { cwd: None, new_id: "N".into() };
        let err = convert(&f, "codex", "claude-code", &opts).unwrap_err();
        assert!(err.to_string().contains("可重建"), "{err}");
    }

    #[test]
    fn swap_quoted_anchors_and_guards() {
        // exact quoted value is replaced
        assert_eq!(swap_quoted("{\"cwd\":\"/a/app\"}", "/a/app", "/a/x"), "{\"cwd\":\"/a/x\"}");
        // a path that merely has old as a PREFIX must NOT be corrupted (the confirmed bug)
        assert_eq!(
            swap_quoted("{\"p\":\"/a/application/f\"}", "/a/app", "/a/x"),
            "{\"p\":\"/a/application/f\"}"
        );
        // empty old → no-op (no replace("",…) between-every-char explosion)
        assert_eq!(swap_quoted("{\"a\":\"b\"}", "", "/x"), "{\"a\":\"b\"}");
        // old == new → no-op
        assert_eq!(swap_quoted("x", "a", "a"), "x");
    }

    #[test]
    fn fresh_id_is_uuid_shaped_and_unique() {
        let a = fresh_id("x");
        let b = fresh_id("y");
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    }
}

/// 把一次工具调用叙述成一行文本(跨 vendor 默认路径:目标模型只需知道发生了什么,不重放)。
/// 同时认 claude(Bash/Read/Write/Edit)与 codex(shell/exec_command/…)的工具名。
pub fn narrate_call(name: &str, input: &Value) -> String {
    let get = |k: &str| input.get(k).and_then(|v| v.as_str());
    match name {
        "Bash" => format!("[ran] {}", truncate(get("command").unwrap_or(""), 500)),
        "shell" | "exec_command" | "shell_command" | "local_shell" => {
            let c = get("cmd").or_else(|| get("command")).unwrap_or("");
            format!("[ran] {}", truncate(c, 500))
        }
        "Read" => format!("[read {}]", get("file_path").unwrap_or("")),
        "Write" | "Edit" | "MultiEdit" => format!("[edited {}]", get("file_path").unwrap_or("")),
        _ => format!("[tool {name}]"),
    }
}

/// 生成一个新的、uuid 形状的 session id(8-4-4-4-12)。无 uuid crate:
/// sha256(时间纳秒 + pid + salt) 取 16 字节,version nibble 置 7(uuidv7 形)。
/// resume 只要求 id 形似 uuid 且唯一(spike 已核实),不校验真实 v7 时间戳。
pub fn fresh_id(salt: &str) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(salt.as_bytes());
    let d = h.finalize();
    let hex = hex::encode(&d[..16]);
    // 形如 xxxxxxxx-xxxx-7xxx-8xxx-xxxxxxxxxxxx
    format!(
        "{}-{}-7{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}
