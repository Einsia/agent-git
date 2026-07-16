//! Secret scanning.
//!
//! Key insight: **this line of defense has to exist precisely because context gets pushed and cloned.**
//! Shepherd / Zed / Claude Code don't have it, and that's not an oversight -- they don't share context.
//!
//! The scan must cover both the claim body **and the evidence snapshots**, because evidence copies source file contents in verbatim.

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
        re: Regex::new(pat).expect("the regex for a built-in rule must compile"),
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

/// Shannon entropy. Used to catch long strings that match no known format but are clearly random secrets.
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
    let n = s.chars().count();
    if n <= 10 {
        return "*".repeat(n);
    }
    // Take the prefix by **char**, not `&s[..4]` -- the latter panics when the 4th byte lands in the middle of a multi-byte UTF-8 sequence,
    // and a single normal line containing an emoji / CJK text would crash the whole scan (and the scan is the safety gate for commit/push).
    let prefix: String = s.chars().take(4).collect();
    format!("{prefix}…{}", "*".repeat(6))
}

pub fn scan_text(text: &str) -> Vec<Finding> {
    scan_text_opts(text, true)
}

/// `entropy` turns off the generic high-entropy detection -- used for session dumps (jsonl): they're full of UUIDs / requestIds /
/// base64 and similar high-entropy but harmless strings, which the entropy check would flag as false positives like crazy. Sessions rely only on the high-precision specific rules.
pub fn scan_text_opts(text: &str, entropy: bool) -> Vec<Finding> {
    let mut out = Vec::new();

    for (i, line) in text.lines().enumerate() {
        for rule in RULES.iter() {
            if let Some(m) = rule.re.find(line) {
                out.push(Finding {
                    rule: rule.name,
                    line: i + 1,
                    excerpt: redact(m.as_str()),
                });
            }
        }

        if entropy {
            for m in HIGH_ENTROPY_CANDIDATE.find_iter(line) {
                let tok = m.as_str();
                if shannon_entropy(tok) > ENTROPY_THRESHOLD {
                    out.push(Finding {
                        rule: "high-entropy-string",
                        line: i + 1,
                        excerpt: redact(tok),
                    });
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_does_not_panic_on_multibyte_boundary() {
        // Each € takes 3 bytes: the old &s[..4] would cut in the middle of a character and panic.
        let out = redact("€€€€€€€€€€€€");
        assert!(out.starts_with('€'));
        assert!(out.contains('…'));
        // Mixed: multi-byte prefix, must also not panic
        let _ = redact("café_secret_value_1234");
        // Short strings take the '*' branch, counted by char
        assert_eq!(redact("日本語"), "***");
    }

    #[test]
    fn scan_text_survives_multibyte_lines() {
        // A line with multi-byte chars + a real secret: must neither panic nor miss the secret.
        let f = scan_text("日本語 password = caféSecret42x\nAKIAIOSFODNN7EXAMPLE\n");
        assert!(f.iter().any(|x| x.rule == "aws-access-key-id"));
    }
}

pub fn scan_file(path: &Path) -> Result<Vec<Finding>> {
    let text = std::fs::read_to_string(path)?;
    // .md (facts) use the full set including entropy detection; jsonl/json/txt (session dumps) use only the specific rules
    let entropy = path.extension().map(|x| x == "md").unwrap_or(false);
    Ok(scan_text_opts(&text, entropy))
}

/// Recursively scan the text files (.md/.jsonl/.json/.txt) in a directory tree, print the hits, and return the hit count.
/// Session dumps are jsonl, and may carry secrets the agent has seen.
pub fn scan_tree(root: &Path) -> Result<usize> {
    let mut total = 0;
    for e in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        let ext_ok = p
            .extension()
            .map(|x| matches!(x.to_str(), Some("md" | "jsonl" | "json" | "txt")))
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        for f in scan_file(p)? {
            eprintln!(
                "  {}:{}  [{}]  {}",
                p.strip_prefix(root).unwrap_or(p).display(),
                f.line,
                f.rule,
                f.excerpt
            );
            total += 1;
        }
    }
    Ok(total)
}
