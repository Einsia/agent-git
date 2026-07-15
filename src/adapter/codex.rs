//! Codex adapter —— 解析 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`。
//!
//! 真实结构（本机核对 838 份 rollout,2025–2026）：每行 `{timestamp, type, payload}`。
//!   type=session_meta   payload:{id, cwd, git:{branch}}   —— 项目归属藏在这里
//!   type=event_msg      payload.type=user_message  → 真实用户 prompt(可能 <task>…</task> 包裹)
//!                       payload.type=agent_message → assistant 最终文本
//!                       payload.type=patch_apply_end → payload.changes 的 key 是被改的文件
//!   type=response_item  payload.type=function_call：
//!                         name=shell        args {"command":["bash","-lc","<script>"]}   (2025)
//!                         name=shell_command args {"command":"<cmd>"}                     (string)
//!                         name=exec_command  args {"cmd":"<cmd>"}                          (主流)
//!                         name=reasoning     加密 CoT,无明文,跳过
//!
//! 与 Claude 的差异:Claude 按项目 slug 分目录;Codex 按**日期**分目录、各项目混在一起,
//! 且 fork/resume 会把**另一个项目**的父会话内嵌进同一份 rollout。所以项目归属必须
//! 扫**全部** session_meta —— 出现任一异项目 cwd 就整份跳过(见 id_if_owned,隐私底线)。

use super::{Adapter, SessionIR};
use anyhow::{bail, Context, Result};
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub struct Codex;

/// ~/.codex/sessions 根。
pub fn sessions_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("读不到 $HOME")?;
    Ok(PathBuf::from(home).join(".codex/sessions"))
}

/// 递归列出所有 rollout-*.jsonl。
fn all_rollouts(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// 一条 rollout 是否**完全属于**目标项目 → Some(session_id),否则 None。
///
/// 隐私底线:fork/resume 的 rollout 会内嵌另一个项目的父会话(其 session_meta.cwd 不同)。
/// 只要扫到任一 cwd != target 就返回 None、整份跳过 —— 绝不把他项目的会话 copy 进来。
/// 逐行读(BufRead,内存有界);扫到异项目 cwd 立即短路,非本项目文件通常读到第一行就返回。
fn id_if_owned(path: &Path, target: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(f);
    let mut id: Option<String> = None;
    let mut matched = false;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if rec.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }
        let Some(p) = rec.get("payload") else { continue };
        match p.get("cwd").and_then(|c| c.as_str()) {
            Some(c) if c == target => {
                matched = true;
                if id.is_none() {
                    id = Some(
                        p.get("id")
                            .and_then(|i| i.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| file_id(path)),
                    );
                }
            }
            Some(_) => return None, // 异项目会话在同一份 rollout 里 → 整份跳过
            None => {}              // session_meta 无 cwd,忽略这条(不算匹配也不算异项目)
        }
    }
    if matched {
        id
    } else {
        None
    }
}

/// rollout 文件名里的 session uuid（末尾那段 UUID），作为兜底 id。
fn file_id(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // rollout-<ISO8601>-<uuid>：uuid 是最后 5 段（8-4-4-4-12）
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        parts[parts.len() - 5..].join("-")
    } else {
        stem
    }
}

/// 属于某个项目（所有 session_meta.cwd == 该项目根）的 rollout：返回 (源文件, 目标 session_id)。
/// session.rs 的 codex 同步用它,只镜像**完全属于**本项目的会话。
pub fn project_rollouts(cwd: &Path) -> Vec<(PathBuf, String)> {
    let Ok(root) = sessions_root() else {
        return vec![];
    };
    let want = cwd.to_string_lossy();
    let mut out = vec![];
    for f in all_rollouts(&root) {
        if let Some(id) = id_if_owned(&f, &want) {
            out.push((f, id));
        }
    }
    out
}

