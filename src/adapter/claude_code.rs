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

/// cwd → Claude Code 的 project slug：绝对路径里的 '/' **和 '.'** 都换成 '-'。
/// 核对真实目录:`/home/user/bolusi/.claude/worktrees` → `-home-user-bolusi--claude-worktrees`
/// (`.claude` 的点也塌成 '-',故双连字符)。只换 '/' 会对任何含点的路径算出错误 slug、找不到目录。
pub fn slug_for(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '.'], "-")
}

pub fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("读不到 $HOME")?;
    Ok(PathBuf::from(home).join(".claude/projects"))
}

/// Claude Code 会往转录里注入一批 XML 式标签（不是真实 prompt）。这些是已知的注入标签名。
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

/// 真实用户 prompt：非空；若以 '<' 开头，仅当其后不是已知注入标签时才算真实。
/// 旧逻辑一刀切"以 '<' 开头即丢"，把 `<div> 不渲染` 这类合法 prompt 也误杀了。
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
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(parse_jsonl(&text, &id))
    }
}

/// 纯解析:一坨 Claude Code jsonl → SessionIR。不碰磁盘,无 IO。
/// **唯一的转录解析实现** —— `export`(agit)和 Hub 渲染都走它,免得两处规则漂移
/// (prompt 过滤、isCompactSummary 排除等只在这里改一次)。
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
        // compaction 会写一条合成的 user 记录(isCompactSummary=true)当摘要 —— 它不是
        // 真实用户 prompt,当成 prompt 会把压缩摘要混进 brief、污染合并输入。排除之。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_collapses_dot_and_slash() {
        // 真实:/home/user/bolusi/.claude/worktrees → -home-user-bolusi--claude-worktrees
        assert_eq!(
            slug_for(Path::new("/home/user/bolusi/.claude/worktrees")),
            "-home-user-bolusi--claude-worktrees"
        );
        // 含点的普通路径也要塌成 '-',否则 sync 找不到目录
        assert_eq!(slug_for(Path::new("/a/b.c/d")), "-a-b-c-d");
    }

    #[test]
    fn real_prompt_keeps_angle_bracket_prose_drops_injected_tags() {
        assert!(is_real_prompt("fix the bug"));
        assert!(is_real_prompt("<div> 不渲染怎么办")); // 合法 prompt,不该丢
        assert!(is_real_prompt("<= 是小于等于"));
        assert!(!is_real_prompt("<system-reminder>...</system-reminder>"));
        assert!(!is_real_prompt("<local-command-caveat>x"));
        assert!(!is_real_prompt("   ")); // 空白
    }
}
