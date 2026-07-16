//! ConversationIR — a lossless intermediate representation of a conversation, purpose-built for **format conversion** (convert).
//!
//! Deliberately kept separate from SessionIR (a lossy summary used for reconcile's brief): convert must be able to rebuild a
//! conversation that the target runtime can resume, not lean on a bag of prompt/command strings.
//!
//! Core design: every Event carries both
//!   - `raw`: the **verbatim original jsonl line** (not a re-parsed Value — serde_json would scramble key order/formatting).
//!            Same-runtime conversion replays raw directly → byte-level restoration.
//!   - `kinds`: a semantic overlay, used only when synthesizing across runtimes.
//!
//! Fidelity tiers: same vendor = byte-level (replay raw); cross vendor = content-level (rebuild the visible turns from kinds,
//! drop the encrypted CoT, synthesize the missing system prompt, narrate tool activity as text). See
//! docs/plans/2026-07-15-claude-codex-conversion-design.md.

use crate::adapter::{claude_code, codex};
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::Path;

/// Lossless conversation representation.
#[derive(Debug, Default, Clone)]
pub struct ConversationIR {
    pub source_runtime: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// codex's base_instructions; claude has none (the transcript contains no system prompt).
    pub system_prompt: Option<String>,
    pub events: Vec<Event>,
}

/// One source record: verbatim raw + semantic overlay.
#[derive(Debug, Clone)]
pub struct Event {
    /// The verbatim original jsonl line (used for same-vendor byte-level replay).
    pub raw: String,
    /// Semantic items extracted from this record (0..n; empty when nothing is extractable, e.g. reasoning/meta).
    pub kinds: Vec<EventKind>,
    pub id: Option<String>,
    pub parent_id: Option<String>,
    pub timestamp: Option<String>,
}

/// Semantic items consumed by cross-vendor synthesis. Encrypted reasoning, meta, token_count, etc. produce **no** kind
/// (they live only in raw and are dropped when going cross-vendor).
#[derive(Debug, Clone)]
pub enum EventKind {
    UserPrompt(String),
    AssistantText(String),
    ToolCall { call_id: Option<String>, name: String, input: Value },
    ToolResult { call_id: Option<String>, output: String },
    FileEdit { paths: Vec<String> },
}

/// convert options.
#[derive(Debug, Clone)]
pub struct ConvertOpts {
    /// Override the target conversation's cwd (decides which project resume lands in). Defaults to the source cwd.
    pub cwd: Option<String>,
    /// The new session id assigned to the output (convert never reuses the source id).
    pub new_id: String,
}

impl Default for ConvertOpts {
    fn default() -> Self {
        ConvertOpts { cwd: None, new_id: String::new() }
    }
}

/// Normalize the runtime name (aligned with adapter::get's keys).
pub fn normalize_runtime(rt: &str) -> &'static str {
    match rt {
        "claude" | "cc" | "claude-code" => "claude-code",
        "codex" => "codex",
        _ => "",
    }
}

/// Read a source session's text → ConversationIR.
pub fn read_conversation(runtime: &str, text: &str) -> Result<ConversationIR> {
    match normalize_runtime(runtime) {
        "claude-code" => Ok(claude_code::read_conversation(text)),
        "codex" => Ok(codex::read_conversation(text)),
        _ => bail!("unknown source runtime `{runtime}` (supported: claude-code / codex)"),
    }
}

/// ConversationIR → the target runtime's session text.
pub fn write_conversation(runtime: &str, ir: &ConversationIR, opts: &ConvertOpts) -> Result<String> {
    match normalize_runtime(runtime) {
        "claude-code" => Ok(claude_code::write_conversation(ir, opts)),
        "codex" => Ok(codex::write_conversation(ir, opts)),
        _ => bail!("unknown target runtime `{runtime}` (supported: claude-code / codex)"),
    }
}

/// Read source → IR → write target. Returns (target session text, IR for the caller to grab meta).
pub fn convert(
    src_path: &Path,
    src_runtime: &str,
    target_runtime: &str,
    opts: &ConvertOpts,
) -> Result<(String, ConversationIR)> {
    let text = std::fs::read_to_string(src_path)
        .map_err(|e| anyhow::anyhow!("failed to read source session {}: {e}", src_path.display()))?;
    let ir = read_conversation(src_runtime, &text)?;
    if ir.events.is_empty() {
        bail!("source session is empty.");
    }
    // Cross-vendor relies on rebuilding the visible turns; a source with zero semantic items (e.g. a truncated rollout with only session_meta)
    // would synthesize a near-empty conversation that the target CLI refuses to resume. Catch it early and give a clear message.
    if is_cross_vendor(src_runtime, target_runtime) {
        let semantic: usize = ir.events.iter().map(|e| e.kinds.len()).sum();
        if semantic == 0 {
            bail!("the source has no visible turns to rebuild (prompt/reply/tool); cannot convert cross-vendor.");
        }
    }
    // Don't rewrite ir.cwd here — same-vendor replay needs the **original** cwd to do the string substitution.
    // The target cwd is decided by the writer from opts.cwd (defaulting to ir.cwd).
    let out = write_conversation(target_runtime, &ir, opts)?;
    Ok((out, ir))
}

/// Whether this is cross-vendor (source and target runtime differ).
pub fn is_cross_vendor(src: &str, target: &str) -> bool {
    normalize_runtime(src) != normalize_runtime(target)
}

/// Replace id / cwd during same-vendor replay: swap the **JSON-quote-wrapped** `"old"` in the raw line for `"new"`.
/// Quote-anchored → won't clobber other content that has old as a substring/prefix (e.g. /a/app matching /a/application),
/// returns unchanged when old is empty or old == new (avoids the char-by-char explosion of replace("", …)).
pub fn swap_quoted(raw: &str, old: &str, new: &str) -> String {
    if old.is_empty() || old == new {
        return raw.to_string();
    }
    raw.replace(&format!("\"{old}\""), &format!("\"{new}\""))
}

/// Truncate by char, appending a marker when too long (never cutting mid-byte, to avoid a UTF-8 panic).
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
        assert!(err.to_string().contains("rebuild"), "{err}");
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

/// Narrate a single tool call as one line of text (the default cross-vendor path: the target model only needs to know what happened, not replay it).
/// Recognizes tool names from both claude (Bash/Read/Write/Edit) and codex (shell/exec_command/…).
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

/// The hex of sha256(input). Used by the hub to store a token digest (never persisting the plaintext secret).
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

/// Generate a new, uuid-shaped session id (8-4-4-4-12). No uuid crate:
/// sha256(time nanos + pid + salt) taking 16 bytes, with the version nibble set to 7 (uuidv7 shape).
/// resume only requires the id to look like a uuid and be unique (verified by the spike), it doesn't validate a real v7 timestamp.
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
    // Shaped like xxxxxxxx-xxxx-7xxx-8xxx-xxxxxxxxxxxx
    format!(
        "{}-{}-7{}-8{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}
