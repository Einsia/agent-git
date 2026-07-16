//! `agit -a sync <ref>` —— 用**对话**合并两个分叉的 agent 分支。
//!
//! 不做结构化提炼、不写 CLAUDE.md。做法（spike 已验证，见 docs/plans/2026-07-16-...）：
//!   1. 三点 diff 求对面相对共同祖先新增的 session（分叉尾巴）。
//!   2. 把两边"最新"的 session 各复制成一份**新 id、绑到本仓库**的可 resume 会话
//!      （走 convert 机制改写 id/cwd —— 绝不动用户的真实会话）。
//!   3. 让两个 agent **只读**地在本仓库里对话：各自带完整上下文，互报改动、就分叉尾巴对账，
//!      能读代码自证的就自己解决，真冲突才拎出来给人。
//!   4. 产物：A 侧那份会话已经含了整段对账 → 就是可 resume 的合并态；对话全文另存一份做溯源。
//!
//! MVP：两边都 claude-code；每侧取"最新一条" session 代表该 agent 的最新上下文。

use crate::convo::{self, ConvertOpts};
use crate::scope::{self, Scope};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// 对话最多几轮（一轮 = B 回一次 + A 回一次）。deliberate 操作，够用即可。
const MAX_ROUNDS: usize = 4;

fn normalize(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
}

pub fn run(reference: &str, runtime: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    if rt != "claude-code" {
        bail!("sync 目前只支持 claude-code 两边（codex 侧待接）。");
    }
    if !which("claude") {
        bail!("sync 要本机有 `claude`（对话的双方都是真 resume 的 claude 会话）。");
    }

    // 0. ref 存在？
    if scope::git_in_status(&agent, &["rev-parse", "--verify", "--quiet", reference]).0 != 0 {
        bail!("Agent Store 里没有引用 `{reference}`。先 `agit -a fetch <remote>`。");
    }

    // 1. 分叉尾巴：对面相对共同祖先新增的 session（三点 diff）。
    let sdir = format!("sessions/{rt}");
    let (_, diff) =
        scope::git_in_status(&agent, &["diff", "--name-only", &format!("HEAD...{reference}"), "--", &sdir]);
    let incoming: Vec<String> = diff.lines().filter(|l| l.ends_with(".jsonl")).map(String::from).collect();
    if incoming.is_empty() {
        println!("{reference} 没有相对共同祖先新增的 session —— 无需 sync。");
        return Ok(0);
    }

    // 2. 各取一侧代表：A = 本地最新（非对面带来）的 session；B = 对面最新的 incoming。
    let a_path = latest_local_session(&agent, &rt, &incoming)
        .context("本地没有可代表 A 的 session（本分支还没自己的会话？）")?;
    let a_src = std::fs::read_to_string(&a_path)?;
    let b_rel = incoming.last().unwrap();
    let (rcb, b_src) = scope::git_in_status(&agent, &["show", &format!("{reference}:{b_rel}")]);
    if rcb != 0 || b_src.trim().is_empty() {
        bail!("读不到对面 session `{b_rel}`。");
    }

    // 3. 复制成"新 id + 绑到本仓库"的可 resume 会话（走 convert 改写 id/cwd，不碰真实会话）。
    let a_id = convo::fresh_id("sync-a");
    let b_id = convo::fresh_id("sync-b");
    install_copy(&rt, &a_src, &a_id, &env)?;
    install_copy(&rt, &b_src, &b_id, &env)?;
    eprintln!(
        "复活两侧会话对话（只读本仓库）：A={} … B={} …",
        &a_id[..8],
        &b_id[..8]
    );

    // 4. 对话循环。A 先开口，之后 B↔A 往返，直到两边都 DONE 或到轮数上限。
    let mut transcript: Vec<(char, String)> = Vec::new();
    let mut msg = turn(&env, &a_id, &open_prompt())?;
    println!("\nA → {msg}\n");
    transcript.push(('A', msg.clone()));

    for _ in 0..MAX_ROUNDS {
        let b = turn(&env, &b_id, &relay_prompt(&msg))?;
        println!("B → {b}\n");
        transcript.push(('B', b.clone()));
        let a = turn(&env, &a_id, &relay_prompt(&b))?;
        println!("A → {a}\n");
        transcript.push(('A', a.clone()));
        msg = a;
        if is_done(&b) && is_done(&msg) {
            break;
        }
    }

    // 5. 收敛综述：从对话里拎出「已达成」与「仍需人裁决」。
    let (resolved, open) = synthesize(&transcript)?;

    // 6. 产物：溯源存档 + 合并态（A 侧会话已含对账，直接 resume）。
    let archive = save_transcript(&agent, &rt, &a_id, &b_id, &transcript)?;
    println!("── sync 结果 ──");
    if !resolved.is_empty() {
        println!("已达成：\n{resolved}");
    }
    println!("\n对话存档 → {}", archive.display());
    println!("合并态（含两边上下文 + 对账）可直接续跑：");
    println!("  (cd {} && claude --resume {a_id})", env.display());

    if open.trim().is_empty() {
        println!("\n没有需要你裁决的冲突。");
        Ok(0)
    } else {
        println!("\n仍需你裁决：\n{open}");
        Ok(1)
    }
}

