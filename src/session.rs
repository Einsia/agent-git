//! 原始 session dump 管理（新模型:不蒸馏 fact,直接版本化 agent 的完整会话)。
//!
//! Claude Code 自己把整个会话 dump 到 ~/.claude/projects/<slug>/:
//!   <uuid>.jsonl              完整转录
//!   <uuid>/subagents/*.jsonl  子 agent 转录
//!   <uuid>/tool-results/*.txt 大工具结果
//!   memory/                   记忆
//! `agit -a sync` 把这坨镜像进 Agent Store 的 sessions/<runtime>/,之后 commit/push/pull 照旧。

use crate::adapter::claude_code;
use crate::scope::{self, Scope};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const SESSIONS_SUBDIR: &str = "sessions";

/// 定位当前项目的 runtime session dump 目录。
fn source_dir(runtime: &str, cwd: &Path) -> Result<PathBuf> {
    match runtime {
        "claude-code" | "claude" | "cc" => {
            let dir = claude_code::projects_dir()?.join(claude_code::slug_for(cwd));
            if !dir.exists() {
                bail!(
                    "找不到本项目的 Claude Code session 目录:{}\n\
                     (这个项目还没在 Claude Code 里跑过?)",
                    dir.display()
                );
            }
            Ok(dir)
        }
        other => bail!("runtime `{other}` 的 session dump 还没接(见 src/session.rs)"),
    }
}

/// `agit -a sync [--from <runtime>]` —— 把 runtime 的 session dump 镜像进 Agent Store。
pub fn sync(runtime: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let dst = agent.join(SESSIONS_SUBDIR).join(&rt);
    std::fs::create_dir_all(&dst)?;

    // runtime 的存储模型不同:Claude 按项目 slug 分目录(整棵镜像);Codex 按日期分目录、
    // 各项目混在一起(按 session_meta.cwd 过滤出本项目的 rollout,只镜像这些)。
    let (stats, source_desc) = match rt.as_str() {
        "claude-code" => {
            let src = source_dir(runtime, &env)?;
            (mirror(&src, &dst)?, src.display().to_string())
        }
        "codex" => codex_collect(&env, &dst)?,
        other => bail!("runtime `{other}` 的 session dump 还没接(见 src/session.rs)"),
    };

    // 落盘前扫一遍密钥 —— dump 全部 session = agent cat 过的一切都在里面
    let hits = crate::scan::scan_tree(&dst)?;

    println!("已镜像 {} 的 session dump:", rt);
    println!("  来源  : {source_desc}");
    println!("  写入  : {}", dst.display());
    println!("  文件  : {} 个({} 更新 / {} 新增),{} 字节", stats.total, stats.updated, stats.added, stats.bytes);
    if hits > 0 {
        eprintln!("  ⚠ 扫到 {hits} 处疑似密钥 —— session 转录里带着 agent 见过的敏感内容。");
        eprintln!("     push 前会再拦一次;先 `agit -a scan` 看看,或从转录里清掉。");
    }
    println!("\n  提交: agit -a add -A && agit -a commit -m 'sync {rt} sessions'");
    Ok(0)
}

fn normalize(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
}

struct Stats {
    total: usize,
    added: usize,
    updated: usize,
    bytes: u64,
}

/// Codex 同步:扫 ~/.codex/sessions,只把 **本项目**(session_meta.cwd == env 根)的 rollout
/// 平铺进 dst/<id>.jsonl。按 cwd 过滤是隐私底线 —— 绝不把别项目的会话卷进来。
fn codex_collect(env: &Path, dst: &Path) -> Result<(Stats, String)> {
    let rollouts = crate::adapter::codex::project_rollouts(env);
    let mut st = Stats { total: 0, added: 0, updated: 0, bytes: 0 };
    for (src, id) in &rollouts {
        let dp = dst.join(format!("{id}.jsonl"));
        let smeta = std::fs::metadata(src)?;
        match std::fs::metadata(&dp) {
            Err(_) => {
                std::fs::copy(src, &dp)?;
                st.added += 1;
            }
            Ok(dmeta) => {
                let newer = match (smeta.modified(), dmeta.modified()) {
                    (Ok(s), Ok(d)) => s > d,
                    _ => true,
                };
                if dmeta.len() != smeta.len() || newer {
                    std::fs::copy(src, &dp)?;
                    st.updated += 1;
                }
            }
        }
        st.total += 1;
        st.bytes += smeta.len();
    }
    let root = crate::adapter::codex::sessions_root()
        .map(|r| r.display().to_string())
        .unwrap_or_default();
    let desc = format!("{root}（cwd={} 过滤出 {} 条）", env.display(), rollouts.len());
    Ok((st, desc))
}

