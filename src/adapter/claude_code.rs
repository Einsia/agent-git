//! Claude Code adapter：解析 ~/.claude/projects/<slug>/<session>.jsonl。
//!
//! 真实结构（本机核对，2026-07）：每行一条 JSON 记录，带 cwd / gitBranch / timestamp。
//!   type=user      message.content 是 str（真实 prompt 或 <...> 注入）或含 tool_result 的 list
//!   type=assistant message.content 是 block list：tool_use / thinking / text
//!   tool_use: {name, input}  —— Read{file_path,offset,limit} / Bash{command} / Write|Edit{file_path}

use super::{Adapter, FileRead, SessionIR};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct ClaudeCode;

/// cwd → Claude Code 的 project slug：绝对路径里的 '/' **和 '.'** 都换成 '-'。
/// 核对真实目录:`/home/user/bolusi/.claude/worktrees` → `-home-user-bolusi--claude-worktrees`
/// (`.claude` 的点也塌成 '-',故双连字符)。只换 '/' 会对任何含点的路径算出错误 slug、找不到目录。
pub fn slug_for(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '.'], "-")
}

pub fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("读不到 $HOME")?;
    Ok(PathBuf::from(home).join(".claude/projects"))
}

/// Claude Code 会往转录里注入一批 XML 式标签（不是真实 prompt）。这些是已知的注入标签名。
const INJECTED_TAGS: &[&str] = &[
    "system-reminder",
    "command-name",
    "command-message",
    "command-args",
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "user-prompt-submit-hook",
    "caveat",
    "budget",
    "session-start",
    "important",
    "policy",
    "function_results",
    "tool_use_error",
];

/// 真实用户 prompt：非空；若以 '<' 开头，仅当其后不是已知注入标签时才算真实。
/// 旧逻辑一刀切"以 '<' 开头即丢"，把 `<div> 不渲染` 这类合法 prompt 也误杀了。
fn is_real_prompt(s: &str) -> bool {
    let t = s.trim_start();
    if t.is_empty() {
        return false;
    }
    let Some(rest) = t.strip_prefix('<') else {
        return true;
    };
    let tag: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    !INJECTED_TAGS.contains(&tag.as_str())
}

impl Adapter for ClaudeCode {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn locate_default(&self, cwd: &Path) -> Result<PathBuf> {
        let dir = projects_dir()?.join(slug_for(cwd));
        if !dir.exists() {
            bail!(
                "找不到本项目的 Claude Code session 目录：{}\n\
                 （slug 由 cwd 推出。换个目录，或用 `agit -a import <session.jsonl>` 显式指定。）",
                dir.display()
            );
        }
        let latest = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .max_by_key(|e| {
                e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .unwrap_or(std::time::UNIX_EPOCH)
            })
            .map(|e| e.path())
            .with_context(|| format!("{} 里没有 .jsonl session", dir.display()))?;
        Ok(latest)
    }

    fn validate(&self, session: &Path) -> Result<()> {
        let text = std::fs::read_to_string(session)
            .with_context(|| format!("读不到 {}", session.display()))?;
        // 前几行含 mode / permission-mode / system 等非消息记录，只要求：
        // 是 JSONL，且早期出现过带已知 type 的记录。
        let mut saw_json = false;
        for line in text.lines().take(40) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                bail!("含非 JSON 行，不像 Claude Code session（.jsonl）");
            };
            saw_json = true;
            if matches!(
                v.get("type").and_then(|t| t.as_str()),
                Some("user" | "assistant" | "system")
            ) {
                return Ok(());
            }
        }
        if saw_json {
            Ok(()) // 是 JSONL，只是前 40 行没碰到消息记录
        } else {
            bail!("空 session")
        }
    }

    fn export(&self, session: Option<&Path>, cwd: &Path) -> Result<SessionIR> {
        let path = match session {
            Some(p) => p.to_path_buf(),
            None => self.locate_default(cwd)?,
        };
        self.validate(&path)?;
        let text = std::fs::read_to_string(&path)?;
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(parse_jsonl(&text, &id))
    }
}

