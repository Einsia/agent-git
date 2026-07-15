//! 原始 session dump 管理（新模型:不蒸馏 fact,直接版本化 agent 的完整会话)。
//!
//! Claude Code 自己把整个会话 dump 到 ~/.claude/projects/<slug>/:
//!   <uuid>.jsonl              完整转录
//!   <uuid>/subagents/*.jsonl  子 agent 转录
//!   <uuid>/tool-results/*.txt 大工具结果
//!   memory/                   记忆
//! `agit -a sync` 把这坨镜像进 Agent Store 的 sessions/<runtime>/,之后 commit/push/pull 照旧。

use crate::adapter::{claude_code, Adapter, SessionIR};
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
    let src = source_dir(runtime, &env)?;
    let dst = agent.join(SESSIONS_SUBDIR).join(&rt);

    std::fs::create_dir_all(&dst)?;
    let stats = mirror(&src, &dst)?;

    // 落盘前扫一遍密钥 —— dump 全部 session = agent cat 过的一切都在里面
    let hits = crate::scan::scan_tree(&dst)?;

    println!("已镜像 {} 的 session dump:", rt);
    println!("  来源  : {}", src.display());
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

/// 一条 session 的紧凑摘要（喂给 merge agent，避免把整条 6MB 转录塞进去）。
fn brief(path: &Path, env: &Path) -> Option<String> {
    let ir: SessionIR = claude_code::ClaudeCode.export(Some(path), env).ok()?;
    let mut s = format!(
        "· 会话 {}{}\n",
        ir.session_id,
        ir.git_branch.map(|b| format!("（分支 {b}）")).unwrap_or_default()
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
        let files: Vec<String> = ir
            .writes
            .iter()
            .filter_map(|w| Path::new(w).strip_prefix(env).ok().map(|r| r.to_string_lossy().into_owned()))
            .take(12)
            .collect();
        if !files.is_empty() {
            s.push_str(&format!("  改动的文件：{}\n", files.join(", ")));
        }
    }
    Some(s)
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

pub fn reconcile(reference: &str, runtime: &str) -> Result<i32> {
    if !crate::llm::available() {
        bail!(
            "reconcile 要一个 LLM 后端来读会话（默认本机 claude）。\n\
             装好 claude，或 export AGIT_LLM_CMD=\"<从 stdin 读 prompt 的命令>\"。"
        );
    }
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let sdir = format!("{SESSIONS_SUBDIR}/{rt}");

    // 0. 先确认 reference 存在 —— 否则 diff 静默返回空,会被误报成"已是最新",
    //    真正的错误要等到下面 git merge 才炸出来,误导人。
    let (rc, _) = scope::git_in_status(&agent, &["rev-parse", "--verify", "--quiet", reference]);
    if rc != 0 {
        bail!("Agent Store 里没有引用 `{reference}`。先 `agit -a fetch <remote>`,再用它的 ref（如 origin/main）。");
    }

    // 1. 合并前，先算出对面带来的是哪些 session。
    //    **三点 diff**(merge-base..reference):只列对面相对共同祖先**新增**的 session。
    //    两点 `HEAD reference` 会把"本地有、对面没有"的自己的 session 也列进来 —— 于是
    //    用户自己的会话被误判成"对面拉进来的",provenance 整个反了(见回归测试)。
    let (dc, diff) = scope::git_in_status(
        &agent,
        &["diff", "--name-only", &format!("HEAD...{reference}"), "--", &sdir],
    );
    if dc != 0 {
        bail!("算 incoming session 失败(git diff HEAD...{reference})。");
    }
    let incoming: Vec<String> = diff
        .lines()
        .filter(|l| l.ends_with(".jsonl"))
        .map(String::from)
        .collect();

    // 2. 真正 git 合并（session 是不同 uuid，通常无文本冲突）
    let before = scope::git_in_status(&agent, &["rev-parse", "HEAD"]).1;
    let (code, out) = scope::git_in_status(&agent, &["merge", "--no-edit", reference]);
    if code != 0 {
        // 非 session 文件的文本冲突：交回 git 处理，别硬来
        let _ = scope::git_in_status(&agent, &["merge", "--abort"]);
        bail!("git 层合并有文本冲突，先手动处理：\n{out}");
    }
    // merge 动了 Agent ref → 记一条 WorkspaceRevision(和 passthrough 的 git 一致;
    // 原生动词不该绕过"任一 ref 移动即配对"的契约)。失败只提示,不拖垮 reconcile。
    let after = scope::git_in_status(&agent, &["rev-parse", "HEAD"]).1;
    if after != before {
        if let Err(e) = crate::workspace::record(&crate::workspace::trigger_label(Scope::Agent, "merge")) {
            eprintln!("agit: 已合并，但生成 WorkspaceRevision 失败: {e:#}");
        }
    }

    if incoming.is_empty() {
        println!("{reference} 没带来新的 session（已是最新）。仍会基于本地会话刷新一次上下文。");
    } else {
        println!("并入 {} 条来自 {reference} 的 session。", incoming.len());
    }

    // 3. brief：本地已有 vs 对面带来
    let all = sessions_on_disk(&agent, &rt);
    let incoming_set: std::collections::HashSet<String> =
        incoming.iter().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()).collect();
    let (mut peer, mut local) = (Vec::new(), Vec::new());
    for p in &all {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let b = brief(p, &env);
        if let Some(b) = b {
            if incoming_set.contains(&name) {
                peer.push(b);
            } else {
                local.push(b);
            }
        }
    }

    // 4. 让 agent 合并
    let prompt = merge_prompt(&local, &peer);
    eprintln!("让 {} 读两边会话、合成统一上下文……", crate::llm::backend_name());
    let reply = crate::llm::ask(&prompt)?;
    let (context, conflicts) = split_reply(&reply);

    // 5. 写 CLAUDE.md
    let claude = crate::commands::write_claude_block(&env, &context)?;
    println!("统一上下文 → {}", claude.display());

    // 6. 只在真冲突时停下问人
    if conflicts.is_empty() {
        println!("没有需要你裁决的矛盾。下个会话直接带着这份上下文。");
        Ok(0)
    } else {
        println!("\n有 {} 处需要你裁决的矛盾：", conflicts.len());
        for (i, c) in conflicts.iter().enumerate() {
            println!("  {}. {}", i + 1, c);
        }
        println!("\n改定后编辑 CLAUDE.md 的受管区块，或重跑 reconcile。");
        Ok(1)
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
