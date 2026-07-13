//! Claim：一条带出处的结论。它是 agit 的原子单位。
//!
//! 磁盘表示就是 `ctx/<subject>.md`：YAML frontmatter + Markdown 正文。
//! **subject 即路径**，这是整个设计的支点 —— git 的三方树合并因此直接成为语义合并：
//! 两个分支改了同一条 claim，git 报冲突；改了不同 claim，git 静默合并。

use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// fact 在 Agent Store 里的相对路径前缀。subject 即路径。
pub const CTX_DIR: &str = "state/facts";

// ─────────────────────────── Tier ───────────────────────────

/// 结论的可逆性分层。借自 Shepherd 的 effect reversibility tier，
/// 但作用在「知识」而非「副作用」上 —— 它决定一条结论如何失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// 从代码读出的事实。源变了就自动失效。
    Reversible,
    /// 从命令输出得出的结论。要重跑才能验证。
    Compensable,
    /// 人做出的决策。不能靠重放推翻，只能被新决策覆盖。
    Irreversible,
}

impl Tier {
    pub fn rank(self) -> u8 {
        match self {
            Tier::Irreversible => 2,
            Tier::Reversible => 1,
            Tier::Compensable => 0,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tier::Reversible => "reversible",
            Tier::Compensable => "compensable",
            Tier::Irreversible => "irreversible",
        };
        f.write_str(s)
    }
}

// ───────────────────────── Evidence ─────────────────────────

/// 证据位点。**没有可验证证据的 claim 不允许入库** —— 这是 provenance
/// 从「模型可以随便填的字段」变成硬约束的地方。
///
/// 文本形式（frontmatter 里就长这样）：
/// ```text
/// file:models/user.ts:4 #a937b4a5
/// file:services/order.ts:11-18 #1f2e3d4c
/// cmd:grep -n 'await.*for' services/order.ts #9c8b7a6d
/// doc:docs/api-v1.md@2024-03-11
/// human:alice@2026-07-09
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Evidence {
    File {
        path: String,
        start: usize,
        end: usize,
        digest: Option<String>,
    },
    Cmd {
        cmd: String,
        digest: Option<String>,
    },
    Doc {
        reference: String,
        captured: NaiveDate,
    },
    Human {
        who: String,
        at: NaiveDate,
    },
}

impl Evidence {
    /// 该证据类型隐含的 tier。
    pub fn implied_tier(&self) -> Tier {
        match self {
            Evidence::File { .. } => Tier::Reversible,
            Evidence::Cmd { .. } => Tier::Compensable,
            Evidence::Doc { .. } => Tier::Reversible,
            Evidence::Human { .. } => Tier::Irreversible,
        }
    }

    pub fn with_digest(self, d: String) -> Self {
        match self {
            Evidence::File {
                path, start, end, ..
            } => Evidence::File {
                path,
                start,
                end,
                digest: Some(d),
            },
            Evidence::Cmd { cmd, .. } => Evidence::Cmd {
                cmd,
                digest: Some(d),
            },
            other => other,
        }
    }

    pub fn digest(&self) -> Option<&str> {
        match self {
            Evidence::File { digest, .. } | Evidence::Cmd { digest, .. } => digest.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Evidence::File {
                path,
                start,
                end,
                digest,
            } => {
                if start == end {
                    write!(f, "file:{}:{}", path, start)?;
                } else {
                    write!(f, "file:{}:{}-{}", path, start, end)?;
                }
                if let Some(d) = digest {
                    write!(f, " #{}", d)?;
                }
                Ok(())
            }
            Evidence::Cmd { cmd, digest } => {
                write!(f, "cmd:{}", cmd)?;
                if let Some(d) = digest {
                    write!(f, " #{}", d)?;
                }
                Ok(())
            }
            Evidence::Doc {
                reference,
                captured,
            } => write!(f, "doc:{}@{}", reference, captured),
            Evidence::Human { who, at } => write!(f, "human:{}@{}", who, at),
        }
    }
}