/// 纯解析:一坨 Claude Code jsonl → SessionIR。不碰磁盘,无 IO。
/// **唯一的转录解析实现** —— `export`(agit)和 Hub 渲染都走它,免得两处规则漂移
/// (prompt 过滤、isCompactSummary 排除等只在这里改一次)。
pub fn parse_jsonl(text: &str, session_id: &str) -> SessionIR {
    let mut ir = SessionIR {
        runtime: "claude-code".into(),
        session_id: session_id.to_string(),
        ..Default::default()
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // 环境线索：取第一条带 cwd/gitBranch 的记录
        if ir.cwd.is_none() {
            if let Some(c) = rec.get("cwd").and_then(|v| v.as_str()) {
                ir.cwd = Some(c.to_string());
            }
        }
        if ir.git_branch.is_none() {
            if let Some(b) = rec.get("gitBranch").and_then(|v| v.as_str()) {
                if !b.is_empty() {
                    ir.git_branch = Some(b.to_string());
                }
            }
        }

        let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let is_meta = rec.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false);
        // compaction 会写一条合成的 user 记录(isCompactSummary=true)当摘要 —— 它不是
        // 真实用户 prompt,当成 prompt 会把压缩摘要混进 brief、污染合并输入。排除之。
        let is_compact = rec
            .get("isCompactSummary")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = rec.get("message").and_then(|m| m.get("content"));

        match ty {
            "user" if !is_meta && !is_compact => {
                if let Some(s) = content.and_then(|c| c.as_str()) {
                    if is_real_prompt(s) {
                        ir.prompts.push(s.trim().to_string());
                    }
                }
            }
            "assistant" => {
                if let Some(blocks) = content.and_then(|c| c.as_array()) {
                    for b in blocks {
                        match b.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                    ir.agent_texts.push(t.to_string());
                                }
                            }
                            Some("tool_use") => {
                                ir.tool_uses += 1;
                                collect_tool(b, &mut ir);
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    dedup(&mut ir.commands);
    dedup(&mut ir.writes);
    ir
}

fn collect_tool(b: &serde_json::Value, ir: &mut SessionIR) {
    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let input = b.get("input");
    let s = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_str()).map(String::from);
    let n = |k: &str| input.and_then(|i| i.get(k)).and_then(|v| v.as_u64()).map(|x| x as usize);

    match name {
        "Read" => {
            if let Some(path) = s("file_path") {
                ir.reads.push(FileRead {
                    path,
                    offset: n("offset"),
                    limit: n("limit"),
                });
            }
        }
        "Bash" => {
            if let Some(cmd) = s("command") {
                ir.commands.push(cmd);
            }
        }
        "Write" | "Edit" => {
            if let Some(path) = s("file_path") {
                ir.writes.push(path);
            }
        }
        _ => {}
    }
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

// ─────────────────── ConversationIR:无损读写(convert 用)───────────────────

use crate::convo::{narrate_call, truncate, ConversationIR, ConvertOpts, Event, EventKind};

/// 合成 claude 记录用的 schema version(核对真实转录:2.1.207)。
const CLAUDE_VERSION: &str = "2.1.207";

/// claude jsonl → ConversationIR(逐字保留每行 + 语义叠加)。
pub fn read_conversation(text: &str) -> ConversationIR {
    let mut ir = ConversationIR {
        source_runtime: "claude-code".into(),
        ..Default::default()
    };
    for line in text.lines() {
        let rec: Option<serde_json::Value> = serde_json::from_str(line.trim()).ok();
        let (mut kinds, mut id, mut parent, mut ts) = (vec![], None, None, None);
        if let Some(rec) = &rec {
            if ir.cwd.is_none() {
                if let Some(c) = rec.get("cwd").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        ir.cwd = Some(c.to_string());
                    }
                }
            }
            if ir.git_branch.is_none() {
                if let Some(b) = rec.get("gitBranch").and_then(|v| v.as_str()) {
                    if !b.is_empty() {
                        ir.git_branch = Some(b.to_string());
                    }
                }
            }
            if ir.session_id.is_empty() {
                if let Some(s) = rec.get("sessionId").and_then(|v| v.as_str()) {
                    ir.session_id = s.to_string();
                }
            }
            id = rec.get("uuid").and_then(|v| v.as_str()).map(String::from);
            parent = rec.get("parentUuid").and_then(|v| v.as_str()).map(String::from);
            ts = rec.get("timestamp").and_then(|v| v.as_str()).map(String::from);
            kinds = extract_claude_kinds(rec);
        }
        ir.events.push(Event { raw: line.to_string(), kinds, id, parent_id: parent, timestamp: ts });
    }
    ir
}

fn extract_claude_kinds(rec: &serde_json::Value) -> Vec<EventKind> {
    let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let is_meta = rec.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false);
    let content = rec.get("message").and_then(|m| m.get("content"));
    let mut out = vec![];
    match ty {
        "user" if !is_meta => match content {
            Some(serde_json::Value::String(s)) => {
                if is_real_prompt(s) {
                    out.push(EventKind::UserPrompt(s.trim().to_string()));
                }
            }
            Some(serde_json::Value::Array(arr)) => {
                // 多模态 prompt:content 是 block 数组([{text},{image},…])。
                // 既要抽 text 块当真实 prompt(否则带图的用户提问会被整条丢掉),也要抽 tool_result。
                for b in arr {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if is_real_prompt(t) {
                                    out.push(EventKind::UserPrompt(t.trim().to_string()));
                                }
                            }
                        }
                        Some("tool_result") => out.push(EventKind::ToolResult {
                            call_id: b.get("tool_use_id").and_then(|v| v.as_str()).map(String::from),
                            output: tool_result_text(b.get("content")),
                        }),
                        _ => {}
                    }
                }
            }
            _ => {}
        },
        "assistant" => {
            if let Some(serde_json::Value::Array(arr)) = content {
                for b in arr {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    out.push(EventKind::AssistantText(t.to_string()));
                                }
                            }
                        }
                        Some("tool_use") => out.push(EventKind::ToolCall {
                            call_id: b.get("id").and_then(|v| v.as_str()).map(String::from),
                            name: b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            input: b.get("input").cloned().unwrap_or(serde_json::Value::Null),
                        }),
                        _ => {} // thinking:加密,跨 vendor 丢弃(同 vendor 由 raw 保留)
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| x.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// ConversationIR → claude jsonl。同 vendor 走 raw 重放(字节级),跨 vendor 走合成。
pub fn write_conversation(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    if ir.source_runtime == "claude-code" {
        same_vendor_claude(ir, opts)
    } else {
        cross_to_claude(ir, opts)
    }
}

