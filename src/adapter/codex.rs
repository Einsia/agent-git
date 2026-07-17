//! Codex adapter — parses `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Real structure (verified against 838 rollouts on this machine, 2025–2026): each line is `{timestamp, type, payload}`.
//!   type=session_meta   payload:{id, cwd, git:{branch}}   — project ownership is hidden here
//!   type=event_msg      payload.type=user_message  → real user prompt (may be wrapped in <task>…</task>)
//!                       payload.type=agent_message → assistant's final text
//!                       payload.type=patch_apply_end → keys of payload.changes are the modified files
//!   type=response_item  payload.type=function_call:
//!                         name=shell        args {"command":["bash","-lc","<script>"]}   (2025)
//!                         name=shell_command args {"command":"<cmd>"}                     (string)
//!                         name=exec_command  args {"cmd":"<cmd>"}                          (mainstream)
//!                         name=reasoning     encrypted CoT, no plaintext, skip
//!
//! Difference from Claude: Claude splits directories by project slug; Codex splits by **date**, mixing all projects together,
//! and fork/resume embeds the parent session of **another project** into the same rollout. So project ownership must
//! scan **all** session_meta — if any foreign-project cwd appears, skip the whole file (see id_if_owned, the privacy backstop).

use super::{Adapter, SessionIR};
use anyhow::{bail, Context, Result};
use std::io::BufRead;
use std::path::{Path, PathBuf};

pub struct Codex;

/// The ~/.codex/sessions root.
pub fn sessions_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("cannot read $HOME")?;
    Ok(PathBuf::from(home).join(".codex/sessions"))
}

/// Recursively list all rollout-*.jsonl.
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

/// Whether a rollout **fully belongs** to the target project → Some(session_id), otherwise None.
///
/// Privacy backstop: a fork/resume rollout embeds the parent session of another project (whose session_meta.cwd differs).
/// As soon as any cwd != target is found, return None and skip the whole file — never copy another project's session in.
/// Read line by line (BufRead, bounded memory); short-circuit as soon as a foreign-project cwd is seen; files not from this project usually return on the first line.
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
            Some(_) => return None, // a foreign-project session in the same rollout → skip the whole file
            None => {}              // session_meta has no cwd, ignore this one (neither a match nor a foreign project)
        }
    }
    if matched {
        id
    } else {
        None
    }
}

/// The session uuid in the rollout filename (the trailing UUID), used as a fallback id.
fn file_id(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // rollout-<ISO8601>-<uuid>: the uuid is the last 5 segments (8-4-4-4-12)
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        parts[parts.len() - 5..].join("-")
    } else {
        stem
    }
}

/// Rollouts belonging to a project (all session_meta.cwd == that project root): returns (source file, target session_id).
/// Used by session.rs's codex sync; mirrors only sessions that **fully belong** to this project.
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

    fn project_sessions(&self, env: &Path) -> Vec<(PathBuf, String)> {
        project_rollouts(env)
    }

    fn source_desc(&self, env: &Path) -> String {
        let owned = project_rollouts(env);
        let root = sessions_root().map(|r| r.display().to_string()).unwrap_or_default();
        format!("{root} (cwd={} matched {} rollouts)", env.display(), owned.len())
    }

    fn watch_dir(&self, _env: &Path) -> Option<PathBuf> {
        sessions_root().ok()
    }

    fn parse(&self, text: &str, fallback_id: &str) -> SessionIR {
        parse_rollout(text, fallback_id)
    }

    fn locate_default(&self, cwd: &Path) -> Result<PathBuf> {
        let root = sessions_root()?;
        if !root.exists() {
            bail!("Codex session directory not found: {} (has Codex ever run on this machine?)", root.display());
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
            format!("no Codex rollout under {} with cwd={} (has this project ever run in Codex?)", root.display(), want)
        })
    }

    fn validate(&self, session: &Path) -> Result<()> {
        let f = std::fs::File::open(session)
            .with_context(|| format!("cannot read {}", session.display()))?;
        let reader = std::io::BufReader::new(f);
        let mut saw_json = false;
        for line in reader.lines().take(40) {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                bail!("contains a non-JSON line; does not look like a Codex rollout (.jsonl)");
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
            bail!("empty rollout")
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

/// Pure parsing: a blob of Codex rollout jsonl → SessionIR. Does not touch disk.
/// **The only Codex parsing implementation** (symmetric with claude_code::parse_jsonl).
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
                        // Modern Codex records file changes here: keys of payload.changes are paths.
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
            "response_item"
                if p.and_then(|p| p.get("type")).and_then(|v| v.as_str()) == Some("function_call") => {
                    ir.tool_uses += 1;
                    let name = p.and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(name, "shell" | "local_shell" | "shell_command" | "exec_command") {
                        let args = p.and_then(|p| p.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(cmd) = extract_command(args) {
                            patch_files(&cmd, &mut ir.writes); // fallback for old-style apply_patch heredoc
                            ir.commands.push(cmd);
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

/// event_msg/user_message → real prompt. Unwraps the `<task>…</task>` wrapper (harnesses like StarryOS send it this way),
/// only drops known injected tags (environment_context / heartbeat, etc.), no longer blanket-dropping "anything starting with '<'".
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

/// The arguments (JSON string) of shell / shell_command / exec_command → the actual command.
/// Two key names: exec_command uses "cmd" (string); shell/shell_command uses "command" (array or string).
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

/// Old-style: an `apply_patch` heredoc embedded in a shell script → extract the modified files (best-effort).
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

/// When patch_apply_end has no structured changes, fall back to parsing "A/M/D <path>" lines from stdout.
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

// ─────────────────── ConversationIR: lossless read/write (used by convert) ───────────────────

use crate::convo::{narrate_call, truncate, ConversationIR, ConvertOpts, Event, EventKind};

/// codex rollout → ConversationIR (preserves every line verbatim + semantic overlay).
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
                "response_item"
                    if p.and_then(|p| p.get("type")).and_then(|v| v.as_str()) == Some("function_call") => {
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
                _ => {}
            }
        }
        ir.events.push(Event { raw: line.to_string(), kinds, id: None, parent_id: None, timestamp: ts });
    }
    ir
}

/// ConversationIR → codex rollout. Same vendor uses raw replay; cross-vendor uses synthesis.
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
        // Quote-anchored replacement, to avoid substring/prefix collateral damage and empty-string blowups (see convo::swap_quoted).
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

const CTS: &str = "2026-01-01T00:00:00.000Z"; // synthetic timestamp

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

/// Cross-vendor (claude → codex): synthesize a rollout. response_item is the channel codex reads on replay,
/// event_msg is the UI channel — emit both, matching the real rollout structure. Tool activity is narrated as text by default.
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

    // Modern format: <task> prompt, exec_command (cmd key), shell_command (command string),
    // patch_apply_end (changes), plus a non-command tool (write_stdin) that only counts toward tool_uses.
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
