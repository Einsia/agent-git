//! Summarizer：证据池 → 结论（fact）。抽取里唯一需要模型的一步。
//!
//! 安全约束是全部要点：模型**只能**引用证据池里 agent 真读过的文件 / 真跑过的命令。
//! 任何指向池外的 locator 一律丢弃 —— 「模型编造出处」因此在构造上不可能。
//!
//! 走本机 `claude -p`（stdin 喂 prompt），让它输出一个 ```json 数组，解析后逐条校验。

use crate::adapter::SessionIR;
use crate::claim::{validate_subject, Claim, Evidence, Meta, Tier};
use crate::evidence;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct DraftFact {
    subject: String,
    body: String,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    tier: Option<String>,
}

/// 相对 env_root 的文件路径（与 extract 的口径一致）。
fn rel(path: &str, env_root: &Path) -> Option<String> {
    let p = Path::new(path);
    p.strip_prefix(env_root)
        .ok()
        .map(|r| r.to_string_lossy().into_owned())
        .or_else(|| p.is_relative().then(|| path.to_string()))
}

/// 证据候选池：agent 真读过的文件 + 真跑过的命令。
struct Pool {
    files: HashSet<String>,
    cmds: Vec<String>,
}

fn build_pool(ir: &SessionIR, env_root: &Path) -> Pool {
    let mut files = HashSet::new();
    for r in &ir.reads {
        if let Some(rp) = rel(&r.path, env_root) {
            files.insert(rp);
        }
    }
    let cmds = ir
        .commands
        .iter()
        .map(|c| c.lines().next().unwrap_or("").trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    Pool { files, cmds }
}

/// 一条模型给的证据是否在池内。file: 校验路径∈池；cmd: 校验文本匹配池中命令。
/// doc:/human: 不接受 —— 抽取出的结论必须落在 agent 这次真看过的东西上。
fn evidence_in_pool(loc: &str, pool: &Pool) -> bool {
    if let Some(rest) = loc.strip_prefix("file:") {
        let path = rest.rsplit_once(':').map(|(p, _)| p).unwrap_or(rest);
        pool.files.contains(path)
    } else if let Some(cmd) = loc.strip_prefix("cmd:") {
        let cmd = cmd.split(" #").next().unwrap_or(cmd).trim();
        pool.cmds.iter().any(|c| c == cmd || c.starts_with(cmd) || cmd.starts_with(c.as_str()))
    } else {
        false
    }
}

fn build_prompt(ir: &SessionIR, pool: &Pool) -> String {
    let mut p = String::new();
    p.push_str(
        "你是一个从 AI agent 工作 session 中提炼「事实性结论」的助手。\n\
         把 agent 这个 session 得出的关键事实提炼成一个 JSON 数组。\n\n\
         严格规则：\n\
         1. 每条结论的 evidence 必须逐字引用下面「证据池」里的 locator，不许编造。\n\
         2. subject 用 kebab-case 的语义路径，如 api/user/id-field-name。\n\
         3. body 是一句话结论，用 session 的语言。\n\
         4. 只写确定的事实；拿不准的不要写。宁缺毋滥。\n\
         5. 只输出一个 ```json 代码块，元素形如：\n\
         {\"subject\":\"...\",\"body\":\"...\",\"evidence\":[\"file:path:12\",\"cmd:...\"],\"tier\":\"reversible|compensable|irreversible\"}\n\n",
    );

    p.push_str("## 目标（用户 prompt）\n");
    for g in ir.prompts.iter().take(15) {
        p.push_str(&format!("- {}\n", g.lines().next().unwrap_or("").trim()));
    }

    p.push_str("\n## 证据池 —— evidence 只能从这里选\n### 文件（写成 file:<路径>:<行号>）\n");
    for f in &pool.files {
        p.push_str(&format!("- {f}\n"));
    }
    p.push_str("### 命令（写成 cmd:<命令>）\n");
    for c in pool.cmds.iter().take(40) {
        p.push_str(&format!("- {c}\n"));
    }

    p.push_str("\n## agent 的关键文本（仅供理解，不能当 locator）\n");
    for t in ir.agent_texts.iter().rev().take(8).rev() {
        let one: String = t.trim().chars().take(300).collect();
        if !one.is_empty() {
            p.push_str(&format!("- {one}\n"));
        }
    }
    p
}

/// 从模型输出里抠出 JSON 数组（优先 ```json 块，否则第一个 [ 到最后一个 ]）。
fn extract_json(text: &str) -> Option<String> {
    if let Some(i) = text.find("```json") {
        let rest = &text[i + 7..];
        if let Some(j) = rest.find("```") {
            return Some(rest[..j].trim().to_string());
        }
    }
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    (end > start).then(|| text[start..=end].to_string())
}

/// 解析 + 校验（纯逻辑，可单测，不调模型）。返回通过校验的 (Claim) 列表。
pub fn validate_drafts(json: &str, pool_files: &HashSet<String>, pool_cmds: &[String], env_root: &Path, author: &str) -> Vec<Claim> {
    let pool = Pool {
        files: pool_files.clone(),
        cmds: pool_cmds.to_vec(),
    };
    let drafts: Vec<DraftFact> = serde_json::from_str(json).unwrap_or_default();
    let mut out = Vec::new();

    for d in drafts {
        if validate_subject(&d.subject).is_err() {
            continue;
        }
        // 只保留池内证据，并当场对齐基线
        let mut evs: Vec<Evidence> = Vec::new();
        for loc in &d.evidence {
            if !evidence_in_pool(loc, &pool) {
                continue; // 丢弃编造 / 池外的 locator
            }
            if let Ok(ev) = loc.parse::<Evidence>() {
                let ev = evidence::capture(env_root, ev.clone()).unwrap_or(ev);
                evs.push(ev);
            }
        }
        if evs.is_empty() {
            continue; // 没有可信证据的结论不入库
        }
        let tier = d
            .tier
            .as_deref()
            .and_then(|t| match t {
                "reversible" => Some(Tier::Reversible),
                "compensable" => Some(Tier::Compensable),
                "irreversible" => Some(Tier::Irreversible),
                _ => None,
            })
            .unwrap_or_else(|| {
                evs.iter().map(|e| e.implied_tier()).max_by_key(|t| t.rank()).unwrap_or(Tier::Reversible)
            });

        out.push(Claim {
            meta: Meta {
                subject: d.subject,
                tier,
                author: author.to_string(),
                created: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                evidence: evs,
            },
            body: d.body.trim().to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 安全性质：模型引用池外 / 编造的 locator，一律丢弃。
    #[test]
    fn fabricated_evidence_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.rs"), "fn main() {}\n").unwrap();

        let mut pool_files = HashSet::new();
        pool_files.insert("real.rs".to_string());
        let pool_cmds = vec!["ls -la".to_string()];

        let json = r#"[
          {"subject":"good/one","body":"真实结论","evidence":["file:real.rs:1","cmd:ls -la"]},
          {"subject":"bad/fabricated","body":"编造出处","evidence":["file:secret/creds.env:1"]},
          {"subject":"bad/no-evidence","body":"无证据","evidence":[]},
          {"subject":"../escape","body":"非法 subject","evidence":["file:real.rs:1"]}
        ]"#;

        let claims = validate_drafts(json, &pool_files, &pool_cmds, dir.path(), "t");
        // 只有 good/one 存活：池内证据、合法 subject、有证据
        assert_eq!(claims.len(), 1, "只应保留 1 条: {:?}", claims.iter().map(|c| &c.meta.subject).collect::<Vec<_>>());
        assert_eq!(claims[0].meta.subject, "good/one");
        // 池内的 file 证据被对齐、带上了摘要
        assert!(claims[0].meta.evidence.iter().any(|e| e.digest().is_some()));
    }

    #[test]
    fn extract_json_handles_fenced_and_bare() {
        assert_eq!(extract_json("prefix ```json\n[1,2]\n``` suffix").as_deref(), Some("[1,2]"));
        assert_eq!(extract_json("noise [\"a\"] more").as_deref(), Some("[\"a\"]"));
        assert_eq!(extract_json("no array here"), None);
    }
}

/// 端到端：session IR → 调 claude → 写 fact 文件。返回写入条数。
pub fn run(ir: &SessionIR, env_root: &Path, facts_dir: &Path) -> Result<usize> {
    let pool = build_pool(ir, env_root);
    if pool.files.is_empty() && pool.cmds.is_empty() {
        bail!("证据池为空，没有可提炼的素材。");
    }
    let prompt = build_prompt(ir, &pool);
    eprintln!("调用本机 claude 归纳结论……（证据池 {} 文件 / {} 命令）", pool.files.len(), pool.cmds.len());
    let reply = crate::llm::ask(&prompt)?;
    let json = extract_json(&reply).context("模型没有输出可解析的 JSON 数组")?;

    let claims = validate_drafts(&json, &pool.files, &pool.cmds, env_root, "claude-summarizer");
    let mut written = 0;
    for c in &claims {
        validate_subject(&c.meta.subject)?; // 已在 validate_drafts 过滤，双保险
        let path = facts_dir.join(format!("{}.md", c.meta.subject));
        if path.exists() {
            continue; // 不覆盖已有 fact
        }
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, c.render()?)?;
        written += 1;
    }
    Ok(written)
}