/// 递归镜像 src → dst(只按大小+mtime 判断是否需要覆盖,够用)。
fn mirror(src: &Path, dst: &Path) -> Result<Stats> {
    let mut st = Stats { total: 0, added: 0, updated: 0, bytes: 0 };
    mirror_into(src, dst, &mut st)?;
    Ok(st)
}

fn mirror_into(src: &Path, dst: &Path, st: &mut Stats) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("读 {}", src.display()))? {
        let entry = entry?;
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            mirror_into(&sp, &dp, st)?;
        } else {
            let smeta = entry.metadata()?;
            match std::fs::metadata(&dp) {
                Err(_) => {
                    std::fs::copy(&sp, &dp)?;
                    st.added += 1;
                }
                Ok(dmeta) => {
                    // 大小**或** mtime 变了就重拷。只看大小会漏掉等长的原地改动
                    // (且与本函数注释"大小+mtime"不符);拿不到 mtime 时保守重拷。
                    let newer = match (smeta.modified(), dmeta.modified()) {
                        (Ok(s), Ok(d)) => s > d,
                        _ => true,
                    };
                    if dmeta.len() != smeta.len() || newer {
                        std::fs::copy(&sp, &dp)?;
                        st.updated += 1;
                    }
                }
            }
            st.total += 1;
            st.bytes += smeta.len();
        }
    }
    Ok(())
}

// ─────────────────────── agent 驱动的 merge（对账）───────────────────────
//
// `agit -a reconcile <ref>`：把对面（<ref>）拉进来的 session 并入本地，
// 然后让一个 agent 读懂两边的会话、理解对面的 taste/convention，合成一份统一的工作上下文
// （写进 CLAUDE.md），只把**真正矛盾**的点列出来问用户。非确定性 —— 这是设计如此。

/// 从文本直接解析成 SessionIR（不碰磁盘）——dry-run 里 brief 对面还没落盘的 blob 时用。
fn parse_text(text: &str, id: &str, rt: &str) -> Option<crate::adapter::SessionIR> {
    match normalize(rt).as_str() {
        "claude-code" => Some(claude_code::parse_jsonl(text, id)),
        "codex" => Some(crate::adapter::codex::parse_rollout(text, id)),
        _ => None,
    }
}

/// brief 一份还没落盘的对面 session（`git show <ref>:<path>`），用于 --dry-run 预览。
fn brief_blob(agent: &Path, reference: &str, path: &str, env: &Path, rt: &str) -> Option<String> {
    let (rc, content) = scope::git_in_status(agent, &["show", &format!("{reference}:{path}")]);
    if rc != 0 || content.trim().is_empty() {
        return None;
    }
    let id = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".jsonl");
    let ir = parse_text(&content, id, rt)?;
    Some(brief_from_ir(&ir, env))
}

/// 一条 session 的紧凑摘要（喂给 merge agent，避免把整条 6MB 转录塞进去）。
/// 按 runtime 选对应 adapter 解析,于是 codex 与 claude 的会话都能进同一份 brief。
fn brief(path: &Path, env: &Path, rt: &str) -> Option<String> {
    let ir = crate::adapter::get(rt).ok()?.export(Some(path), env).ok()?;
    Some(brief_from_ir(&ir, env))
}

/// SessionIR → 紧凑 brief 文本（brief 与 brief_blob 共用，规则只写一份）。
fn brief_from_ir(ir: &crate::adapter::SessionIR, env: &Path) -> String {
    let mut s = format!(
        "· 会话 {}{}\n",
        ir.session_id,
        ir.git_branch.as_ref().map(|b| format!("（分支 {b}）")).unwrap_or_default()
    );
    if !ir.prompts.is_empty() {
        s.push_str("  用户要它做的：\n");
        for p in ir.prompts.iter().take(8) {
            s.push_str(&format!("    - {}\n", p.lines().next().unwrap_or("").trim()));
        }
    }
    let concl: Vec<&String> = ir.agent_texts.iter().rev().take(5).collect();
    if !concl.is_empty() {
        s.push_str("  agent 说过的结论/进展（节选）：\n");
        for t in concl.into_iter().rev() {
            let one: String = t.trim().chars().take(240).collect();
            if !one.is_empty() {
                s.push_str(&format!("    - {one}\n"));
            }
        }
    }
    if !ir.writes.is_empty() {
        // env 下的写:显示相对路径;否则(如 codex 会话记的是另一机器的绝对路径)退回 basename,
        // 否则 strip_prefix 失败会把所有 codex 改动文件都吞掉。
        let files: Vec<String> = ir
            .writes
            .iter()
            .map(|w| {
                let p = Path::new(w);
                p.strip_prefix(env)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
                    .or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| w.clone())
            })
            .take(12)
            .collect();
        if !files.is_empty() {
            s.push_str(&format!("  改动的文件：{}\n", files.join(", ")));
        }
    }
    s
}

