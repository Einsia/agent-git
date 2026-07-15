//! 把合成的 session 文本落进目标 runtime 的 session store,使其可被原生 CLI resume。
//!
//! spike 已核实(2026-07-15,见 docs/plans):两个 CLI 都**扫目录 + 按 id 解析**,无需维护索引。
//!   - Claude:`~/.claude/projects/<slug>/<uuid>.jsonl`,`claude --resume <uuid>`。已端到端验证。
//!   - Codex :`~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl`,`codex exec resume <uuid>`。
//!            放置/解析机制成立;"history 是否真载入"待 `codex login` 后验收。

use crate::adapter::claude_code;
use crate::convo::normalize_runtime;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct ResumeHandle {
    pub path: PathBuf,
    pub resume_cmd: String,
}

/// 落盘并返回 (路径, resume 命令)。
pub fn install(runtime: &str, id: &str, cwd: &Path, bytes: &str) -> Result<ResumeHandle> {
    match normalize_runtime(runtime) {
        "claude-code" => install_claude(id, cwd, bytes),
        "codex" => install_codex(id, cwd, bytes),
        _ => bail!("未知目标 runtime `{runtime}`"),
    }
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("读不到 $HOME")?))
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
    // date-partitioned;具体日期无所谓 —— resume 递归扫 sessions/ 并按 id 解析。
    let dir = home()?.join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));
    std::fs::write(&path, bytes)?;
    Ok(ResumeHandle {
        path,
        resume_cmd: format!("(cd {} && codex resume {id})", cwd.display()),
    })
}
