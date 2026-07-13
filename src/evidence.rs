//! 证据的采集与校验。
//!
//! 这是 agit 与三个竞品（Shepherd / Zed checkpoint / Claude Code rewind）
//! 分道扬镳的地方：它们记录**状态**，我们记录**结论 + 指向源头的指针**。
//! 有了指针，就能回头问「源头还是当初那样吗」。

use crate::claim::Evidence;
use crate::gitx;
use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDate};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

/// 证据陈旧超过这个天数就判定 STALE。
const DOC_STALE_DAYS: i64 = 365;

// ─────────────────────── 采集侧的 denylist ───────────────────────

/// 这些路径**永不**采集内容快照，只留 locator 与摘要。
///
/// 场景：agent `cat` 了 `.env` 查数据库问题，密码进了 context。
/// 一旦 push、同事 clone，密码就发到每个人机器上。
/// 第一道防线是「根本不抄进来」，而不是事后扫描。
static DENY_GLOBS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    vec![
        ".env",
        ".envrc",
        ".netrc",
        ".npmrc",
        "credentials",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ]
});

static DENY_EXTS: Lazy<Vec<&'static str>> =
    Lazy::new(|| vec!["pem", "key", "p12", "pfx", "jks", "keystore"]);

pub fn is_denied(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if DENY_GLOBS.iter().any(|g| name == *g) || name.starts_with(".env.") {
        return true;
    }
    if let Some(ext) = path.extension() {
        if DENY_EXTS.contains(&ext.to_string_lossy().to_lowercase().as_str()) {
            return true;
        }
    }
    gitx::is_ignored(path)
}

// ─────────────────────────── 摘要 ───────────────────────────

/// 证据摘要取 SHA-256 前 8 个十六进制字符。
/// 这不是安全边界（完整性由 git 的对象哈希保证），
/// 它只需要让「源文件变了」这件事可被廉价检测。
pub fn digest8(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())[..8].to_string()
}