impl Adapter for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn locate_default(&self, cwd: &Path) -> Result<PathBuf> {
        let root = sessions_root()?;
        if !root.exists() {
            bail!("找不到 Codex session 目录:{}（这台机器上跑过 Codex 吗?）", root.display());
        }
        let want = cwd.to_string_lossy();
        let latest = all_rollouts(&root)
            .into_iter()
            .filter(|f| id_if_owned(f, &want).is_some())
            .max_by_key(|f| {
                std::fs::metadata(f)
                    .and_then(|m| m.modified())
                    .ok()
                    .unwrap_or(std::time::UNIX_EPOCH)
            });
        latest.with_context(|| {
            format!("{} 下没有 cwd={} 的 Codex rollout（这个项目在 Codex 里跑过吗?）", root.display(), want)
        })
    }

    fn validate(&self, session: &Path) -> Result<()> {
        let f = std::fs::File::open(session)
            .with_context(|| format!("读不到 {}", session.display()))?;
        let reader = std::io::BufReader::new(f);
        let mut saw_json = false;
        for line in reader.lines().take(40) {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                bail!("含非 JSON 行，不像 Codex rollout（.jsonl）");
            };
            saw_json = true;
            if matches!(
                v.get("type").and_then(|t| t.as_str()),
                Some("session_meta" | "response_item" | "event_msg" | "turn_context")
            ) {
                return Ok(());
            }
        }
        if saw_json {
            Ok(())
        } else {
            bail!("空 rollout")
        }
    }

    fn export(&self, session: Option<&Path>, cwd: &Path) -> Result<SessionIR> {
        let path = match session {
            Some(p) => p.to_path_buf(),
            None => self.locate_default(cwd)?,
        };
        self.validate(&path)?;
        let text = std::fs::read_to_string(&path)?;
        Ok(parse_rollout(&text, &file_id(&path)))
    }
}

