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

    /// This runtime's sessions owned by the project at `env`: `(transcript path, session id)`. The
    /// single answer to "which of this project's sessions are this runtime's", used by capture and by
    /// "has this project run here yet".
    fn project_sessions(&self, env: &Path) -> Vec<(PathBuf, String)>;

    /// A human description of where those sessions come from, for snap's output. Named per runtime
    /// because the on-disk layouts differ (claude splits by project slug, codex by date).
    fn source_desc(&self, env: &Path) -> String;

    /// The directory the watcher polls for this runtime's new sessions (`None` if not applicable). No
    /// existence check — the watcher waits for it to appear.
    fn watch_dir(&self, env: &Path) -> Option<PathBuf>;

    /// Parse this runtime's raw transcript text into the IR (the hub renders session digests from it).
    fn parse(&self, text: &str, fallback_id: &str) -> SessionIR;
}

/// One registered runtime. This is the single place a runtime is named: its canonical name, a
/// one-line description, the aliases `get` also accepts, and how to construct its adapter. Adding a
/// runtime is one entry here plus the adapter impl — nothing else in the tree names a runtime.
struct Registered {
    name: &'static str,
    desc: &'static str,
    aliases: &'static [&'static str],
    make: fn() -> Box<dyn Adapter>,
}

const REGISTRY: &[Registered] = &[
    Registered {
        name: "claude-code",
        desc: "Claude Code — parses ~/.claude/projects/<slug>/<session>.jsonl (implemented)",
        aliases: &["claude", "cc"],
        make: || Box::new(claude_code::ClaudeCode),
    },
    Registered {
        name: "codex",
        desc: "Codex — parses ~/.codex/sessions/*/rollout-*.jsonl (filters projects by cwd) (implemented)",
        aliases: &[],
        make: || Box::new(codex::Codex),
    },
];

/// The canonical runtime names, in registry order. The one source every other module iterates instead
/// of naming a runtime; `session::runtimes()` re-exports it for convenience.
pub fn names() -> Vec<&'static str> {
    REGISTRY.iter().map(|r| r.name).collect()
}

/// Get an adapter by canonical name or alias.
pub fn get(runtime: &str) -> Result<Box<dyn Adapter>> {
    match REGISTRY.iter().find(|r| r.name == runtime || r.aliases.contains(&runtime)) {
        Some(r) => Ok((r.make)()),
        None => bail!("unknown runtime `{runtime}`. Registered: {}", names().join(", ")),
    }
}

pub fn list() -> Vec<(&'static str, &'static str)> {
    REGISTRY.iter().map(|r| (r.name, r.desc)).collect()
}

/// Canonicalize a runtime name or alias to its registered canonical name (`"cc"` → `"claude-code"`),
/// or `None` if it names no registered runtime. The single place the alias map lives — modules that
/// used to keep a private copy of the `"claude"|"cc"|"claude-code" => "claude-code"` match now route
/// through this.
pub fn normalize(name: &str) -> Option<&'static str> {
    REGISTRY.iter().find(|r| r.name == name || r.aliases.contains(&name)).map(|r| r.name)
}
