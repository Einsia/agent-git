//! subject 语义对齐 —— 堵住「语义相同、subject 不同 → 悄悄共存」这个漏判。
//!
//! 在 fact 落盘**之前**判断：这条新结论是不是已有某条的同义？
//!   - 有 LLM 后端（默认 claude）：让它从已有 subject 里选一个同义的，或答 NONE。
//!     模型只能从**已有列表**里选，不能自由发挥 —— 和抽取那套「只能引用池内」一个思路。
//!   - 没有 LLM：退回确定性启发式（同一父路径 + 正文词重叠），只当低置信提示。
//!
//! 这一步产出的是**建议**（可 --force 覆盖），不进 merge 的确定性裁决路径。

use crate::llm;

pub enum Alignment {
    /// 没找到同义的，放心新建。
    Fresh,
    /// 疑似与已有 subject 同义。
    Duplicate { subject: String, how: &'static str },
}

/// existing: 已有 fact 的 (subject, 正文首行)。
pub fn check(new_subject: &str, new_body: &str, existing: &[(String, String)]) -> Alignment {
    if existing.is_empty() {
        return Alignment::Fresh;
    }
    if llm::available() {
        if let Some(s) = ask_llm(new_subject, new_body, existing) {
            return Alignment::Duplicate { subject: s, how: "llm" };
        }
        return Alignment::Fresh;
    }
    // 无模型：确定性启发式
    if let Some(s) = heuristic(new_subject, new_body, existing) {
        return Alignment::Duplicate { subject: s, how: "heuristic" };
    }
    Alignment::Fresh
}

fn ask_llm(new_subject: &str, new_body: &str, existing: &[(String, String)]) -> Option<String> {
    let mut list = String::new();
    for (s, first) in existing.iter().take(200) {
        list.push_str(&format!("- {s} :: {}\n", first.trim()));
    }
    let prompt = format!(
        "你在帮一个知识库去重。下面有一批已有「结论」和一条「新结论」。\n\
         判断：新结论陈述的**事实**，是否已经被已有的某一条覆盖了 —— 哪怕命名、措辞、\n\
         角度完全不同，只要说的是**同一个客观事实**，就算命中。宁可多报，供人确认。\n\n\
         例子：\n\
         已有「api/user/id :: 用户主键字段是 user_id」，新「user/identity :: 用户唯一标识那个字段叫 user_id」\n\
         → 命中，都是在说同一个字段。\n\n\
         格式：subject :: 一句话结论\n\
         已有：\n{list}\n\
         新结论：\n- {new_subject} :: {new_body}\n\n\
         先想一句，最后**单独一行**只写命中的那个已有 subject（原样复制），或写 NONE。"
    );
    let reply = llm::ask(&prompt).ok()?;
    let ans = reply.lines().map(|l| l.trim()).rev().find(|l| !l.is_empty())?;
    if ans.eq_ignore_ascii_case("none") {
        return None;
    }
    // 模型答案含某个已有 subject 即算命中（容忍它多说几个字）。
    existing.iter().map(|(s, _)| s).find(|s| ans.contains(s.as_str())).cloned()
}

/// 确定性启发式：同一父路径 + 正文词 Jaccard ≥ 0.5。低置信，只作提示。
fn heuristic(new_subject: &str, new_body: &str, existing: &[(String, String)]) -> Option<String> {
    let parent = |s: &str| s.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let np = parent(new_subject);
    let nw = words(new_body);
    for (s, first) in existing {
        if parent(s) != np || np.is_empty() {
            continue;
        }
        if jaccard(&nw, &words(first)) >= 0.5 {
            return Some(s.clone());
        }
    }
    None
}

fn words(s: &str) -> Vec<String> {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() > 1)
        .map(|w| w.to_lowercase())
        .collect()
}

fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: std::collections::HashSet<_> = a.iter().collect();
    let sb: std::collections::HashSet<_> = b.iter().collect();
    let inter = sa.intersection(&sb).count() as f64;
    let uni = sa.union(&sb).count() as f64;
    inter / uni
}