fn sessions_on_disk(agent: &Path, rt: &str) -> Vec<PathBuf> {
    let dir = agent.join(SESSIONS_SUBDIR).join(rt);
    walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// reconcile 的模式：正常合并、预览、放弃半途合并、定稿手动解决的合并。
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileFlags {
    pub dry_run: bool,
    pub abort: bool,
    pub cont: bool,
}

/// Agent Store 是否有进行中的合并（据此决定能不能再叠一层、或该不该 --continue）。
fn merge_in_progress(agent: &Path) -> bool {
    scope::git_in_status(agent, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).0 == 0
}

/// merge 动了 Agent ref → 记一条 WorkspaceRevision（和 passthrough 一致）。失败只提示。
fn record_merge(before: &str, agent: &Path) {
    let after = scope::git_in_status(agent, &["rev-parse", "HEAD"]).1;
    if after != *before {
        if let Err(e) = crate::workspace::record(&crate::workspace::trigger_label(Scope::Agent, "merge")) {
            eprintln!("agit: 已合并，但生成 WorkspaceRevision 失败: {e:#}");
        }
    }
}

/// 列出 diff-spec 下变动的 session 文件名（.jsonl）。spec 如 ["HEAD...origin/main"] 或 ["ORIG_HEAD","HEAD"]。
fn incoming_sessions(agent: &Path, spec: &[&str], sdir: &str) -> Result<Vec<String>> {
    let mut args = vec!["diff", "--name-only"];
    args.extend_from_slice(spec);
    args.push("--");
    args.push(sdir);
    let (dc, diff) = scope::git_in_status(agent, &args);
    if dc != 0 {
        bail!("算 incoming session 失败（git diff {}）。", spec.join(" "));
    }
    Ok(diff.lines().filter(|l| l.ends_with(".jsonl")).map(String::from).collect())
}

pub fn reconcile(reference: Option<&str>, runtime: &str, flags: ReconcileFlags) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let sdir = format!("{SESSIONS_SUBDIR}/{rt}");

    // ── --abort：放弃半途的合并，回到干净状态 ──
    if flags.abort {
        if !merge_in_progress(&agent) {
            println!("Agent Store 没有进行中的合并，无需 --abort。");
            return Ok(0);
        }
        let (rc, _) = scope::git_in_status(&agent, &["merge", "--abort"]);
        if rc != 0 {
            bail!("git merge --abort 失败。");
        }
        println!("已放弃进行中的合并，Agent Store 回到合并前状态。");
        return Ok(0);
    }

    // ── --continue：把用户手动解决好的文本冲突定稿，再照常合成上下文 ──
    if flags.cont {
        if !merge_in_progress(&agent) {
            bail!("没有进行中的合并可 --continue。先 `agit -a reconcile <ref>`。");
        }
        let (_, unresolved) = scope::git_in_status(&agent, &["diff", "--name-only", "--diff-filter=U"]);
        if !unresolved.trim().is_empty() {
            bail!("还有未解决的冲突文件，先改好并 `agit -a add -A`：\n{unresolved}");
        }
        let before = scope::git_in_status(&agent, &["rev-parse", "HEAD"]).1;
        let (rc, out) = scope::git_in_status(&agent, &["commit", "--no-edit"]);
        if rc != 0 {
            bail!("定稿合并提交失败：\n{out}");
        }
        record_merge(&before, &agent);
        // incoming = 相对合并前（ORIG_HEAD 是我们这侧合并前的 HEAD）新增的 session。
        let incoming = incoming_sessions(&agent, &["ORIG_HEAD", "HEAD"], &sdir)?;
        println!("已定稿合并。");
        return synthesize_and_write(&env, &agent, &rt, &incoming, true);
    }

    // ── 正常 / --dry-run：需要一个 <ref> ──
    let reference = reference.context("reconcile 需要一个 <ref>（如 origin/main），或用 --abort / --continue。")?;

    if merge_in_progress(&agent) {
        bail!("Agent Store 有进行中的合并。先 `agit -a reconcile --continue`（已解决冲突）或 `--abort`。");
    }

    // 0. ref 存在？否则 diff 静默空、被误报成"已是最新"。
    let (rc, _) = scope::git_in_status(&agent, &["rev-parse", "--verify", "--quiet", reference]);
    if rc != 0 {
        bail!("Agent Store 里没有引用 `{reference}`。先 `agit -a fetch <remote>`,再用它的 ref（如 origin/main）。");
    }

    // 1. 三点 diff 求对面相对共同祖先**新增**的 session（两点会把本地独有的也算进来，provenance 反）。
    let incoming = incoming_sessions(&agent, &[&format!("HEAD...{reference}")], &sdir)?;

    // ── --dry-run：不合并、不写盘，只预览合成的上下文 ──
    if flags.dry_run {
        // 本地 = 已落盘；对面 = 从 <ref> 的 blob 读（还没合进来）。
        let local: Vec<String> = sessions_on_disk(&agent, &rt)
            .iter()
            .filter_map(|p| brief(p, &env, &rt))
            .collect();
        let peer: Vec<String> = incoming
            .iter()
            .filter_map(|path| brief_blob(&agent, reference, path, &env, &rt))
            .collect();
        println!("dry-run：{reference} 带来 {} 条 session（不会合并，也不写 CLAUDE.md）。\n", peer.len());
        let (context, conflicts) = synthesize(&local, &peer)?;
        println!("── 预览：统一上下文 ──\n{context}\n");
        report_conflicts(&conflicts);
        return Ok(0);
    }

    // 2. 真正 git 合并（session 是不同 uuid，通常无文本冲突）。
    let before = scope::git_in_status(&agent, &["rev-parse", "HEAD"]).1;
    let (code, out) = scope::git_in_status(&agent, &["merge", "--no-edit", reference]);
    if code != 0 {
        // 文本冲突：**保留**合并中状态交给用户（别 --abort 掉再喊"手动处理"——那会把待解决的东西删了）。
        println!("git 层有文本冲突，已保留合并中状态：\n{out}\n");
        println!("解决办法：");
        println!("  1. 改好冲突文件，`agit -a add -A`");
        println!("  2. `agit -a reconcile --continue`  （定稿并合成上下文）");
        println!("  或 `agit -a reconcile --abort`      （放弃这次合并）");
        return Ok(1);
    }
    record_merge(&before, &agent);

    if incoming.is_empty() {
        println!("{reference} 没带来新的 session（已是最新）。仍会基于本地会话刷新一次上下文。");
    } else {
        println!("并入 {} 条来自 {reference} 的 session。", incoming.len());
    }
    synthesize_and_write(&env, &agent, &rt, &incoming, true)
}