/// 同 vendor:逐行重放 raw,只把源 session id 换成新 id(+可选 cwd 覆盖),其余字节不动。
/// 用引号锚定的 swap_quoted,避免子串/前缀误伤与空串炸开(见 convo::swap_quoted)。
fn same_vendor_claude(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    let mut lines = Vec::with_capacity(ir.events.len());
    for e in &ir.events {
        let mut raw = e.raw.clone();
        if !opts.new_id.is_empty() {
            raw = crate::convo::swap_quoted(&raw, &ir.session_id, &opts.new_id);
        }
        if let (Some(old), Some(new)) = (&ir.cwd, &opts.cwd) {
            raw = crate::convo::swap_quoted(&raw, old, new);
        }
        lines.push(raw);
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// 跨 vendor(codex → claude):从 kinds 合成 claude 记录,串起 parentUuid 链。
/// 工具活动叙述成文本(目标模型只需知道发生了什么,不重放外来工具 schema)。
fn cross_to_claude(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    let cwd = opts.cwd.clone().or_else(|| ir.cwd.clone()).unwrap_or_default();
    let branch = ir.git_branch.clone().unwrap_or_default();
    let ts = "2026-01-01T00:00:00.000Z"; // 合成时间戳;claude resume 不要求单调
    let mut records: Vec<String> = vec![];
    let mut parent: Option<String> = None;
    let mut counter: u64 = 0;

    let mut push = |records: &mut Vec<String>, parent: &mut Option<String>, role_user: bool, text: &str| {
        counter += 1;
        let uuid = crate::convo::fresh_id(&format!("cl{counter}"));
        let msg = if role_user {
            serde_json::json!({"role":"user","content":text})
        } else {
            serde_json::json!({"role":"assistant","content":[{"type":"text","text":text}]})
        };
        let rec = serde_json::json!({
            "type": if role_user {"user"} else {"assistant"},
            "uuid": uuid,
            "parentUuid": parent.clone(),
            "sessionId": opts.new_id,
            "cwd": cwd,
            "gitBranch": branch,
            "timestamp": ts,
            "version": CLAUDE_VERSION,
            "userType": "external",
            "isSidechain": false,
            "message": msg,
        });
        records.push(rec.to_string());
        *parent = Some(uuid);
    };

    if let Some(sp) = &ir.system_prompt {
        let note = format!(
            "[Resumed from a Codex session. Imported system instructions follow.]\n\n{}",
            truncate(sp, 4000)
        );
        push(&mut records, &mut parent, true, &note);
    }
    for e in &ir.events {
        for k in &e.kinds {
            let (user, text) = match k {
                EventKind::UserPrompt(s) => (true, s.clone()),
                EventKind::AssistantText(s) => (false, s.clone()),
                EventKind::ToolCall { name, input, .. } => (false, narrate_call(name, input)),
                EventKind::ToolResult { output, .. } => (false, format!("[output] {}", truncate(output, 2000))),
                EventKind::FileEdit { paths } => (false, format!("[edited: {}]", paths.join(", "))),
            };
            if text.trim().is_empty() {
                continue;
            }
            push(&mut records, &mut parent, user, &text);
        }
    }
    let mut s = records.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_collapses_dot_and_slash() {
        // 真实:/home/user/bolusi/.claude/worktrees → -home-user-bolusi--claude-worktrees
        assert_eq!(
            slug_for(Path::new("/home/user/bolusi/.claude/worktrees")),
            "-home-user-bolusi--claude-worktrees"
        );
        // 含点的普通路径也要塌成 '-',否则 sync 找不到目录
        assert_eq!(slug_for(Path::new("/a/b.c/d")), "-a-b-c-d");
    }

    #[test]
    fn multimodal_user_prompt_text_is_captured() {
        // user turn with an image attachment: content is a block array, not a string.
        let src = "{\"type\":\"user\",\"sessionId\":\"S\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"should we write e2e tests?\"},{\"type\":\"image\",\"source\":{}}]}}\n";
        let ir = read_conversation(src);
        let has = ir
            .events
            .iter()
            .flat_map(|e| &e.kinds)
            .any(|k| matches!(k, EventKind::UserPrompt(p) if p == "should we write e2e tests?"));
        assert!(has, "multimodal user text must not be dropped");
    }

    #[test]
    fn conversation_roundtrip_byte_faithful() {
        let src = "{\"type\":\"user\",\"sessionId\":\"S1\",\"uuid\":\"u1\",\"parentUuid\":null,\"cwd\":\"/p\",\"gitBranch\":\"main\",\"message\":{\"role\":\"user\",\"content\":\"hi there\"}}\n\
                   {\"type\":\"assistant\",\"sessionId\":\"S1\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"abc\"},{\"type\":\"text\",\"text\":\"hello\"}]}}\n";
        let ir = read_conversation(src);
        assert_eq!(ir.session_id, "S1");
        // same-vendor replay with new_id == source id → no rewrite → byte-identical
        let opts = ConvertOpts { cwd: None, new_id: "S1".into() };
        assert_eq!(write_conversation(&ir, &opts), src, "same-vendor replay must reproduce input");
        // semantic overlay: prompt + assistant text captured; thinking dropped
        let kinds: Vec<&EventKind> = ir.events.iter().flat_map(|e| e.kinds.iter()).collect();
        assert!(matches!(kinds[0], EventKind::UserPrompt(p) if p == "hi there"));
        assert!(kinds.iter().any(|k| matches!(k, EventKind::AssistantText(t) if t == "hello")));
    }

    #[test]
    fn real_prompt_keeps_angle_bracket_prose_drops_injected_tags() {
        assert!(is_real_prompt("fix the bug"));
        assert!(is_real_prompt("<div> 不渲染怎么办")); // 合法 prompt,不该丢
        assert!(is_real_prompt("<= 是小于等于"));
        assert!(!is_real_prompt("<system-reminder>...</system-reminder>"));
        assert!(!is_real_prompt("<local-command-caveat>x"));
        assert!(!is_real_prompt("   ")); // 空白
    }
}
