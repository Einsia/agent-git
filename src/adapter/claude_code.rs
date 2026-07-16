//! Claude Code adapter: parses ~/.claude/projects/<slug>/<session>.jsonl.
//!
//! Real structure (verified locally, 2026-07): one JSON record per line, with cwd / gitBranch / timestamp.
//!   type=user      message.content is a str (a real prompt or a <...> injection) or a list containing tool_result
//!   type=assistant message.content is a block list: tool_use / thinking / text
//!   tool_use: {name, input}  -- Read{file_path,offset,limit} / Bash{command} / Write|Edit{file_path}

use super::{Adapter, FileRead, SessionIR};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct ClaudeCode;

/// cwd → Claude Code's project slug: EVERY non-alphanumeric char in the absolute path becomes '-'.
/// Claude Code slugifies by replacing anything that isn't [A-Za-z0-9], so '/', '.', '_', spaces, etc.
/// all collapse to '-'. Verified against real directories:
///   `/home/user/bolusi/.claude/worktrees` → `-home-user-bolusi--claude-worktrees` (dot → '-')
///   `/home/user/_test/payments`           → `-home-user--test-payments`          (underscore → '-')
/// Replacing only '/' and '.' would keep the underscore and fail to find the session directory.
pub fn slug_for(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("cannot read $HOME")?;
    Ok(PathBuf::from(home).join(".claude/projects"))
}

/// A transcript's owning project = the **launch cwd**: the first record that carries one. Claude derives
/// the slug from exactly that, and it is the only cwd that identifies the project.
///
/// Deliberately NOT codex's rule (`id_if_owned`: every recorded cwd must match, else drop the file).
/// Claude records `cwd` on *every* record, so one `cd` by the agent makes later records carry a
/// subdirectory. Measured on 1013 real transcripts here: 9 drift across several cwds — including this
/// repo's own session (`/home/user/agent-git` then `/home/user/agent-git/hub-ui`). An all-must-match
/// rule would silently stop capturing them. Reads line by line and stops at the first cwd.
fn launch_cwd(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue; // leading records may be queue-operations / non-JSON
        };
        if let Some(c) = rec.get("cwd").and_then(|v| v.as_str()) {
            if !c.is_empty() {
                return Some(c.to_string());
            }
        }
    }
    None
}

/// The claude sessions that belong to `env`: (transcript path, session id). The id is the file stem, so
/// callers can carry each session's sidecar dirs (`<id>/subagents`, `<id>/tool-results`) along with it.
///
/// The privacy bottom line, symmetric with `codex::project_rollouts`: `slug_for` collapses every
/// non-alphanumeric char, so `/a/b.c` and `/a/b/c` share ONE `projects/<slug>/` directory — which can
/// therefore hold a *different* project's transcripts. Mirroring the directory wholesale would copy them
/// into this project's Agent Store and push them to this project's teammates. A transcript with no cwd
/// at all is **not** owned (fail closed): better to miss a capture than to leak someone else's work.
pub fn project_sessions(env: &Path) -> Vec<(PathBuf, String)> {
    let Ok(dir) = projects_dir().map(|d| d.join(slug_for(env))) else {
        return vec![];
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let want = env.to_string_lossy();
    let mut out = vec![];
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if !p.is_file() || p.extension().map(|x| x != "jsonl").unwrap_or(true) {
            continue;
        }
        if launch_cwd(&p).as_deref() != Some(&*want) {
            continue;
        }
        if let Some(id) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) {
            out.push((p, id));
        }
    }
    out
}

/// Does this project have the slug directory to itself? Used to decide whether slug-level content that
/// belongs to no session (`memory/`) can be attributed to us at all.
pub fn slug_dir_is_exclusive(env: &Path) -> bool {
    let Ok(dir) = projects_dir().map(|d| d.join(slug_for(env))) else {
        return false;
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return false;
    };
    let want = env.to_string_lossy();
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.is_file() && p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            match launch_cwd(&p) {
                Some(c) if c == *want => {}
                _ => return false, // a foreign (or unattributable) transcript shares this slug
            }
        }
    }
    true
}

/// Claude Code injects a batch of XML-style tags into the transcript (not real prompts). These are the known injected tag names.
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