fn read_lines(root: &Path, path: &str, start: usize, end: usize) -> Result<String> {
    let full = root.join(path);
    let text = std::fs::read_to_string(&full)
        .with_context(|| format!("读取 {} 失败", full.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    if start > lines.len() || end > lines.len() {
        bail!("{} 只有 {} 行，取不到 {}-{}", path, lines.len(), start, end);
    }
    Ok(lines[start - 1..end].join("\n"))
}

fn run_cmd(root: &Path, cmd: &str) -> Result<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .output()
        .with_context(|| format!("执行失败: {cmd}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ─────────────────────────── 采集 ───────────────────────────

/// 落盘前把证据「钉」在当下：读取源头、计算摘要、写回 locator。
///
/// **这一步会拒绝没有真实源头的证据。** provenance 因此不是一个
/// 模型可以随便填的字段，而是构造上的准入条件。
pub fn capture(root: &Path, ev: Evidence) -> Result<Evidence> {
    match &ev {
        Evidence::File { path, start, end, .. } => {
            let p = Path::new(path);
            if is_denied(&root.join(p)) || is_denied(p) {
                bail!(
                    "{} 在 denylist 上（.env / 私钥 / 被 gitignore 的路径）。\n\
                     agit 拒绝把它的内容抄进 context —— 这正是密钥泄漏的路径。",
                    path
                );
            }
            let content = read_lines(root, path, *start, *end)?;
            Ok(ev.with_digest(digest8(content.as_bytes())))
        }
        Evidence::Cmd { cmd, .. } => {
            let out = run_cmd(root, cmd)?;
            Ok(ev.with_digest(digest8(out.as_bytes())))
        }
        // doc / human 的新鲜度由日期决定，没有可算的摘要。
        _ => Ok(ev),
    }
}

// ─────────────────────────── 校验 ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 源头仍与采集时一致。
    Fresh,
    /// 需要重跑命令才能判定（默认不重跑，见下）。
    Recheck,
    /// 无法自动判定（外部文档、人工决策）。
    Unverifiable,
    /// 源头已经变了，或证据已经过期。
    Stale,
    /// 源头不存在了。
    Missing,
}

impl Status {
    pub fn rank(self) -> u8 {
        match self {
            Status::Fresh => 4,
            Status::Recheck => 3,
            Status::Unverifiable => 2,
            Status::Stale => 1,
            Status::Missing => 0,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Fresh => "FRESH",
            Status::Recheck => "RECHECK",
            Status::Unverifiable => "UNVERIFIABLE",
            Status::Stale => "STALE",
            Status::Missing => "MISSING",
        }
    }
}

pub struct Verdict {
    pub status: Status,
    pub detail: String,
}

fn v(status: Status, detail: impl Into<String>) -> Verdict {
    Verdict {
        status,
        detail: detail.into(),
    }
}

/// 校验一条证据。
///
/// `rerun` 控制是否重新执行 `cmd:` 证据。**默认为 false，这是安全设计**：
/// 一条来自别人分支的 claim 可以携带任意 shell 命令，clone 下来跑 `agit verify`
/// 就等于执行陌生人的代码。必须显式 `--rerun` 才会执行。
pub fn verify(root: &Path, ev: &Evidence, rerun: bool) -> Verdict {
    match ev {
        Evidence::File {
            path,
            start,
            end,
            digest,
        } => {
            let full = root.join(path);
            if !full.exists() {
                return v(Status::Missing, format!("{path} 不存在了"));
            }
            let content = match read_lines(root, path, *start, *end) {
                Ok(c) => c,
                Err(e) => return v(Status::Missing, e.to_string()),
            };
            let now = digest8(content.as_bytes());
            match digest {
                None => v(Status::Unverifiable, format!("{path} 未记录摘要")),
                Some(d) if *d == now => {
                    let first = content.lines().next().unwrap_or("").trim();
                    v(Status::Fresh, format!("{path}:{start} → {first}"))
                }
                Some(d) => v(
                    Status::Stale,
                    format!("{path}:{start} 已变更（{d} → {now}）"),
                ),
            }
        }

        Evidence::Cmd { cmd, digest } => {
            if !rerun {
                return v(
                    Status::Recheck,
                    format!("需重跑才能判定（--rerun 显式启用）: {cmd}"),
                );
            }
            let out = match run_cmd(root, cmd) {
                Ok(o) => o,
                Err(e) => return v(Status::Missing, e.to_string()),
            };
            let now = digest8(out.as_bytes());
            match digest {
                None => v(Status::Unverifiable, "未记录摘要"),
                Some(d) if *d == now => v(Status::Fresh, format!("重跑一致: {cmd}")),
                Some(d) => v(Status::Stale, format!("重跑结果已变（{d} → {now}）: {cmd}")),
            }
        }

        Evidence::Doc {
            reference,
            captured,
        } => {
            let today: NaiveDate = Local::now().date_naive();
            let age = (today - *captured).num_days();
            if age > DOC_STALE_DAYS {
                v(Status::Stale, format!("{reference}，{age} 天前采集"))
            } else {
                v(Status::Fresh, format!("{reference}，{age} 天前采集"))
            }
        }

        // 人工决策不随代码失效；它只会被新的决策覆盖。
        Evidence::Human { who, at } => v(
            Status::Unverifiable,
            format!("{who} 于 {at} 的人工决策，不随代码失效"),
        ),
    }
}

/// 一条 claim 的整体状态 = 其证据中最强的那一条。
pub fn claim_status(root: &Path, evidence: &[Evidence], rerun: bool) -> (Status, Vec<Verdict>) {
    let verdicts: Vec<Verdict> = evidence.iter().map(|e| verify(root, e, rerun)).collect();
    let best = verdicts
        .iter()
        .map(|v| v.status)
        .max_by_key(|s| s.rank())
        .unwrap_or(Status::Missing);
    (best, verdicts)
}