/// 合并落盘后：brief 本地 vs 对面 → 合成 → 写 CLAUDE.md + 持久化冲突清单。
fn synthesize_and_write(env: &Path, agent: &Path, rt: &str, incoming: &[String], write: bool) -> Result<i32> {
    let all = sessions_on_disk(agent, rt);
    let incoming_set: std::collections::HashSet<String> =
        incoming.iter().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()).collect();
    let (mut peer, mut local) = (Vec::new(), Vec::new());
    for p in &all {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(b) = brief(p, env, rt) {
            if incoming_set.contains(&name) {
                peer.push(b);
            } else {
                local.push(b);
            }
        }
    }

    let (context, conflicts) = synthesize(&local, &peer)?;
    if write {
        let claude = crate::commands::write_claude_block(env, &context)?;
        println!("统一上下文 → {}", claude.display());
        persist_conflicts(&conflicts)?;
    }
    report_conflicts(&conflicts);
    Ok(if conflicts.is_empty() { 0 } else { 1 })
}

/// 有 LLM 就语义合成；没有就退回确定性机械并集（reconcile 离线也能用，只是不去重/不判冲突）。
fn synthesize(local: &[String], peer: &[String]) -> Result<(String, Vec<String>)> {
    if crate::llm::available() {
        let prompt = merge_prompt(local, peer);
        eprintln!("让 {} 读两边会话、合成统一上下文……", crate::llm::backend_name());
        let reply = crate::llm::ask(&prompt)?;
        Ok(split_reply(&reply))
    } else {
        eprintln!("没有可用的 LLM 后端 —— 退回确定性机械合并（不做语义去重/冲突识别）。");
        eprintln!("装好 claude / codex，或 export AGIT_LLM_CMD=\"…\" 后重跑，可得语义合并。");
        Ok((deterministic_context(local, peer), Vec::new()))
    }
}