/// Real user prompt: non-empty; if it starts with '<', it counts as real only when what follows is not a known injected tag.
/// The old logic bluntly dropped "anything starting with '<'", which also wrongly killed legitimate prompts like `<div> won't render`.
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
                "cannot find this project's Claude Code session directory: {}\n\
                 (the slug is derived from cwd. Change directory, or specify it explicitly with `agit -a import <session.jsonl>`.)",
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
            .with_context(|| format!("{} contains no .jsonl session", dir.display()))?;
        Ok(latest)
    }

    fn validate(&self, session: &Path) -> Result<()> {
        let text = std::fs::read_to_string(session)
            .with_context(|| format!("cannot read {}", session.display()))?;
        // The first few lines contain non-message records like mode / permission-mode / system, so we only require:
        // it is JSONL, and a record with a known type appeared early on.
        let mut saw_json = false;
        for line in text.lines().take(40) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                bail!("contains a non-JSON line; does not look like a Claude Code session (.jsonl)");
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
            Ok(()) // it is JSONL, just no message record in the first 40 lines
        } else {
            bail!("empty session")
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

/// Pure parsing: a chunk of Claude Code jsonl → SessionIR. Touches no disk, no IO.
/// **The only transcript-parsing implementation** -- both `export` (agit) and Hub rendering go through it, so the rules don't drift in two places
/// (prompt filtering, isCompactSummary exclusion, etc. are changed only once, here).
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

        // Environment hints: take the first record that carries cwd/gitBranch
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
        // compaction writes a synthetic user record (isCompactSummary=true) as a summary -- it is not
        // a real user prompt; treating it as one would mix the compaction summary into the brief and pollute the merged input. Exclude it.
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

// ─────────────────── ConversationIR: lossless read/write (used by convert) ───────────────────

use crate::convo::{narrate_call, truncate, ConversationIR, ConvertOpts, Event, EventKind};

/// schema version used when synthesizing claude records (verified against a real transcript: 2.1.207).
const CLAUDE_VERSION: &str = "2.1.207";

/// claude jsonl → ConversationIR (preserves every line verbatim + a semantic overlay).
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
    // The synthetic user record written by compaction (isCompactSummary=true) is not a real prompt. The same-vendor path
    // keeps it via raw; but when rebuilding across vendors from kinds, treating it as a UserPrompt would disguise the compaction
    // summary as a user question and inject it into the target session (parse_jsonl already excludes it on the SessionIR path;
    // this adds the same for the ConversationIR path).
    let is_compact = rec
        .get("isCompactSummary")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let content = rec.get("message").and_then(|m| m.get("content"));
    let mut out = vec![];
    match ty {
        "user" if !is_meta && !is_compact => match content {
            Some(serde_json::Value::String(s)) => {
                if is_real_prompt(s) {
                    out.push(EventKind::UserPrompt(s.trim().to_string()));
                }
            }
            Some(serde_json::Value::Array(arr)) => {
                // Multimodal prompt: content is a block array ([{text},{image},…]).
                // We must extract text blocks as the real prompt (otherwise a user question with an image would be dropped entirely), and also extract tool_result.
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
                        _ => {} // thinking: encrypted, dropped across vendors (kept by raw within the same vendor)
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

/// ConversationIR → claude jsonl. Same vendor uses raw replay (byte-level); cross vendor uses synthesis.
pub fn write_conversation(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    if ir.source_runtime == "claude-code" {
        same_vendor_claude(ir, opts)
    } else {
        cross_to_claude(ir, opts)
    }
}

/// Same vendor: replay raw line by line, only swapping the source session id for the new id (+ an optional cwd override), leaving all other bytes untouched.
/// Uses the quote-anchored swap_quoted to avoid substring/prefix collateral damage and empty-string blowups (see convo::swap_quoted).
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

/// Cross vendor (codex → claude): synthesize claude records from kinds, stringing together the parentUuid chain.
/// Tool activity is narrated as text (the target model only needs to know what happened; it does not replay foreign tool schemas).
fn cross_to_claude(ir: &ConversationIR, opts: &ConvertOpts) -> String {
    let cwd = opts.cwd.clone().or_else(|| ir.cwd.clone()).unwrap_or_default();
    let branch = ir.git_branch.clone().unwrap_or_default();
    let ts = "2026-01-01T00:00:00.000Z"; // synthetic timestamp; claude resume does not require monotonicity
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
    fn slug_collapses_non_alphanumeric() {
        // real: /home/user/bolusi/.claude/worktrees → -home-user-bolusi--claude-worktrees
        assert_eq!(
            slug_for(Path::new("/home/user/bolusi/.claude/worktrees")),
            "-home-user-bolusi--claude-worktrees"
        );
        // a normal path containing a dot must also collapse to '-', otherwise sync won't find the directory
        assert_eq!(slug_for(Path::new("/a/b.c/d")), "-a-b-c-d");
        // real bug: an underscore in the path is also collapsed by Claude Code, so agit must match.
        // /home/user/_test/payments → -home-user--test-payments (NOT -home-user-_test-payments)
        assert_eq!(
            slug_for(Path::new("/home/user/_test/payments")),
            "-home-user--test-payments"
        );
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
    fn compaction_summary_is_not_a_cross_vendor_prompt() {
        // a compaction summary is a synthetic user record; on cross-vendor rebuild it must NOT
        // resurface as a fake user prompt (same-vendor still keeps it verbatim via raw).
        let src = "{\"type\":\"user\",\"isCompactSummary\":true,\"sessionId\":\"S\",\"message\":{\"role\":\"user\",\"content\":\"This session is a summary of prior work: we refactored auth.\"}}\n\
                   {\"type\":\"user\",\"sessionId\":\"S\",\"message\":{\"role\":\"user\",\"content\":\"now add logout\"}}\n";
        let ir = read_conversation(src);
        let prompts: Vec<String> = ir
            .events
            .iter()
            .flat_map(|e| &e.kinds)
            .filter_map(|k| if let EventKind::UserPrompt(p) = k { Some(p.clone()) } else { None })
            .collect();
        assert_eq!(prompts, vec!["now add logout".to_string()], "compaction summary leaked as a prompt: {prompts:?}");
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
        assert!(is_real_prompt("<div> won't render, what do I do")); // legitimate prompt, should not be dropped
        assert!(is_real_prompt("<= means less than or equal"));
        assert!(!is_real_prompt("<system-reminder>...</system-reminder>"));
        assert!(!is_real_prompt("<local-command-caveat>x"));
        assert!(!is_real_prompt("   ")); // whitespace
    }
}
