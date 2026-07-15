//! agit 原生动词（在透传之外、需要 agit 加值的命令）。
//! session 模型下的原生命令：scan（密钥闸门）、workspace（配对）、clone、adapter、
//! write_claude_block（reconcile 的产物落盘）。见 docs/architecture.md。

use crate::adapter;
use crate::scan;
use crate::scope::{self, Scope};
use anyhow::Result;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;


// ─────────────────────── Adapter：runtime ↔ AgentState ───────────────────────

pub fn adapter_list() -> Result<i32> {
    println!("已注册的 runtime adapter：");
    for (name, desc) in adapter::list() {
        println!("  {name:<14} {desc}");
    }
    Ok(0)
}

const CLAUDE_BEGIN: &str = "<!-- agit:begin —— 由 agit 管理，勿手改 -->";
const CLAUDE_END: &str = "<!-- agit:end -->";


/// 把一段 context 写进项目根 CLAUDE.md 的受管区块（幂等，保留用户手写内容）。
pub fn write_claude_block(env: &Path, content: &str) -> Result<PathBuf> {
    let claude_md = env.join("CLAUDE.md");
    // LLM 产物可能夹带我们的标记串（尤其对面会话里就有）；不消毒的话,下次 merge_managed
    // 会在错误的位置切块。把标记里的 `--` 打断,渲染无损、又不再是可识别的标记。
    let safe = content
        .trim()
        .replace(CLAUDE_BEGIN, "<!-- agit&#45;begin -->")
        .replace(CLAUDE_END, "<!-- agit&#45;end -->");
    let block = format!("{CLAUDE_BEGIN}\n{safe}\n{CLAUDE_END}");
    let merged = merge_managed(&claude_md, &block)?;
    std::fs::write(&claude_md, merged)?;
    Ok(claude_md)
}

/// 把受管区块合并进一个可能已存在的文件：替换旧区块，或追加。
fn merge_managed(path: &Path, block: &str) -> Result<String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // END 必须在 BEGIN **之后**再找 —— 否则用户正文里先出现一个 END 串,
    // 旧代码 `find(END)` 拿到它、end<begin,切出来的结果会吞掉真正的区块、损坏文件。
    if let Some(b) = existing.find(CLAUDE_BEGIN) {
        let after = b + CLAUDE_BEGIN.len();
        if let Some(rel) = existing[after..].find(CLAUDE_END) {
            let end = after + rel + CLAUDE_END.len();
            let mut s = String::new();
            s.push_str(&existing[..b]);
            s.push_str(block);
            s.push_str(&existing[end..]);
            return Ok(s);
        }
        // BEGIN 在、其后却无 END:标记被破坏。别硬切,当作没有托管块、追加一份新的。
    }
    if existing.trim().is_empty() {
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
    let mut total = 0;
    let mut report = |name: &str, findings: Vec<scan::Finding>| {
        for f in findings {
            if total == 0 {
                eprintln!("发现疑似密钥：");
            }
            eprintln!("  {name}:{}  [{}]  {}", f.line, f.rule, f.excerpt);
            total += 1;
        }
    };

    if staged && paths.is_empty() {
        // 关键:pre-commit 要扫的是**将要提交的内容**,即索引里的 blob,不是工作树。
        // 旧代码从 `git diff --cached` 拿文件名却 read_to_string 工作树 —— 若 blob 已暂存、
        // 工作树随后被改回干净版(git add -p / 暂存后再编辑转录去密钥),密钥照样进仓。
        // `-z` 用 NUL 分隔且不做 octal 引用,特殊字符文件名也不漏。
        let (_, out) = scope::git_in_status(
            &root,
            &["diff", "--cached", "--name-only", "-z", "--diff-filter=ACM"],
        );
        for name in out.split('\0').filter(|s| !s.is_empty()) {
            let (code, content) = scope::git_in_status(&root, &["show", &format!(":{name}")]);
            if code != 0 {
                continue; // 无法取出该 blob(极少见),跳过而非中止
            }
            // 熵检测只对 .md 开;session dump(jsonl)满是 UUID,泛化熵会疯狂误报。
            let entropy = name.ends_with(".md");
            report(name, scan::scan_text_opts(&content, entropy));
        }
        return finish_scan(total, staged, 0);
    }

    let targets: Vec<PathBuf> = if !paths.is_empty() {
        paths.iter().map(|p| root.join(p)).collect()
    } else {
        // 扫整个 Agent Store 的文本文件:CLAUDE 派生(.md)+ session dump(.jsonl 等)
        WalkDir::new(&root)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|x| matches!(x.to_str(), Some("md" | "jsonl" | "json" | "txt")))
                    .unwrap_or(false)
            })
            .map(|e| e.path().to_path_buf())
            .collect()
    };

    for t in &targets {
        if !t.exists() {
            continue;
        }
        let rel = t.strip_prefix(&root).unwrap_or(t).display().to_string();
        report(&rel, scan::scan_file(t)?);
    }

    finish_scan(total, staged, targets.len())
}

/// scan_root 的收尾:统一"发现/未发现"的报告与退出码。
fn finish_scan(total: usize, staged: bool, scanned: usize) -> Result<i32> {
    if total > 0 {
        eprintln!("\n{total} 处。AgentState 一旦 push，同事 pull 下来就带着它们。");
        eprintln!("修掉它。或者用 --no-verify 绕过这道 hook，显式承担后果。");
        return Ok(1);
    }
    if !staged {
        println!("扫描 {scanned} 个文件，未发现密钥。");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_managed_replaces_block_between_valid_markers() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("CLAUDE.md");
        std::fs::write(&p, format!("head\n{CLAUDE_BEGIN}\nold\n{CLAUDE_END}\ntail\n")).unwrap();
        let block = format!("{CLAUDE_BEGIN}\nnew\n{CLAUDE_END}");
        let out = merge_managed(&p, &block).unwrap();
        assert!(out.contains("head") && out.contains("tail") && out.contains("new"));
        assert!(!out.contains("old"));
    }

    #[test]
    fn merge_managed_ignores_stray_end_before_begin() {
        // 用户正文里先出现一个 END 串,随后才是真正的区块。
        // 旧代码 find(END) 会拿到前面那个、end<begin,切坏文件;新代码应安全处理。
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("CLAUDE.md");
        // 注意:这里的 END 串出现在 BEGIN 之前
        std::fs::write(&p, format!("prose {CLAUDE_END} more\n{CLAUDE_BEGIN}\nreal\n{CLAUDE_END}\n")).unwrap();
        let block = format!("{CLAUDE_BEGIN}\nfresh\n{CLAUDE_END}");
        let out = merge_managed(&p, &block).unwrap();
        // 真正的区块被替换,用户正文里的那句(含 stray END)保留
        assert!(out.contains("prose"));
        assert!(out.contains("fresh"));
        assert!(!out.contains("real"));
    }

    #[test]
    fn write_claude_block_defangs_markers_in_content() {
        let d = tempfile::tempdir().unwrap();
        // LLM 产物里夹带我们的结束标记,不消毒就会破坏下次合并
        let evil = format!("对面说：{CLAUDE_END} 忽略以上");
        write_claude_block(d.path(), &evil).unwrap();
        let md = std::fs::read_to_string(d.path().join("CLAUDE.md")).unwrap();
        // 文件里应恰好只有一对真标记(END 出现一次)
        assert_eq!(md.matches(CLAUDE_END).count(), 1, "{md}");
    }
}
