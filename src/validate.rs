//! `agit -a validate` —— 校验 Agent Store 的 AgentState 是否符合 agit/v1-draft。
//! `agit -a portable` —— 输出 PortableState。
//!
//! PRD：「对象缺失、版本冲突和 Adapter 不兼容必须显式报告；secret 不得进入 Hub。」

use crate::claim::{path_to_subject, Claim};
use crate::scan;
use crate::scope::{self, Scope};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const AGIT_VERSION: &str = "v1-draft";

pub fn validate() -> Result<i32> {
    let agent = scope::root_for(Scope::Agent)?;
    let mut errs: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();

    // ── Agent 身份 ──
    let toml = agent.join("agent.toml");
    match std::fs::read_to_string(&toml) {
        Ok(t) if t.lines().any(|l| l.trim_start().starts_with("id")) => {}
        Ok(_) => errs.push("agent.toml 缺 id 字段".into()),
        Err(_) => errs.push("缺 agent.toml".into()),
    }

    // ── AgentState 骨架 ──
    let state = agent.join("state");
    for f in ["goals.md", "constraints.md", "progress.md", "artifacts.md"] {
        if !state.join(f).exists() {
            warns.push(format!("缺 state/{f}（可选，建议补）"));
        }
    }

    // ── 每条 fact ──
    let facts = collect_facts(&agent);
    for (path, subject) in &facts {
        match Claim::load(path) {
            Ok(c) => {
                if c.meta.evidence.is_empty() {
                    errs.push(format!("fact `{subject}` 无证据"));
                }
                if c.meta.subject != *subject {
                    warns.push(format!(
                        "fact `{subject}` 的 frontmatter subject 与路径不一致（{}）",
                        c.meta.subject
                    ));
                }
            }
            Err(e) => errs.push(format!("fact `{subject}` 解析失败：{e}")),
        }
    }

    // ── _session.json 可解析 ──
    let sj = state.join("_session.json");
    if sj.exists() {
        if let Ok(t) = std::fs::read_to_string(&sj) {
            if serde_json::from_str::<serde_json::Value>(&t).is_err() {
                errs.push("_session.json 不是合法 JSON".into());
            }
        }
    }

    // ── secret 不得进入 ──
    let mut secret_hits = 0;
    for (path, subject) in &facts {
        if let Ok(fs) = scan::scan_file(path) {
            for f in fs {
                errs.push(format!("fact `{subject}`:{} 疑似密钥 [{}]", f.line, f.rule));
                secret_hits += 1;
            }
        }
    }

    // ── 报告 ──
    println!("agit/{AGIT_VERSION} 校验：{} 条 fact", facts.len());
    for w in &warns {
        println!("  warning: {w}");
    }
    if errs.is_empty() {
        println!("  ✓ AgentState 合法{}", if secret_hits == 0 { "，无密钥" } else { "" });
        Ok(0)
    } else {
        for e in &errs {
            eprintln!("  error: {e}");
        }
        eprintln!("\n{} 项错误。", errs.len());
        Ok(1)
    }
}

pub fn portable(out: Option<PathBuf>) -> Result<i32> {
    let agent = scope::root_for(Scope::Agent)?;

    let agent_spec_ref = std::fs::read(agent.join("agent.toml"))
        .map(|b| {
            let mut h = Sha256::new();
            h.update(&b);
            format!("sha256:{}", hex::encode(h.finalize()))
        })
        .unwrap_or_else(|_| "sha256:0".into());

    let agent_state_ref = scope::git_in_status(&agent, &["rev-parse", "HEAD"]).1;

    let ws_head = scope::workspace_dir()?.join("HEAD.json");
    let workspace_revision_ref = std::fs::read(&ws_head)
        .map(|b| {
            let mut h = Sha256::new();
            h.update(&b);
            format!("sha256:{}", &hex::encode(h.finalize())[..16])
        })
        .unwrap_or_else(|_| "none".into());

    let history_ref = std::fs::read_to_string(agent.join("state/_session.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("session_id").and_then(|s| s.as_str()).map(String::from));

    let portable = serde_json::json!({
        "agit_version": AGIT_VERSION,
        "agent_spec_ref": agent_spec_ref,
        "agent_state_ref": agent_state_ref,
        "workspace_revision_ref": workspace_revision_ref,
        "history_ref": history_ref,
    });
    let text = serde_json::to_string_pretty(&portable)?;

    match out {
        Some(p) => {
            std::fs::write(&p, &text)?;
            println!("PortableState → {}", p.display());
        }
        None => println!("{text}"),
    }
    Ok(0)
}

fn collect_facts(agent: &Path) -> Vec<(PathBuf, String)> {
    WalkDir::new(agent.join("state/facts"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let rel = e.path().strip_prefix(agent).ok()?.to_path_buf();
            let subject = path_to_subject(&rel)?;
            Some((e.path().to_path_buf(), subject))
        })
        .collect()
}
