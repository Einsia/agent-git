//! 语义合并：git 的三方树合并 + 我们的裁决。
//!
//! `.gitattributes` 里 `ctx/** merge=agit` 把 claim 路径绑到本驱动上。
//! 当且仅当同一条 claim 在两个分支上都被修改时，git 才会调用它，
//! 并传入三个临时文件：%O 共同祖先、%A 我方（也是输出）、%B 对方。
//!
//! 裁决必须是**确定性**的 —— 同样的三份输入跑一万次结果一样。
//! 所以判据是「证据在合并那一刻的真实状态」，不是模型的猜测。

use crate::claim::{subject_to_path, Claim, Evidence, Tier};
use crate::evidence::{claim_status, Status};
use crate::gitx;
use anyhow::{bail, Result};
use std::path::Path;

const OURS_BEGIN: &str = "<<<<<<< ours";
const SEP: &str = "=======";
const THEIRS_END: &str = ">>>>>>> theirs";

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// 同一条结论、同样的正文，只是证据不同 → 取并集，不算冲突。
fn union_evidence(a: &[Evidence], b: &[Evidence]) -> Vec<Evidence> {
    let mut out = a.to_vec();
    for e in b {
        if !out.contains(e) {
            out.push(e.clone());
        }
    }
    out
}

/// 判断一侧相对祖先是否「没动」。只比较承载语义的部分，
/// 忽略 author / created —— 那两个字段每次落盘都会变。
fn same_substance(x: &Claim, y: &Claim) -> bool {
    x.body == y.body && x.meta.evidence == y.meta.evidence
}

struct SideVerdict {
    status: Status,
    lines: Vec<String>,
    tier: Tier,
}

fn assess(root: &Path, c: &Claim) -> SideVerdict {
    // 合并期不重跑命令：一条来自对方分支的 claim 可以携带任意 shell 命令。
    let (status, verdicts) = claim_status(root, &c.meta.evidence, false);
    let lines = c
        .meta
        .evidence
        .iter()
        .zip(verdicts.iter())
        .map(|(e, v)| format!("[{}] {} — {}", v.status.label(), e, v.detail))
        .collect();
    SideVerdict {
        status,
        lines,
        tier: c.meta.tier,
    }
}

/// merge driver 入口：`agit merge-file %O %A %B %P`
pub fn driver(base: &Path, ours: &Path, theirs: &Path, path: &str) -> Result<i32> {
    let root = gitx::repo_root().unwrap_or_else(|_| Path::new(".").to_path_buf());

    let (bt, ot, tt) = (read(base), read(ours), read(theirs));

    let (ob, tb) = match (Claim::parse(&ot), Claim::parse(&tt)) {
        (Ok(o), Ok(t)) => (o, t),
        _ => {
            // 解析不了就退回原始三方冲突，绝不猜。
            let out = format!("{OURS_BEGIN}\n{ot}{SEP}\n{tt}{THEIRS_END}\n");
            std::fs::write(ours, out)?;
            return Ok(1);
        }
    };
    let bc = Claim::parse(&bt).ok();

    // ── 情况一：正文一致 → 合并证据，无冲突 ──
    if ob.body == tb.body {
        let mut merged = ob.clone();
        merged.meta.evidence = union_evidence(&ob.meta.evidence, &tb.meta.evidence);
        std::fs::write(ours, merged.render()?)?;
        return Ok(0);
    }

    // ── 情况二：一侧未动 → 取另一侧 ──
    if let Some(b) = &bc {
        if same_substance(&ob, b) {
            std::fs::write(ours, tt)?;
            return Ok(0);
        }
        if same_substance(&tb, b) {
            return Ok(0); // 保留我方
        }
    }

    // ── 情况三：真冲突 → 当场重新校验双方证据，给出确定性建议 ──
    let ov = assess(&root, &ob);
    let tv = assess(&root, &tb);

    let recommend = match ov.status.rank().cmp(&tv.status.rank()) {
        std::cmp::Ordering::Greater => Some("ours"),
        std::cmp::Ordering::Less => Some("theirs"),
        std::cmp::Ordering::Equal => match ov.tier.rank().cmp(&tv.tier.rank()) {
            std::cmp::Ordering::Greater => Some("ours"),
            std::cmp::Ordering::Less => Some("theirs"),
            std::cmp::Ordering::Equal => None,
        },
    };

    let subject = &ob.meta.subject;
    let mut out = String::new();
    out.push_str(&format!("{OURS_BEGIN}\n{ot}"));
    out.push_str(&format!("{SEP}\n{tt}"));
    out.push_str(&format!("{THEIRS_END}\n\n"));

    out.push_str("# ─────────────── agit 证据裁决 ───────────────\n");
    out.push_str(&format!("# ours   (tier={})\n", ov.tier));
    for l in &ov.lines {
        out.push_str(&format!("#     {l}\n"));
    }
    out.push_str(&format!("# theirs (tier={})\n", tv.tier));
    for l in &tv.lines {
        out.push_str(&format!("#     {l}\n"));
    }
    out.push_str("#\n");
    match recommend {
        Some(side) => {
            out.push_str(&format!(
                "# 建议采纳: {side}   （ours={} / theirs={}，依据合并时的证据状态，非模型判断）\n",
                ov.status.label(),
                tv.status.label()
            ));
            out.push_str(&format!("#   agit resolve {subject} --take {side}\n"));
        }
        None => {
            out.push_str("# 无法自动判定：双方证据强度相同。需要人类裁决。\n");
            out.push_str(&format!("#   agit resolve {subject} --take ours|theirs\n"));
        }
    }
    out.push_str(&format!(
        "# （路径 {path}；本段注释会在 resolve 时被剥除）\n"
    ));

    std::fs::write(ours, out)?;
    Ok(1)
}

/// 从冲突文件里取出指定一侧，写回成一条干净的 claim。
pub fn resolve(subject: &str, take: &str) -> Result<i32> {
    let root = gitx::repo_root()?;
    let rel = subject_to_path(subject)?;
    let path = root.join(&rel);

    let text = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("找不到 {}", rel.display()))?;

    let Some(head) = text.find(&format!("{OURS_BEGIN}\n")) else {
        bail!("{} 当前不处于冲突状态", rel.display());
    };
    let body = &text[head + OURS_BEGIN.len() + 1..];
    let Some(sep) = body.find(&format!("\n{SEP}\n")) else {
        bail!("冲突标记损坏：找不到 `{SEP}`");
    };
    let after = &body[sep + SEP.len() + 2..];
    let Some(end) = after.find(&format!("\n{THEIRS_END}")) else {
        bail!("冲突标记损坏：找不到 `{THEIRS_END}`");
    };

    let chosen = match take {
        "ours" => &body[..sep + 1],
        "theirs" => &after[..end + 1],
        _ => bail!("--take 只能是 ours 或 theirs"),
    };

    // 走一遍 parse/render，保证落盘的是规范化形式。
    let claim = Claim::parse(chosen)?;
    std::fs::write(&path, claim.render()?)?;

    gitx::git(&["add", &rel.to_string_lossy()])?;
    println!("已采纳 {take}：{}", rel.display());
    println!("  {}", claim.body.lines().next().unwrap_or(""));
    Ok(0)
}