/// 纯解析:一坨 Codex rollout jsonl → SessionIR。不碰磁盘。
/// **唯一的 Codex 解析实现**（和 claude_code::parse_jsonl 对称）。
pub fn parse_rollout(text: &str, fallback_id: &str) -> SessionIR {
    let mut ir = SessionIR {
        runtime: "codex".into(),
        session_id: fallback_id.to_string(),
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
        let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let p = rec.get("payload");

        match ty {
            "session_meta" => {
                if let Some(p) = p {
                    if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
                        ir.session_id = id.to_string();
                    }
                    if let Some(c) = p.get("cwd").and_then(|v| v.as_str()) {
                        ir.cwd = Some(c.to_string());
                    }
                    if let Some(b) = p.get("git").and_then(|g| g.get("branch")).and_then(|v| v.as_str()) {
                        if !b.is_empty() {
                            ir.git_branch = Some(b.to_string());
                        }
                    }
                }
            }
            "event_msg" => {
                let sub = p.and_then(|p| p.get("type")).and_then(|v| v.as_str()).unwrap_or("");
                match sub {
                    "user_message" => {
                        if let Some(m) = p.and_then(|p| p.get("message")).and_then(|v| v.as_str()) {
                            if let Some(pr) = clean_prompt(m) {
                                ir.prompts.push(pr);
                            }
                        }
                    }
                    "agent_message" => {
                        if let Some(m) = p.and_then(|p| p.get("message")).and_then(|v| v.as_str()) {
                            if !m.trim().is_empty() {
                                ir.agent_texts.push(m.to_string());
                            }
                        }
                    }
                    "patch_apply_end" => {
                        // 现代 Codex 把文件改动记在这里:payload.changes 的 key 是路径。
                        if let Some(ch) = p.and_then(|p| p.get("changes")).and_then(|c| c.as_object()) {
                            for k in ch.keys() {
                                ir.writes.push(k.clone());
                            }
                        } else if let Some(so) = p.and_then(|p| p.get("stdout")).and_then(|v| v.as_str()) {
                            patch_stdout_files(so, &mut ir.writes);
                        }
                    }
                    _ => {}
                }
            }
            "response_item" => {
                if p.and_then(|p| p.get("type")).and_then(|v| v.as_str()) == Some("function_call") {
                    ir.tool_uses += 1;
                    let name = p.and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(name, "shell" | "local_shell" | "shell_command" | "exec_command") {
                        let args = p.and_then(|p| p.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(cmd) = extract_command(args) {
                            patch_files(&cmd, &mut ir.writes); // 老式 apply_patch heredoc 兜底
                            ir.commands.push(cmd);
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

const INJECTED: &[&str] = &["<environment_context", "<user_instructions", "<heartbeat", "<system"];

/// event_msg/user_message → 真实 prompt。解开 `<task>…</task>` 包裹(StarryOS 等 harness 会这样发),
/// 只丢弃已知的注入式标签(environment_context / heartbeat 等),不再一刀切"以 '<' 开头即丢"。
fn clean_prompt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(rest) = t.strip_prefix("<task>") {
        let inner = rest.strip_suffix("</task>").unwrap_or(rest).trim();
        return if inner.is_empty() { None } else { Some(inner.to_string()) };
    }
    if INJECTED.iter().any(|k| t.starts_with(k)) {
        return None;
    }
    Some(t.to_string())
}

/// shell / shell_command / exec_command 的 arguments(JSON 串)→ 实际命令。
/// 键名两种:exec_command 用 "cmd"(string);shell/shell_command 用 "command"(array 或 string)。
fn extract_command(args_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let cmd = v.get("cmd").or_else(|| v.get("command"))?;
    if let Some(arr) = cmd.as_array() {
        let parts: Vec<String> = arr.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        if let Some(i) = parts.iter().position(|p| p == "-lc" || p == "-c") {
            if let Some(s) = parts.get(i + 1) {
                return Some(s.clone());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    } else {
        cmd.as_str().map(String::from)
    }
}

/// 老式:shell 脚本里内嵌 `apply_patch` heredoc → 抽出被改文件（best-effort）。
fn patch_files(script: &str, out: &mut Vec<String>) {
    if !script.contains("apply_patch") {
        return;
    }
    for line in script.lines() {
        let l = line.trim();
        for pfx in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(p) = l.strip_prefix(pfx) {
                out.push(p.trim().to_string());
            }
        }
    }
}

/// patch_apply_end 无结构化 changes 时,退回解析 stdout 的 "A/M/D <path>" 行。
fn patch_stdout_files(stdout: &str, out: &mut Vec<String>) {
    for line in stdout.lines() {
        let l = line.trim();
        if let Some(rest) = l
            .strip_prefix("A ")
            .or_else(|| l.strip_prefix("M "))
            .or_else(|| l.strip_prefix("D "))
        {
            let p = rest.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    }
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

// ─────────────────── ConversationIR:无损读写(convert 用)───────────────────

use crate::convo::{narrate_call, truncate, ConversationIR, ConvertOpts, Event, EventKind};

/// codex rollout → ConversationIR(逐字保留每行 + 语义叠加)。
pub fn read_conversation(text: &str) -> ConversationIR {
    let mut ir = ConversationIR {
        source_runtime: "codex".into(),
        ..Default::default()
    };
    for line in text.lines() {
        let rec: Option<serde_json::Value> = serde_json::from_str(line.trim()).ok();
        let mut kinds = vec![];
        let mut ts = None;
        if let Some(rec) = &rec {
            ts = rec.get("timestamp").and_then(|v| v.as_str()).map(String::from);
            let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let p = rec.get("payload");
            match ty {
                "session_meta" => {
                    if let Some(p) = p {
                        if ir.session_id.is_empty() {
                            if let Some(id) = p.get("id").and_then(|v| v.as_str()) {
                                ir.session_id = id.to_string();
                            }
                        }
                        if ir.cwd.is_none() {
                            if let Some(c) = p.get("cwd").and_then(|v| v.as_str()) {
                                if !c.is_empty() {
                                    ir.cwd = Some(c.to_string());
                                }
                            }
                        }
                        if ir.git_branch.is_none() {
                            if let Some(b) = p.get("git").and_then(|g| g.get("branch")).and_then(|v| v.as_str()) {
                                if !b.is_empty() {
                                    ir.git_branch = Some(b.to_string());
                                }
                            }
                        }
                        if ir.system_prompt.is_none() {
                            if let Some(bi) = p
                                .get("base_instructions")
                                .or_else(|| p.get("instructions"))
                                .and_then(|v| v.as_str())
                            {
                                if !bi.is_empty() {
                                    ir.system_prompt = Some(bi.to_string());
                                }
                            }
                        }
                    }
                }
                "event_msg" => {
                    let sub = p.and_then(|p| p.get("type")).and_then(|v| v.as_str()).unwrap_or("");
                    let msg = p.and_then(|p| p.get("message")).and_then(|v| v.as_str());
                    match sub {
                        "user_message" => {
                            if let Some(pr) = msg.and_then(clean_prompt) {
                                kinds.push(EventKind::UserPrompt(pr));
                            }
                        }
                        "agent_message" => {
                            if let Some(m) = msg {
                                if !m.trim().is_empty() {
                                    kinds.push(EventKind::AssistantText(m.to_string()));
                                }
                            }
                        }
                        "patch_apply_end" => {
                            let mut paths = vec![];
                            if let Some(ch) = p.and_then(|p| p.get("changes")).and_then(|c| c.as_object()) {
                                for k in ch.keys() {
                                    paths.push(k.clone());
                                }
                            } else if let Some(so) = p.and_then(|p| p.get("stdout")).and_then(|v| v.as_str()) {
                                patch_stdout_files(so, &mut paths);
                            }
                            if !paths.is_empty() {
                                kinds.push(EventKind::FileEdit { paths });
                            }
                        }
                        _ => {}
                    }
                }
                "response_item" => {
                    if p.and_then(|p| p.get("type")).and_then(|v| v.as_str()) == Some("function_call") {
                        let name = p.and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                        if matches!(name, "shell" | "local_shell" | "shell_command" | "exec_command") {
                            let args = p.and_then(|p| p.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");
                            if let Some(cmd) = extract_command(args) {
                                kinds.push(EventKind::ToolCall {
                                    call_id: p.and_then(|p| p.get("call_id")).and_then(|v| v.as_str()).map(String::from),
                                    name: "shell".into(),
                                    input: serde_json::json!({ "command": cmd }),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        ir.events.push(Event { raw: line.to_string(), kinds, id: None, parent_id: None, timestamp: ts });
    }
    ir
}

/// ConversationIR → codex rollout。同 vendor 走 raw 重放,跨 vendor 走合成。
pub fn write_conversation(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    if ir.source_runtime == "codex" {
        same_vendor_codex(ir, opts)
    } else {
        cross_to_codex(ir, opts)
    }
}

fn same_vendor_codex(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    let mut lines = Vec::with_capacity(ir.events.len());
    for e in &ir.events {
        let mut raw = e.raw.clone();
        // 引号锚定替换,避免子串/前缀误伤与空串炸开(见 convo::swap_quoted)。
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

const CTS: &str = "2026-01-01T00:00:00.000Z"; // 合成时间戳

fn resp_message(role: &str, ctype: &str, text: &str) -> String {
    serde_json::json!({
        "timestamp": CTS, "type": "response_item",
        "payload": {"type":"message","role":role,"content":[{"type":ctype,"text":text}]}
    })
    .to_string()
}
fn ev_msg(sub: &str, text: &str) -> String {
    serde_json::json!({
        "timestamp": CTS, "type": "event_msg",
        "payload": {"type":sub,"message":text}
    })
    .to_string()
}

/// 跨 vendor(claude → codex):合成一份 rollout。response_item 是 codex 重放读取的通道,
/// event_msg 是 UI 通道 —— 两者都发,和真实 rollout 结构一致。工具活动默认叙述成文本。
fn cross_to_codex(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    let cwd = opts.cwd.clone().or_else(|| ir.cwd.clone()).unwrap_or_default();
    let branch = ir.git_branch.clone().unwrap_or_default();
    let mut lines = vec![serde_json::json!({
        "timestamp": CTS, "type": "session_meta",
        "payload": {
            "id": opts.new_id, "timestamp": CTS, "cwd": cwd,
            "originator": "agit-convert", "cli_version": "0.0.0",
            "instructions": ir.system_prompt.clone(), "git": {"branch": branch}
        }
    })
    .to_string()];

    for e in &ir.events {
        for k in &e.kinds {
            let (role, ctype, sub, text) = match k {
                EventKind::UserPrompt(s) => ("user", "input_text", "user_message", s.clone()),
                EventKind::AssistantText(s) => ("assistant", "output_text", "agent_message", s.clone()),
                EventKind::ToolCall { name, input, .. } => {
                    ("assistant", "output_text", "agent_message", narrate_call(name, input))
                }
                EventKind::ToolResult { output, .. } => {
                    ("assistant", "output_text", "agent_message", format!("[output] {}", truncate(output, 2000)))
                }
                EventKind::FileEdit { paths } => {
                    ("assistant", "output_text", "agent_message", format!("[edited: {}]", paths.join(", ")))
                }
            };
            if text.trim().is_empty() {
                continue;
            }
            lines.push(resp_message(role, ctype, &text));
            lines.push(ev_msg(sub, &text));
        }
    }
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // 现代格式:<task> prompt、exec_command(cmd 键)、shell_command(command string)、
    // patch_apply_end(changes)、以及一个非命令工具(write_stdin)只计入 tool_uses。
    const ROLLOUT: &str = r#"
{"type":"session_meta","payload":{"id":"sess-1","cwd":"/proj/x","git":{"branch":"feature"}}}
{"type":"event_msg","payload":{"type":"user_message","message":"<task>\nImplement the refund flow\n</task>"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>injected</environment_context>"}]}}
{"type":"response_item","payload":{"type":"reasoning","content":null,"encrypted_content":"gAAAA..."}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\",\"workdir\":\"/proj/x\"}"}}
{"type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{\"command\":\"cat README.md\",\"workdir\":\".\"}"}}
{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls -la\"]}"}}
{"type":"response_item","payload":{"type":"function_call","name":"write_stdin","arguments":"{\"text\":\"y\"}"}}
{"type":"event_msg","payload":{"type":"patch_apply_end","success":true,"changes":{"/proj/x/src/main.rs":{"type":"update"},"/proj/x/src/new.rs":{"type":"add"}}}}
{"type":"event_msg","payload":{"type":"agent_message","message":"done: refund flow added"}}
"#;

    #[test]
    fn parse_rollout_modern_format() {
        let ir = parse_rollout(ROLLOUT, "fallback");
        assert_eq!(ir.session_id, "sess-1");
        assert_eq!(ir.git_branch.as_deref(), Some("feature"));
        assert_eq!(ir.cwd.as_deref(), Some("/proj/x"));
        // <task> unwrapped; injected <environment_context> (a response_item, not event_msg) never seen
        assert_eq!(ir.prompts, vec!["Implement the refund flow"]);
        assert_eq!(ir.agent_texts, vec!["done: refund flow added"]);
        // commands from exec_command(cmd) + shell_command(command str) + shell(-lc array)
        assert!(ir.commands.contains(&"cargo test".to_string()), "{:?}", ir.commands);
        assert!(ir.commands.contains(&"cat README.md".to_string()), "{:?}", ir.commands);
        assert!(ir.commands.contains(&"ls -la".to_string()), "{:?}", ir.commands);
        // writes from patch_apply_end.changes keys
        assert!(ir.writes.contains(&"/proj/x/src/main.rs".to_string()), "{:?}", ir.writes);
        assert!(ir.writes.contains(&"/proj/x/src/new.rs".to_string()), "{:?}", ir.writes);
        // every function_call counts (exec + shell_command + shell + write_stdin = 4)
        assert_eq!(ir.tool_uses, 4);
    }

    #[test]
    fn conversation_roundtrip_byte_faithful() {
        let src = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"S1\",\"cwd\":\"/p\",\"git\":{\"branch\":\"main\"}}}\n\
                   {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"do the thing\"}}\n\
                   {\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n";
        let ir = read_conversation(src);
        assert_eq!(ir.session_id, "S1");
        assert_eq!(ir.cwd.as_deref(), Some("/p"));
        let opts = ConvertOpts { cwd: None, new_id: "S1".into() };
        assert_eq!(write_conversation(&ir, &opts), src, "same-vendor replay must reproduce input");
        let kinds: Vec<&EventKind> = ir.events.iter().flat_map(|e| e.kinds.iter()).collect();
        assert!(kinds.iter().any(|k| matches!(k, EventKind::UserPrompt(p) if p == "do the thing")));
        assert!(kinds.iter().any(|k| matches!(k, EventKind::ToolCall { .. })));
    }

    #[test]
    fn clean_prompt_handles_task_and_injection() {
        assert_eq!(clean_prompt("<task>\ndo the thing\n</task>").as_deref(), Some("do the thing"));
        assert_eq!(clean_prompt("plain prompt").as_deref(), Some("plain prompt"));
        assert_eq!(clean_prompt("<environment_context>x</environment_context>"), None);
        assert_eq!(clean_prompt("   "), None);
    }

    #[test]
    fn extract_command_reads_cmd_and_command() {
        assert_eq!(extract_command(r#"{"cmd":"pwd && ls"}"#).as_deref(), Some("pwd && ls"));
        assert_eq!(extract_command(r#"{"command":"cat x"}"#).as_deref(), Some("cat x"));
        assert_eq!(extract_command(r#"{"command":["bash","-lc","echo hi"]}"#).as_deref(), Some("echo hi"));
        assert_eq!(extract_command("not json").as_deref(), None);
    }

    #[test]
    fn patch_stdout_files_parses_amd() {
        let mut out = vec![];
        patch_stdout_files("Success. Updated the following files:\nA /a/x.rs\nM /a/y.rs\nD /a/z.rs", &mut out);
        assert_eq!(out, vec!["/a/x.rs", "/a/y.rs", "/a/z.rs"]);
    }
}
