//! Agent Store 上的 fact 领域动词：new / verify / why。
//!
//! 关键的双根：**fact 文件住在 Agent Store**（state/facts/<subject>.md），
//! 但证据的 `file:` 指针相对 **Environment（代码仓库）** 解析。
//! 所以每个操作都同时持有 agent_root 与 env_root。

use crate::claim::{path_to_subject, subject_to_path, Claim, Evidence, Meta, Tier};
use crate::evidence::{self, Status};
use crate::scan;
use crate::scope::{self, Scope};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn agent_root() -> Result<PathBuf> {
    scope::root_for(Scope::Agent)
}

fn all_facts(agent: &Path) -> Vec<(PathBuf, String)> {
    let facts = agent.join("state/facts");
    WalkDir::new(&facts)
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

// ─────────────────────────── new ───────────────────────────

pub fn new_fact(
    subject: &str,
    evidence: &[String],
    message: &str,
    tier: Option<Tier>,
    author: Option<String>,
) -> Result<i32> {
    let agent = agent_root()?;
    let env = scope::environment_root()?;
    let rel = subject_to_path(subject)?;
    let path = agent.join(&rel);

    if path.exists() {
        bail!("{} 已存在。编辑它，或先 agit -a rm。", rel.display());
    }

    // 证据相对 Environment 采集：读源、算摘要、denylist 拦截
    let mut captured = Vec::new();
    for raw in evidence {
        let ev: Evidence = raw.parse()?;
        let ev = evidence::capture(&env, ev).with_context(|| format!("证据无法采集: {raw}"))?;
        captured.push(ev);
    }
    if captured.is_empty() {
        bail!("fact 必须至少带一条证据（-e）。没有出处的结论不入库。");
    }

    let tier = tier.unwrap_or_else(|| {
        captured
            .iter()
            .map(|e| e.implied_tier())
            .max_by_key(|t| t.rank())
            .unwrap_or(Tier::Reversible)
    });
    let author = author
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let (_, name) = scope::git_in_status(&agent, &["config", "user.name"]);
            if name.is_empty() { "unknown".into() } else { name }
        });

    let claim = Claim {
        meta: Meta {
            subject: subject.to_string(),
            tier,
            author,
            created: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            evidence: captured,
        },
        body: message.trim().to_string(),
    };
    let text = claim.render()?;

    // 落盘前扫密钥（正文 + 证据行）
    let findings = scan::scan_text(&text);
    if !findings.is_empty() {
        eprintln!("拒绝写入：fact 里含有疑似密钥。");
        for f in findings {
            eprintln!("  行 {}  [{}]  {}", f.line, f.rule, f.excerpt);
        }
        return Ok(1);
    }

    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, text)?;

    println!("新建 fact  {}", rel.display());
    println!("  tier: {tier}");
    for e in &claim.meta.evidence {
        println!("  证据: {e}");
    }
    println!("\n  审阅后：agit -a add -A && agit -a commit -m '...'");
    Ok(0)
}

// ─────────────────────────── verify ───────────────────────────

pub fn verify(rerun: bool) -> Result<i32> {
    let agent = agent_root()?;
    let env = scope::environment_root()?;
    let facts = all_facts(&agent);

    if facts.is_empty() {
        println!("Agent Store 里还没有 fact。用 agit -a new，或 agit -a import 抽取。");
        return Ok(0);
    }

    let mut bad = 0;
    println!("{:<12} {:<36} {}", "状态", "subject", "证据");
    println!("{}", "─".repeat(96));

    for (path, subject) in &facts {
        let claim = match Claim::load(path) {
            Ok(c) => c,
            Err(e) => {
                println!("{:<12} {:<36} {}", "PARSE-ERR", subject, e);
                bad += 1;
                continue;
            }
        };
        let (status, verdicts) = evidence::claim_status(&env, &claim.meta.evidence, rerun);
        if matches!(status, Status::Stale | Status::Missing) {
            bad += 1;
        }
        for (i, (ev, v)) in claim.meta.evidence.iter().zip(verdicts.iter()).enumerate() {
            let (s, subj) = if i == 0 { (status.label(), subject.as_str()) } else { ("", "") };
            println!("{:<12} {:<36} [{}] {}", s, subj, v.status.label(), ev);
            if !v.detail.is_empty() {
                println!("{:<12} {:<36}   ↳ {}", "", "", v.detail);
            }
        }
    }

    println!("{}", "─".repeat(96));
    if bad > 0 {
        println!("{bad} / {} 条 fact 的证据已失效或不可达。", facts.len());
        println!("这些结论不该再被 agent 信任 —— agit -a why <subject> 看出处链。");
        return Ok(1);
    }
    println!("{} 条 fact，证据全部新鲜。", facts.len());
    Ok(0)
}

// ─────────────────────────── why ───────────────────────────

pub fn why(subject: &str) -> Result<i32> {
    let agent = agent_root()?;
    let env = scope::environment_root()?;
    let rel = subject_to_path(subject)?;
    let claim = Claim::load(&agent.join(&rel))?;

    println!("subject : {}", claim.meta.subject);
    println!("tier    : {}", claim.meta.tier);
    println!("作者    : {}", claim.meta.author);
    println!("创建于  : {}", claim.meta.created);
    println!("\n结论");
    for l in claim.body.lines() {
        println!("  {l}");
    }
    println!("\n出处链");
    let (_, verdicts) = evidence::claim_status(&env, &claim.meta.evidence, false);
    for (ev, v) in claim.meta.evidence.iter().zip(verdicts.iter()) {
        println!("  [{}] {}", v.status.label(), ev);
        println!("        {}", v.detail);
    }
    println!("\n提交历史（Agent Store）");
    let (_, log) = scope::git_in_status(
        &agent,
        &["log", "--oneline", "--no-decorate", "--", &rel.to_string_lossy()],
    );
    for l in log.lines() {
        println!("  {l}");
    }
    Ok(0)
}