/// 把一段 claude 会话内容复制成新 id + cwd=env 的可 resume 会话（同 vendor 重放，改写 id/cwd）。
fn install_copy(rt: &str, src: &str, new_id: &str, env: &Path) -> Result<()> {
    let ir = convo::read_conversation(rt, src)?;
    let opts = ConvertOpts { cwd: Some(env.to_string_lossy().into_owned()), new_id: new_id.to_string() };
    let bytes = convo::write_conversation(rt, &ir, &opts)?;
    crate::register::install(rt, new_id, env, &bytes)?;
    Ok(())
}

/// 让一个 resume 的 claude 会话走一轮（只读工具），返回它这轮的文本回复。
fn turn(env: &Path, session_id: &str, prompt: &str) -> Result<String> {
    let out = Command::new("claude")
        .current_dir(env)
        .args([
            "--resume",
            session_id,
            "-p",
            prompt,
            "--output-format",
            "json",
            "--allowedTools",
            "Read",
            "Grep",
            "Glob",
        ])
        .output()
        .context("启动 claude 失败（它在 PATH 里吗？）")?;
    if !out.status.success() {
        bail!("claude --resume 返回非零：{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // --output-format json → {..,"result":"…","session_id":"…"}
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
            return Ok(r.trim().to_string());
        }
    }
    Ok(text.trim().to_string())
}

const RULES: &str = "规则：每轮≤4句；真冲突（两者不能同时成立、或合并会坏）另起一行以 'CONFLICT:' 开头；\
你有本仓库的**只读**权限（Read/Grep/Glob），能读代码自证就别靠记忆猜；没有要提或已对完，结尾写 'DONE'。";

fn open_prompt() -> String {
    format!(
        "你是 agent A。你和另一个 agent B 从同一起点、在各自分支上改了这个仓库，现在要合并。\
         请和 B 对账，找出真冲突。{RULES}\n\n开场：简述你 fork 之后改了什么，并问 B 改了什么，好据此查冲突。"
    )
}

fn relay_prompt(other: &str) -> String {
    format!("对面 agent 说：\"{other}\"\n\n按同样规则回应。{RULES}")
}

fn is_done(msg: &str) -> bool {
    let tail: String = msg.trim_end().chars().rev().take(12).collect::<String>().chars().rev().collect();
    tail.contains("DONE")
}

/// 从对话综述出 (已达成, 仍需裁决)。用一次性 LLM（llm.rs 后端）读全文。
fn synthesize(transcript: &[(char, String)]) -> Result<(String, String)> {
    let convo_text: String =
        transcript.iter().map(|(who, m)| format!("{who}: {m}")).collect::<Vec<_>>().join("\n\n");
    let prompt = format!(
        "下面是两个 agent 合并分支时的对账对话。给人读，输出两段：\n\
         【已达成】它们达成一致的决定（一行一条；没有就写「无」）。\n\
         【仍需裁决】还需要人拍板的冲突（一行一条；没有就写「无」）。\n\
         精炼，别复述整段对话。\n\n{convo_text}"
    );
    if !crate::llm::available() {
        // 无 LLM 后端：退回从标记里机械抽取。
        let open: Vec<String> = transcript
            .iter()
            .flat_map(|(_, m)| m.lines())
            .filter(|l| l.trim_start().starts_with("CONFLICT:"))
            .map(|l| format!("- {}", l.trim_start().trim_start_matches("CONFLICT:").trim()))
            .collect();
        return Ok((String::from("（无 LLM 后端，未综述）"), open.join("\n")));
    }
    let reply = crate::llm::ask(&prompt)?;
    Ok(split_sections(&reply))
}

/// 把综述回复拆成 (已达成, 仍需裁决) 两段（按中文小标题）。
fn split_sections(reply: &str) -> (String, String) {
    let open_marker = reply.find("【仍需裁决】");
    match open_marker {
        Some(i) => {
            let resolved = reply[..i].replace("【已达成】", "").trim().to_string();
            let open = reply[i..].replace("【仍需裁决】", "").trim().to_string();
            let open = if open.contains('无') && open.chars().filter(|c| !c.is_whitespace()).count() <= 2 {
                String::new()
            } else {
                open
            };
            (resolved, open)
        }
        None => (reply.trim().to_string(), String::new()),
    }
}

/// 把对话全文存进 Agent Store（版本化的溯源：这两个 agent 是**怎么**对齐的）。
fn save_transcript(agent: &Path, rt: &str, a_id: &str, b_id: &str, transcript: &[(char, String)]) -> Result<PathBuf> {
    let dir = agent.join("sessions").join("sync");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{}.md", &a_id[..8], &b_id[..8]));
    let mut md = format!("# sync 对账（{rt}）\n\nA={a_id}\nB={b_id}\n\n");
    for (who, m) in transcript {
        md.push_str(&format!("**{who}:** {m}\n\n"));
    }
    std::fs::write(&path, md)?;
    Ok(path)
}

/// 本地最新（非对面带来）的 session 文件。incoming 是对面带来的相对路径集合。
fn latest_local_session(agent: &Path, rt: &str, incoming: &[String]) -> Option<PathBuf> {
    let incoming_names: std::collections::HashSet<&str> =
        incoming.iter().map(|p| p.rsplit('/').next().unwrap_or(p)).collect();
    let dir = agent.join("sessions").join(rt);
    walkdir::WalkDir::new(&dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            !incoming_names.contains(name.as_ref())
        })
        .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .map(|e| e.path().to_path_buf())
}

fn which(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
