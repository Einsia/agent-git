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

/// cwd → Claude Code 的 project slug：绝对路径里的 '/' 换成 '-'。
fn slug_for(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("读不到 $HOME")?;
    Ok(PathBuf::from(home).join(".claude/projects"))
}

/// 真实用户 prompt：字符串、非空、不以 '<' 开头（排除 caveat / 命令注入 / 工具标签）。
fn is_real_prompt(s: &str) -> bool {
    let t = s.trim_start();
    !t.is_empty() && !t.starts_with('<')
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

        let mut ir = SessionIR {
            runtime: "claude-code".into(),
            session_id: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
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
            let content = rec.get("message").and_then(|m| m.get("content"));

            match ty {
                "user" if !is_meta => {
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
                                Some("tool_use") => collect_tool(b, &mut ir),
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
        Ok(ir)
    }

    fn import(&self, state_dir: &Path, out: &Path) -> Result<()> {
        // MVP：把 AgentState 汇成一份 runtime-neutral 的 markdown digest。
        // Claude Code 可以把它作为一条 context 消息读入 —— 一条命令复用他人的 context。
        let mut md = String::from("# 导入的 Agent Context\n\n");
        for (title, file) in [
            ("目标", "goals.md"),
            ("约束", "constraints.md"),
            ("进度", "progress.md"),
            ("Artifact", "artifacts.md"),
        ] {
            let p = state_dir.join(file);
            if let Ok(body) = std::fs::read_to_string(&p) {
                md.push_str(&format!("## {title}\n\n{}\n\n", body.trim()));
            }
        }
        let facts = state_dir.join("facts");
        if facts.exists() {
            md.push_str("## 已知事实（带证据）\n\n");
            let mut entries: Vec<_> = std::fs::read_dir(&facts)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                .collect();
            entries.sort_by_key(|e| e.path());
            for e in entries {
                if let Ok(body) = std::fs::read_to_string(e.path()) {
                    md.push_str(&body);
                    md.push_str("\n\n");
                }
            }
        }
        std::fs::write(out, md)?;
        Ok(())
    }
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
