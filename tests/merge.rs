//! merge driver 的裁决 golden 测试。
//!
//! 直接驱动 `agit merge-file %O %A %B %P`（git 在 Agent Store 里调用它的方式）。
//! 「裁决是确定性的」是这个产品的全部卖点，跨 v1→v2 不变，测试也不变。
//!
//! 为避免依赖还没接回的证据摘要捕获，这里用 doc:/human: 证据构造 FRESH/STALE。

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agit");

fn claim(evidence: &str, body: &str) -> String {
    format!(
        "---\nsubject: a/b\ntier: reversible\nauthor: t\n\
         created: 2026-07-09T10:00:00Z\nevidence:\n- '{evidence}'\n---\n\n{body}\n"
    )
}

/// 写三个临时文件，跑 merge-file，返回 (退出码, 写回 %A 的内容)。
fn drive(base: Option<&str>, ours: &str, theirs: &str) -> (i32, String) {
    let d = tempfile::tempdir().unwrap();
    let (b, o, t) = (d.path().join("O"), d.path().join("A"), d.path().join("B"));
    std::fs::write(&b, base.unwrap_or("")).unwrap();
    std::fs::write(&o, ours).unwrap();
    std::fs::write(&t, theirs).unwrap();

    let out = Command::new(BIN)
        .arg("merge-file")
        .arg(&b)
        .arg(&o)
        .arg(&t)
        .arg("state/facts/a/b.md")
        .current_dir(d.path())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        std::fs::read_to_string(&o).unwrap(),
    )
}

#[test]
fn identical_bodies_union_evidence() {
    let ours = claim("doc:x.md@2026-07-01", "同一个结论。");
    let theirs = claim("human:alice@2026-07-01", "同一个结论。");
    let (code, out) = drive(None, &ours, &theirs);
    assert_eq!(code, 0, "正文相同不该冲突");
    assert!(out.contains("doc:x.md@2026-07-01"));
    assert!(out.contains("human:alice@2026-07-01"), "证据应取并集:\n{out}");
    assert!(!out.contains("<<<<<<<"));
}

#[test]
fn ours_unchanged_takes_theirs() {
    let base = claim("doc:x.md@2026-07-01", "旧。");
    let theirs = claim("doc:x.md@2026-07-01", "新。");
    let (code, out) = drive(Some(&base), &base, &theirs);
    assert_eq!(code, 0);
    assert!(out.contains("新。"), "我方未动应取对方:\n{out}");
}

/// 核心：新鲜证据 vs 陈旧证据 → 建议新鲜的一侧。裁决来自合并时的证据状态。
#[test]
fn conflict_recommends_fresher_evidence() {
    let fresh = claim("doc:code.md@2026-07-01", "字段叫 user_id。"); // 近期
    let stale = claim("doc:wiki.md@2019-01-01", "字段叫 uid。"); // 2019
    let (code, out) = drive(None, &fresh, &stale);
    assert_eq!(code, 1, "双方都改必须冲突");
    assert!(out.contains("<<<<<<< ours") && out.contains(">>>>>>> theirs"));
    assert!(out.contains("[FRESH]"), "近期 doc 应 FRESH:\n{out}");
    assert!(out.contains("[STALE]"), "2019 的 doc 应 STALE:\n{out}");
    assert!(out.contains("建议采纳: ours"), "应建议更新鲜的一侧:\n{out}");
}

/// 对称：把双方对调，建议必须翻转。确定性不等于偏心。
#[test]
fn recommendation_is_symmetric() {
    let fresh = claim("doc:code.md@2026-07-01", "新鲜。");
    let stale = claim("doc:wiki.md@2019-01-01", "陈旧。");
    let (_, a) = drive(None, &fresh, &stale);
    let (_, b) = drive(None, &stale, &fresh);
    assert!(a.contains("建议采纳: ours"));
    assert!(b.contains("建议采纳: theirs"), "对调后建议必须翻转:\n{b}");
}

#[test]
fn equal_strength_refuses_to_guess() {
    let ours = claim("doc:x.md@2026-07-01", "说法一。");
    let theirs = claim("doc:y.md@2026-07-02", "说法二。");
    let (code, out) = drive(None, &ours, &theirs);
    assert_eq!(code, 1);
    assert!(out.contains("无法自动判定"), "强度相同不该给建议:\n{out}");
}

#[test]
fn unparseable_falls_back_to_raw_conflict() {
    let (code, out) = drive(None, "不是 claim\n", "也不是\n");
    assert_eq!(code, 1);
    assert!(out.contains("<<<<<<< ours"));
    assert!(!out.contains("建议采纳"), "解析不了就绝不猜");
}
