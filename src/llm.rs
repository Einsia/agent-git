//! Pluggable LLM CLI backend -- every place in agit that "needs a model" routes through here.
//!
//! The only consumer right now is the semantic merge in `reconcile` (session.rs): it reads both sessions' briefs,
//! synthesizes a unified context, and judges real conflicts. Storage/sync/file-level merging is all deterministic git; only this layer calls a model.
//!
//! Backend selection (the hook left for Codex):
//!   1. `AGIT_LLM_CMD="<any command>"`  -- works immediately; the command reads the prompt from stdin and writes the result to stdout.
//!                                      e.g. export AGIT_LLM_CMD="codex exec -"
//!   2. `AGIT_LLM=claude` (default)    -- local `claude -p`
//!   3. `AGIT_LLM=codex`            -- reserved: fill in here once we have codex's non-interactive invocation

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

enum Backend {
    Claude,
    Codex,
    /// AGIT_LLM_CMD: the whole command, run via `sh -c`, with the prompt fed through stdin.
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
        other => Backend::Cmd(other.to_string()), // treat as a command name, e.g. "ollama run llama3"
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

/// Whether the current backend is actually available (used to decide "align semantically if a model exists, otherwise fall back to deterministic merge").
pub fn available() -> bool {
    match backend() {
        Backend::Claude => which("claude"),
        Backend::Codex => which("codex"),
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

/// Feed the prompt to the backend and return its text reply.
pub fn ask(prompt: &str) -> Result<String> {
    // codex exec streams a bunch of reasoning/events to stdout; `-o <file>` writes only the final reply.
    // Handle it separately to get clean text (otherwise the trailing ```json block that reconcile parses gets polluted by the chrome).
    if let Backend::Codex = backend() {
        return ask_codex(prompt);
    }
    let (program, args): (&str, Vec<String>) = match backend() {
        Backend::Claude => ("claude", vec!["-p".into()]),
        Backend::Codex => unreachable!("codex is handled above"),
        Backend::Cmd(c) => ("sh", vec!["-c".into(), c]),
    };

    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start LLM backend: {program} (is it on PATH?)"))?;
    child
        .stdin
        .take()
        .context("could not get the backend's stdin")?
        .write_all(prompt.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("LLM backend returned non-zero");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `codex exec` backend: the prompt goes through stdin, and the final reply is written to a temp file via `-o <file>` and read back.
/// `--skip-git-repo-check` lets it run in any directory; a read-only sandbox is fine (pure text synthesis, it doesn't need to touch files).
fn ask_codex(prompt: &str) -> Result<String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out_file = std::env::temp_dir().join(format!("agit-codex-{}-{nanos}.txt", std::process::id()));

    let mut child = Command::new("codex")
        .args(["exec", "--skip-git-repo-check", "--color", "never"])
        .arg("-o")
        .arg(&out_file)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start codex (is it on PATH? install @openai/codex)")?;
    child
        .stdin
        .take()
        .context("could not get codex's stdin")?
        .write_all(prompt.as_bytes())?;
    let status = child.wait()?;

    let reply = std::fs::read_to_string(&out_file).ok();
    let _ = std::fs::remove_file(&out_file);
    if !status.success() {
        bail!("codex exec returned non-zero");
    }
    reply.filter(|s| !s.trim().is_empty()).context("codex exec produced no reply")
}
