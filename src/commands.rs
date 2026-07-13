//! agit 原生动词（在透传之外、需要 agit 加值的命令）。
//! v2 spine：先接 scan（hooks 需要）与 workspace。verify/why/new/resolve 的
//! Agent-Store 版本在下一步移植（见 docs/architecture-v2.md）。

use crate::adapter;
use crate::extract;
use crate::scan;
use crate::scope::{self, Scope};
use anyhow::Result;
use std::path::PathBuf;
use walkdir::WalkDir;

const FACTS_SUBDIR: &str = "state/facts";

// ─────────────────────── Adapter：runtime ↔ AgentState ───────────────────────

pub fn adapter_list() -> Result<i32> {
    println!("已注册的 runtime adapter：");
    for (name, desc) in adapter::list() {
        println!("  {name:<14} {desc}");
    }
    Ok(0)
}

/// `agit -a import [--from <runtime>] [--summarize] [<session>]`
/// 从 runtime session 抽取 AgentState 写进 Agent Store。用 adapter 的 export（runtime→AgentGit）。
/// `--summarize` 额外调本机 claude 把证据池归纳成 fact。
pub fn import_cmd(runtime: &str, session: Option<PathBuf>, summarize: bool) -> Result<i32> {
    let env_root = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let state = agent.join("state");

    let ad = adapter::get(runtime)?;
    let ir = ad.export(session.as_deref(), &env_root)?;
    let sum = extract::write_state(&ir, &state, &env_root)?;

    println!("从 {} session 抽取 AgentState：", ad.name());
    println!("  session   : {}", ir.session_id);
    if let Some(b) = &ir.git_branch {
        println!("  代码分支  : {b}");
    }
    println!("  目标      : {} 条 prompt", sum.prompts);
    println!("  证据池    : {} 条 file 证据已对齐基线，{} 条跳过", sum.reads_captured, sum.reads_skipped);
    println!("              {} 条命令（只记不跑）", sum.commands);
    println!("  artifact  : {} 个", sum.artifacts);
    println!();
    println!("写入 {}", state.display());

    if summarize {
        match crate::summarize::run(&ir, &env_root, &state.join("facts")) {
            Ok(n) => println!("  Summarizer：从证据池归纳出 {n} 条 fact（写入 state/facts/）"),
            Err(e) => eprintln!("  Summarizer 跳过：{e:#}"),
        }
    } else {
        println!("  证据池是 fact 的原材料。加 --summarize 让本机 claude 自动归纳，或用 agit -a new 手工提炼。");
    }
    println!("  审阅后：agit -a add -A && agit -a commit -m '导入 {} 的 context'", ir.session_id);
    Ok(0)
}

/// `agit -a export [--to <runtime>] [<out>]`
/// 把 AgentState 导出成 runtime 可复用的可移植摘要。用 adapter 的 import（AgentGit→runtime）。
pub fn export_cmd(runtime: &str, out: Option<PathBuf>) -> Result<i32> {
    let agent = scope::root_for(Scope::Agent)?;
    let state = agent.join("state");
    let out = out.unwrap_or_else(|| {
        scope::env_root()
            .map(|r| r.join(".agit/imported-context.md"))
            .unwrap_or_else(|_| PathBuf::from("imported-context.md"))
    });

    let ad = adapter::get(runtime)?;
    ad.import(&state, &out)?;
    println!("已导出 AgentState → {}", out.display());
    println!("  {} 可以把它作为一条 context 消息读入，一条命令复用他人的 context。", ad.name());
    Ok(0)
}

/// 扫描某个 scope 里的密钥。默认扫 Agent Store 的 facts；--staged 只扫暂存的。
pub fn scan_cmd(scope: Scope, staged: bool, paths: &[PathBuf]) -> Result<i32> {
    scan_root(&scope::root_for(scope)?, staged, paths)
}

/// hook 专用：扫描 cwd 所在的那个 git 仓库，不走 scope 发现。
/// pre-commit/pre-push 在 Agent Store 里运行，cwd 就是它，直接扫它。
pub fn hook_scan(staged: bool) -> Result<i32> {
    let (_, top) = scope::git_in_status(std::path::Path::new("."), &["rev-parse", "--show-toplevel"]);
    let root = if top.is_empty() {
        std::env::current_dir()?
    } else {
        PathBuf::from(top)
    };
    scan_root(&root, staged, &[])
}

fn scan_root(root: &std::path::Path, staged: bool, paths: &[PathBuf]) -> Result<i32> {
    let targets: Vec<PathBuf> = if !paths.is_empty() {
        paths.iter().map(|p| root.join(p)).collect()
    } else if staged {
        let (_, out) = scope::git_in_status(
            &root,
            &["diff", "--cached", "--name-only", "--diff-filter=ACM"],
        );
        out.lines().map(|p| root.join(p)).collect()
    } else {
        let facts = root.join(FACTS_SUBDIR);
        WalkDir::new(&facts)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .map(|e| e.path().to_path_buf())
            .collect()
    };

    let mut total = 0;
    for t in &targets {
        if !t.exists() {
            continue;
        }
        for f in scan::scan_file(t)? {
            if total == 0 {
                eprintln!("发现疑似密钥：");
            }
            eprintln!(
                "  {}:{}  [{}]  {}",
                t.strip_prefix(&root).unwrap_or(t).display(),
                f.line,
                f.rule,
                f.excerpt
            );
            total += 1;
        }
    }

    if total > 0 {
        eprintln!("\n{total} 处。AgentState 一旦 push，同事 pull 下来就带着它们。");
        eprintln!("修掉它。或者用 --no-verify 绕过这道 hook，显式承担后果。");
        return Ok(1);
    }
    if !staged {
        println!("扫描 {} 个文件，未发现密钥。", targets.len());
    }
    Ok(0)
}

/// `agit clone <url>` —— 把团队的 Agent Store 拉到 .agit/agent 并装好驱动/hook。
/// 消费他人 context 的一条命令：clone + init（幂等）。
pub fn clone_agent(url: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = env.join(scope::AGENT_DIR);
    if agent.join(".git").exists() {
        anyhow::bail!(
            "{} 已存在。要换成远端的 context，先移除它，或直接 agit -a pull。",
            agent.display()
        );
    }
    std::fs::create_dir_all(agent.parent().unwrap())?;
    let (code, _) = scope::git_in_status(
        &env,
        &["clone", "-q", url, &agent.to_string_lossy()],
    );
    if code != 0 {
        anyhow::bail!("git clone {url} 失败");
    }
    println!("已拉取 Agent Store ← {url}");
    // 装 driver / hook（init 幂等，会在已有 clone 上补装配置）
    crate::init::run()?;
    println!("\n看看拿到了什么： agit -a verify");
    Ok(0)
}

/// 打印当前 WorkspaceRevision（Agent↔Environment 配对）。
pub fn workspace_show() -> Result<i32> {
    let head = scope::workspace_dir()?.join("HEAD.json");
    if !head.exists() {
        println!("还没有 WorkspaceRevision。任一库 commit 后会自动生成。");
        return Ok(0);
    }
    println!("{}", std::fs::read_to_string(head)?);
    Ok(0)
}

pub fn workspace_log() -> Result<i32> {
    let log = scope::workspace_dir()?.join("log.jsonl");
    if !log.exists() {
        println!("还没有 WorkspaceRevision。");
        return Ok(0);
    }
    print!("{}", std::fs::read_to_string(log)?);
    Ok(0)
}