impl From<Evidence> for String {
    fn from(e: Evidence) -> String {
        e.to_string()
    }
}

impl TryFrom<String> for Evidence {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl FromStr for Evidence {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let (kind, rest) = s
            .split_once(':')
            .ok_or_else(|| anyhow!("证据缺少类型前缀（file: / cmd: / doc: / human:）: {s}"))?;

        // 摘要统一以 " #" 结尾，与命令里可能出现的 '#' 区分开。
        let (rest, digest) = match rest.rsplit_once(" #") {
            Some((r, d)) if !d.contains(' ') => (r, Some(d.to_string())),
            _ => (rest, None),
        };

        match kind {
            "file" => {
                let (path, lines) = rest
                    .rsplit_once(':')
                    .ok_or_else(|| anyhow!("file 证据缺少行号: {s}"))?;
                let (start, end) = match lines.split_once('-') {
                    Some((a, b)) => (a.parse()?, b.parse()?),
                    None => {
                        let n: usize = lines.parse()?;
                        (n, n)
                    }
                };
                if start == 0 || end < start {
                    bail!("行号范围非法: {lines}");
                }
                Ok(Evidence::File {
                    path: path.to_string(),
                    start,
                    end,
                    digest,
                })
            }
            "cmd" => Ok(Evidence::Cmd {
                cmd: rest.to_string(),
                digest,
            }),
            "doc" => {
                let (r, date) = rest
                    .rsplit_once('@')
                    .ok_or_else(|| anyhow!("doc 证据缺少采集日期 @YYYY-MM-DD: {s}"))?;
                Ok(Evidence::Doc {
                    reference: r.to_string(),
                    captured: date.parse().context("采集日期解析失败")?,
                })
            }
            "human" => {
                let (who, date) = rest
                    .rsplit_once('@')
                    .ok_or_else(|| anyhow!("human 证据缺少日期 @YYYY-MM-DD: {s}"))?;
                Ok(Evidence::Human {
                    who: who.to_string(),
                    at: date.parse().context("日期解析失败")?,
                })
            }
            other => bail!("未知证据类型 `{other}`（支持 file / cmd / doc / human）"),
        }
    }
}

// ─────────────────────────── Claim ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub subject: String,
    pub tier: Tier,
    pub author: String,
    pub created: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub meta: Meta,
    pub body: String,
}

impl Claim {
    /// 规范化序列化。字段顺序固定、LF 换行、结尾恰好一个换行 ——
    /// 内容哈希的稳定性依赖于此，不要随意改动。
    pub fn render(&self) -> Result<String> {
        let yaml = serde_yaml::to_string(&self.meta)?;
        Ok(format!("---\n{}---\n\n{}\n", yaml, self.body.trim_end()))
    }

    pub fn parse(text: &str) -> Result<Claim> {
        let text = text.strip_prefix("---\n").ok_or_else(|| {
            anyhow!("claim 文件必须以 YAML frontmatter 开头（`---` 起始行）")
        })?;
        let (yaml, body) = text
            .split_once("\n---\n")
            .ok_or_else(|| anyhow!("claim 文件的 frontmatter 没有闭合的 `---`"))?;
        let meta: Meta = serde_yaml::from_str(yaml).context("frontmatter 解析失败")?;
        Ok(Claim {
            meta,
            body: body.trim().to_string(),
        })
    }

    pub fn load(path: &Path) -> Result<Claim> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        Claim::parse(&text).with_context(|| format!("解析 {} 失败", path.display()))
    }
}

// ─────────────────────── subject ↔ path ───────────────────────

/// subject 必须是安全的相对路径片段。它会直接变成文件路径，
/// 所以这里是一道安全边界：拒绝 `..`、绝对路径、以及路径分隔符之外的怪字符。
pub fn validate_subject(subject: &str) -> Result<()> {
    if subject.is_empty() {
        bail!("subject 不能为空");
    }
    if subject.starts_with('/') || subject.ends_with('/') {
        bail!("subject 不能以 `/` 开头或结尾: {subject}");
    }
    for seg in subject.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            bail!("subject 含有非法路径段: {subject}");
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            bail!("subject 只允许 [a-zA-Z0-9._-] 与 `/`: {subject}");
        }
    }
    Ok(())
}

