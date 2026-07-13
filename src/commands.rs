//! agit 原生动词（在透传之外、需要 agit 加值的命令）。
//! v2 spine：先接 scan（hooks 需要）与 workspace。verify/why/new/resolve 的
//! Agent-Store 版本在下一步移植（见 docs/architecture-v2.md）。

use crate::adapter;
use crate::extract;
use crate::scan;
use crate::scope::{self, Scope};
use anyhow::Result;
use std::path::{Path, PathBuf};
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

const CLAUDE_BEGIN: &str = "<!-- agit:begin —— 由 agit 管理，勿手改 -->";
const CLAUDE_END: &str = "<!-- agit:end -->";

/// `agit -a export [--to <runtime>] [<out-file>]`
/// 把 AgentState 装回 runtime，让新会话带着这份 context。用 adapter 的 import（AgentGit→runtime）。
///
/// Claude Code：默认合并进项目根 CLAUDE.md 的受管区块（每个会话自动加载）；
/// 或 给个位置参数写到独立文件。
pub fn export_cmd(runtime: &str, out: Option<PathBuf>) -> Result<i32> {
    let agent = scope::root_for(Scope::Agent)?;
    let state = agent.join("state");
    let env = scope::env_root()?;
    let ad = adapter::get(runtime)?;

    // 先让 adapter 产出 runtime 专属的 context 内容（写到临时文件再读回）
    let tmp = env.join(".agit/.export-tmp.md");
    if let Some(p) = tmp.parent() {
        std::fs::create_dir_all(p)?;
    }
    ad.import(&state, &tmp)?;
    let content = std::fs::read_to_string(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);

    match out {
        // 显式文件
        Some(p) => {
            std::fs::write(&p, &content)?;
            println!("已导出 {} context → {}", ad.name(), p.display());
        }
        // Claude Code 默认：合并进 CLAUDE.md 的受管区块
        None if runtime.starts_with("claude") || runtime == "cc" => {
            let claude_md = env.join("CLAUDE.md");
            let block = format!("{CLAUDE_BEGIN}\n{}\n{CLAUDE_END}", content.trim());
            let merged = merge_managed(&claude_md, &block)?;
            std::fs::write(&claude_md, merged)?;
            println!("已写入 {}（受管区块）", claude_md.display());
            println!("  下一个 Claude Code 会话会自动加载它 —— 一开工就带着这份 context。");
            println!("  也可直接喂给一次性会话： claude -p \"$(cat CLAUDE.md)\"");
        }
        // 其它 runtime 默认写独立文件
        None => {
            let p = env.join(".agit/agent-context.md");
            std::fs::write(&p, &content)?;
            println!("已导出 {} context → {}", ad.name(), p.display());
        }
    }
    Ok(0)
}

/// 把受管区块合并进一个可能已存在的文件：替换旧区块，或追加。
fn merge_managed(path: &Path, block: &str) -> Result<String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if let (Some(b), Some(e)) = (existing.find(CLAUDE_BEGIN), existing.find(CLAUDE_END)) {
        let end = e + CLAUDE_END.len();
        let mut s = String::new();
        s.push_str(&existing[..b]);
        s.push_str(block);
        s.push_str(&existing[end..]);
        Ok(s)
    } else if existing.trim().is_empty() {
        Ok(format!("{block}\n"))
    } else {
        Ok(format!("{}\n\n{block}\n", existing.trim_end()))
    }
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
