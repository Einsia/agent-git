//! Runtime Adapter — parses a runtime's raw session into a runtime-neutral SessionIR.
//!
//! Under the session model, an adapter does exactly one deterministic thing: read the session file → SessionIR.
//! The only consumer is `reconcile`'s `brief()` (session.rs): it pulls the prompt / last few agent text blocks /
//! changed files out of the IR and compresses them into a compact summary fed to the merge LLM.
//!
//! (The old two-stage extraction "evidence candidate pool → Summarizer → fact" was removed along with the fact model; we no longer distill conclusions here.)

pub mod claude_code;
pub mod codex;

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// A single Read call (a file fragment the agent actually looked at).
#[derive(Debug, Clone)]
pub struct FileRead {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// A runtime-neutral, normalized session. Each adapter's export produces it first.
#[derive(Debug, Default)]
pub struct SessionIR {
    pub runtime: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// Real user prompts (command injection, caveats, and tool results already stripped out). Used by brief.
    pub prompts: Vec<String>,
    /// Files the agent read. Not consumed by brief today; kept for future summary enrichment.
    pub reads: Vec<FileRead>,
    /// Commands the agent ran. Not consumed by brief today; kept for future summary enrichment.
    pub commands: Vec<String>,
    /// Files the agent modified — listed in brief as "changed files".
    pub writes: Vec<String>,
    /// The assistant's text blocks — brief takes the last few as "conclusions/progress".
    pub agent_texts: Vec<String>,
    /// Total number of tool_use blocks (including uncategorized tools) — Hub renders "N tool calls".
    pub tool_uses: usize,
}

/// The three methods specified by the PRD. Both Codex and ClaudeCode implement it.
pub trait Adapter {
    fn name(&self) -> &'static str;

    /// runtime session → normalized IR. Deterministic, no model calls. reconcile's brief relies on it to read the conversation.
    /// When `session` is None, the adapter locates the current project's latest session itself.
    fn export(&self, session: Option<&Path>, cwd: &Path) -> Result<SessionIR>;

    /// Validate whether a session file is well-formed for this runtime.
    fn validate(&self, session: &Path) -> Result<()>;

    /// Locate the current project's default (latest) session. Adapters with no session concept may return an error.
    fn locate_default(&self, cwd: &Path) -> Result<PathBuf>;
}

/// Get an adapter by name. New runtimes register here.
pub fn get(runtime: &str) -> Result<Box<dyn Adapter>> {
    match runtime {
        "claude-code" | "claude" | "cc" => Ok(Box::new(claude_code::ClaudeCode)),
        "codex" => Ok(Box::new(codex::Codex)),
        other => bail!("unknown runtime `{other}`. Registered: claude-code, codex"),
    }
}

pub fn list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude-code", "Claude Code — parses ~/.claude/projects/<slug>/<session>.jsonl (implemented)"),
        ("codex", "Codex — parses ~/.codex/sessions/*/rollout-*.jsonl (filters projects by cwd) (implemented)"),
    ]
}