pub fn subject_to_path(subject: &str) -> Result<PathBuf> {
    validate_subject(subject)?;
    Ok(PathBuf::from(CTX_DIR).join(format!("{subject}.md")))
}

pub fn path_to_subject(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let s = s.strip_prefix(&format!("{CTX_DIR}/"))?;
    Some(s.strip_suffix(".md")?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// locator 的文本形式是磁盘格式的一部分，round-trip 必须精确。
    #[test]
    fn evidence_roundtrip() {
        let cases = [
            "file:models/user.ts:4",
            "file:models/user.ts:4 #a937b4a5",
            "file:services/order.ts:7-10 #d59b83ff",
            "cmd:grep -c 'await db' services/order.ts",
            "cmd:grep -c 'await db' services/order.ts #53c234e5",
            "doc:docs/api-v1.md@2024-03-11",
            "human:alice@2026-07-09",
        ];
        for c in cases {
            let e: Evidence = c.parse().unwrap_or_else(|e| panic!("{c}: {e}"));
            assert_eq!(e.to_string(), c, "round-trip 不一致");
        }
    }

    /// 命令里带冒号不能把 locator 解析弄乱。
    #[test]
    fn cmd_with_colon() {
        let e: Evidence = "cmd:sed -n '4p' a.ts | tr -d ':'".parse().unwrap();
        assert!(matches!(e, Evidence::Cmd { .. }));
        assert_eq!(e.to_string(), "cmd:sed -n '4p' a.ts | tr -d ':'");
    }

    #[test]
    fn tier_inferred_from_evidence_kind() {
        let f: Evidence = "file:a.ts:1".parse().unwrap();
        let c: Evidence = "cmd:ls".parse().unwrap();
        let h: Evidence = "human:alice@2026-01-01".parse().unwrap();
        assert_eq!(f.implied_tier(), Tier::Reversible);
        assert_eq!(c.implied_tier(), Tier::Compensable);
        assert_eq!(h.implied_tier(), Tier::Irreversible);
        assert!(h.implied_tier().rank() > f.implied_tier().rank());
    }

    /// subject 会直接变成文件路径，这里是一道安全边界。
    #[test]
    fn subject_rejects_traversal() {
        for bad in [
            "../etc/passwd",
            "/abs/path",
            "a//b",
            "a/../b",
            "trailing/",
            "",
            "has space",
            "semi;colon",
        ] {
            assert!(validate_subject(bad).is_err(), "应当拒绝: {bad:?}");
        }
        for good in ["api/user/id-field-name", "a", "a_b/c-d.e"] {
            assert!(validate_subject(good).is_ok(), "应当接受: {good:?}");
        }
    }

    #[test]
    fn subject_path_roundtrip() {
        let s = "api/user/id-field-name";
        let p = subject_to_path(s).unwrap();
        assert_eq!(p, PathBuf::from("state/facts/api/user/id-field-name.md"));
        assert_eq!(path_to_subject(&p).as_deref(), Some(s));
    }

    /// 规范化序列化：解析再渲染必须回到同样的字节。
    #[test]
    fn claim_render_is_canonical() {
        let text = "---\nsubject: a/b\ntier: reversible\nauthor: alice\n\
                    created: 2026-07-09T10:00:00Z\nevidence:\n- 'file:x.ts:1 #deadbeef'\n---\n\n结论。\n";
        let c = Claim::parse(text).unwrap();
        assert_eq!(c.render().unwrap(), text);
    }

    #[test]
    fn claim_rejects_missing_frontmatter() {
        assert!(Claim::parse("没有 frontmatter").is_err());
        assert!(Claim::parse("---\nsubject: a\n").is_err()); // 未闭合
    }
}