/// 无 LLM 时的确定性上下文：两边 brief 的机械并集，明确标注未做语义合并。
fn deterministic_context(local: &[String], peer: &[String]) -> String {
    let none = "（无）\n".to_string();
    let lo = if local.is_empty() { none.clone() } else { local.join("\n") };
    let pe = if peer.is_empty() { none } else { peer.join("\n") };
    format!(
        "## 统一工作上下文（机械合并 · 无 LLM）\n\n\
         > 没有可用的 LLM 后端，下面是两边会话的机械并集，**未做语义去重与冲突识别**。\n\
         > 装好 claude / codex（或设 AGIT_LLM_CMD）后重跑 `reconcile` 可得语义合并。\n\n\
         ### 本地会话\n{lo}\n\n### 对面带来的会话\n{pe}\n"
    )
}

/// 把冲突清单持久化到 workspace 目录（不再只是打到 stdout 就没了）。无冲突则清掉旧文件。
fn persist_conflicts(conflicts: &[String]) -> Result<()> {
    let path = scope::workspace_dir()?.join("reconcile-conflicts.md");
    if conflicts.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let mut md = String::from("# 待裁决的矛盾（上次 reconcile）\n\n");
    for (i, c) in conflicts.iter().enumerate() {
        md.push_str(&format!("{}. {}\n", i + 1, c));
    }
    md.push_str("\n改定后编辑 CLAUDE.md 的受管区块，或重跑 reconcile。\n");
    std::fs::write(&path, md)?;
    Ok(())
}

fn report_conflicts(conflicts: &[String]) {
    if conflicts.is_empty() {
        println!("没有需要你裁决的矛盾。下个会话直接带着这份上下文。");
    } else {
        println!("\n有 {} 处需要你裁决的矛盾：", conflicts.len());
        for (i, c) in conflicts.iter().enumerate() {
            println!("  {}. {}", i + 1, c);
        }
        println!("\n改定后编辑 CLAUDE.md 的受管区块，或重跑 reconcile。");
    }
}

fn merge_prompt(local: &[String], peer: &[String]) -> String {
    let none = "（无）\n".to_string();
    let lo = if local.is_empty() { none.clone() } else { local.join("\n") };
    let pe = if peer.is_empty() { none } else { peer.join("\n") };
    format!(
        "你在把两个开发者各自的 agent 工作上下文合并成一份，给下一个 agent 用。\n\n\
         【本地已有的会话】\n{lo}\n\n\
         【对面拉进来的会话】\n{pe}\n\n\
         任务：\n\
         1. 读懂对面做了什么、它的风格/约定是什么。\n\
         2. 合成一份**统一的工作上下文**：目标、已知结论、约定、下一步。措辞不同但意思一致的，合成一条。\n\
         3. 只挑出**真正矛盾**的点：一方主张 X，另一方主张与之不相容的 Y。\n\n\
         输出格式（严格）：先写统一上下文的 markdown 正文；最后**单独一个** ```json 代码块，形如：\n\
         {{\"conflicts\": [\"一句话描述矛盾1\", \"矛盾2\"]}}\n\
         没有矛盾就写 {{\"conflicts\": []}}。"
    )
}

/// 把回复拆成 (统一上下文 markdown, 冲突列表)。
fn split_reply(reply: &str) -> (String, Vec<String>) {
    // 找最后一个 ```json 块
    let mut conflicts = Vec::new();
    let mut context = reply.to_string();
    if let Some(i) = reply.rfind("```json") {
        let after = &reply[i + 7..];
        if let Some(j) = after.find("```") {
            let json = after[..j].trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
                if let Some(arr) = v.get("conflicts").and_then(|c| c.as_array()) {
                    conflicts = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect();
                }
            }
            context = reply[..i].trim().to_string();
        }
    }
    (context, conflicts)
}
