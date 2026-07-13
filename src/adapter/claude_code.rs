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
        // AgentGit → Claude Code：产出一段 CLAUDE.md 风格的 context。
        // Claude Code 每个 session 自动加载项目根的 CLAUDE.md，所以这就是「复用」的落地：
        // 下一个会话一开工就带着这些目标、带出处的结论、决定。
        let md = render_claude_context(state_dir)?;
        std::fs::write(out, md)?;
        Ok(())
    }
}

/// 把 Agent Store 的 state/ 渲染成给 Claude Code 读的 context markdown。
pub fn render_claude_context(state_dir: &Path) -> Result<String> {
    let mut md = String::new();
    md.push_str(
        "# 继承的 Agent Context（由 agit 从既往会话提炼）\n\n\
         > 这是本仓库此前 agent 工作积累的上下文。下面每条「已知事实」都带证据出处，\n\
         > 你可以直接信任，但代码可能已经变化 —— 需要核实时运行 `agit -a verify`。\n\n",
    );

    let read = |f: &str| std::fs::read_to_string(state_dir.join(f)).unwrap_or_default();

    let goals = read("goals.md");
    if !goals.trim().is_empty() {
        md.push_str(&format!("## 目标\n\n{}\n\n", goals.trim_start_matches("# 目标").trim()));
    }
    let cons = read("constraints.md");
    if cons.trim().len() > "# 约束".len() {
        md.push_str(&format!("## 约束\n\n{}\n\n", cons.trim_start_matches("# 约束").trim()));
    }

    // 已知事实（带证据）
    let facts_dir = state_dir.join("facts");
    let mut facts: Vec<(String, String)> = vec![];
    if facts_dir.exists() {
        collect_md(&facts_dir, &facts_dir, &mut facts);
    }
    facts.sort_by(|a, b| a.0.cmp(&b.0));
    if !facts.is_empty() {
        md.push_str("## 已知事实（带证据）\n\n");
        for (subject, body) in &facts {
            let (concl, evs) = split_fact(body);
            md.push_str(&format!("- **{subject}** — {concl}\n"));
            for e in evs {
                md.push_str(&format!("  - 依据 `{e}`\n"));
            }
        }
        md.push('\n');
    }

    let prog = read("progress.md");
    let prog_body = prog.trim_start_matches("# 进度").trim();
    if !prog_body.is_empty() && !prog_body.starts_with("_（抽取") {
        md.push_str(&format!("## 进度\n\n{prog_body}\n\n"));
    }

    md.push_str(
        "---\n_复用命令：`agit clone <hub-url>/<name>.git` 拉取，`agit -a verify` 核实新鲜度。_\n",
    );
    Ok(md)
}

fn collect_md(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_md(base, &p, out);
        } else if p.extension().map(|x| x == "md").unwrap_or(false)
            && !p.file_name().map(|f| f.to_string_lossy().starts_with('.')).unwrap_or(false)
        {
            if let Ok(body) = std::fs::read_to_string(&p) {
                let subject = p
                    .strip_prefix(base)
                    .ok()
                    .and_then(|r| r.to_str())
                    .map(|s| s.trim_end_matches(".md").to_string())
                    .unwrap_or_default();
                out.push((subject, body));
            }
        }
    }
}

/// 从一条 fact 文件里拆出 (结论正文, 证据 locator 列表)。
fn split_fact(text: &str) -> (String, Vec<String>) {
    let mut evs = vec![];
    let mut body = text.trim().to_string();
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some((fm, b)) = rest.split_once("\n---\n") {
            body = b.trim().lines().next().unwrap_or("").to_string();
            let mut in_ev = false;
            for line in fm.lines() {
                if line.starts_with("evidence:") {
                    in_ev = true;
                } else if in_ev {
                    let t = line.trim();
                    if let Some(it) = t.strip_prefix("- ") {
                        evs.push(it.trim().trim_matches('\'').to_string());
                    } else if !t.is_empty() {
                        in_ev = false;
                    }
                }
            }
        }
    }
    (body, evs)
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
