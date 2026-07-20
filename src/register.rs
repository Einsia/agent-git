//! Write the synthesized session text into the target runtime's session store so it can be resumed by the native CLI.
//!
//! Spike verified (2026-07-15, see docs/plans): both CLIs **scan the directory + resolve by id**, so there's no index to maintain.
//!   - Claude: `~/.claude/projects/<slug>/<uuid>.jsonl`, `claude --resume <uuid>`. Verified end-to-end.
//!   - Codex : `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl`, `codex resume <uuid>` (interactive).
//!            The place/resolve mechanism works; "whether history is really loaded" awaits acceptance after `codex login`.

use crate::adapter::claude_code;
use crate::convo::normalize_runtime;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct ResumeHandle {
    pub path: PathBuf,
    pub resume_cmd: String,
}

/// Write to disk and return (path, resume command).
pub fn install(runtime: &str, id: &str, cwd: &Path, bytes: &str) -> Result<ResumeHandle> {
    match normalize_runtime(runtime) {
        "claude-code" => install_claude(id, cwd, bytes),
        "codex" => install_codex(id, cwd, bytes),
        _ => bail!("unknown target runtime `{runtime}`"),
    }
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("could not read $HOME")?))
}

fn install_claude(id: &str, cwd: &Path, bytes: &str) -> Result<ResumeHandle> {
    let slug = claude_code::slug_for(cwd);
    let dir = home()?.join(".claude/projects").join(slug);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, bytes)?;
    Ok(ResumeHandle {
        path,
        resume_cmd: format!("(cd {} && claude --resume {id})", cwd.display()),
    })
}

fn install_codex(id: &str, cwd: &Path, bytes: &str) -> Result<ResumeHandle> {
    // date-partitioned; the exact date doesn't matter -- resume recursively scans sessions/ and resolves by id.
    let dir = home()?.join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
    std::fs::write(&path, bytes)?;
    Ok(ResumeHandle {
        path,
        // INTERACTIVE resume: `codex resume <id>` (current codex CLI: `codex resume [SESSION_ID] [PROMPT]`,
        // prompt optional) opens the TUI carrying the session -- which is what `agit start`/`resume` want.
        // The old `codex exec resume <id>` is codex's NON-interactive one-shot mode and REQUIRES a prompt,
        // so a promptless `agit start --as codex` died with "No prompt provided". `codex resume` resolves
        // the same rollout files by id (the recursive sessions/ scan), so the on-disk install is unchanged;
        // only the launch verb differs. (Earlier note that `codex resume` "makes resume fail" was on an
        // older codex without the `resume` subcommand.)
        resume_cmd: format!("(cd {} && codex resume {id})", cwd.display()),
    })
}
