//! 密钥扫描。
//!
//! 关键认知：**这道防线之所以必须存在，恰恰是因为 context 会被 push 和 clone。**
//! Shepherd / Zed / Claude Code 都没有它，不是疏忽 —— 是它们不分享 context。
//!
//! 扫描范围必须同时覆盖 claim 正文**和证据快照**，因为证据会把源文件内容抄进来。

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::Path;

pub struct Finding {
    pub rule: &'static str,
    pub line: usize,
    pub excerpt: String,
}

struct Rule {
    name: &'static str,
    re: Regex,
}

static RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    let r = |name, pat: &str| Rule {
        name,
        re: Regex::new(pat).expect("内置规则的正则必须可编译"),
    };
    vec![
        r("aws-access-key-id", r"\bAKIA[0-9A-Z]{16}\b"),
        r("github-pat", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
        r("github-fine-grained-pat", r"\bgithub_pat_[A-Za-z0-9_]{22,}\b"),
        r("slack-token", r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"),
        r("openai-key", r"\bsk-[A-Za-z0-9_-]{20,}\b"),
        r("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{20,}\b"),
        r("private-key-block", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        r("jwt", r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b"),
        r(
            "assigned-secret",
            r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key)\b\s*[:=]\s*["']?([^\s"'#]{6,})"#,
        ),
        r(
            "connection-string",
            r"(?i)\b(postgres|postgresql|mysql|mongodb|redis|amqp)://[^\s:@/]+:[^\s:@/]+@",
        ),
    ]
});

/// Shannon 熵。用来抓那些不匹配任何已知格式、但明显是随机密钥的长串。
fn shannon_entropy(s: &str) -> f64 {
    let n = s.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            p * p.log2()
        })
        .sum::<f64>()
}

static HIGH_ENTROPY_CANDIDATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{32,}").unwrap());

const ENTROPY_THRESHOLD: f64 = 4.2;

fn redact(s: &str) -> String {
    let s: String = s.chars().take(48).collect();
    if s.len() <= 10 {
        return "*".repeat(s.len());
    }
    format!("{}…{}", &s[..4], "*".repeat(6))
}

pub fn scan_text(text: &str) -> Vec<Finding> {
    let mut out = Vec::new();

    for (i, line) in text.lines().enumerate() {
        // 忽略我们自己写的 locator 行：`evidence: file:models/user.ts:4 #a937b4a5`
        // 摘要是 8 位十六进制，不会触发规则，但连接串形态的 locator 可能误报。
        for rule in RULES.iter() {
            if let Some(m) = rule.re.find(line) {
                out.push(Finding {
                    rule: rule.name,
                    line: i + 1,
                    excerpt: redact(m.as_str()),
                });
            }
        }

        for m in HIGH_ENTROPY_CANDIDATE.find_iter(line) {
            let tok = m.as_str();
            // 十六进制摘要、git sha、base64 编码的普通文本熵都不够高。
            if shannon_entropy(tok) > ENTROPY_THRESHOLD {
                out.push(Finding {
                    rule: "high-entropy-string",
                    line: i + 1,
                    excerpt: redact(tok),
                });
            }
        }
    }

    out
}

pub fn scan_file(path: &Path) -> Result<Vec<Finding>> {
    let text = std::fs::read_to_string(path)?;
    Ok(scan_text(&text))
}
