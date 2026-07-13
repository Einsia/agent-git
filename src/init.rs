//! `agit init` —— 在当前代码仓库旁边建起 Agent Store 与配对基建。

use crate::scope::{self, AGENT_DIR};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

const GITATTR: &str = "state/facts/** merge=agit";

pub fn run() -> Result<i32> {
    let env = scope::env_root().context("agit init 需要在一个 git 仓库（你的代码仓库）里运行")?;
    let agent = env.join(AGENT_DIR);

    // 1. Environment 侧：把 .agit/ 挡在代码历史之外
    ensure_gitignore(&env)?;

    // 2. 建 Agent Store（独立 git 仓库）
    let fresh = !agent.join(".git").exists();
    if fresh {
        std::fs::create_dir_all(&agent)?;
        git(&agent, &["init", "-q", "-b", "main"])?;
        // 让 agent 提交有个默认身份，独立于代码仓库
        let _ = git(&agent, &["config", "user.name", "agit"]);
        let _ = git(&agent, &["config", "user.email", "agit@local"]);
        scaffold_state(&agent)?;
    }

    // 3. AgentState 的合并驱动 —— 注册在 Agent Store 上（不是代码仓库）
    let exe = std::env::current_exe().context("无法定位 agit 自身路径")?;
    git(&agent, &["config", "merge.agit.name", "agit fact merge (evidence-aware)"])?;
    git(
        &agent,
        &[
            "config",
            "merge.agit.driver",
            &format!("{} merge-file %O %A %B %P", exe.display()),
        ],
    )?;
    install_hook(&agent, "pre-commit", &exe, "hook-scan --staged")?;
    install_hook(&agent, "pre-push", &exe, "hook-scan")?;

    if fresh {
        git(&agent, &["add", "-A"])?;
        git(&agent, &["commit", "-q", "-m", "agit: 初始化 Agent Store"])?;
    }

    println!("agit 已就绪。");
    println!("  Environment : {}", env.display());
    println!("  Agent Store : {}", agent.display());
    println!();
    println!("  agit <git-args>       在代码仓库上跑 git（透明）");
    println!("  agit -a <git-args>    在 Agent Store 上跑同构操作");
    println!("  agit -a status        看 AgentState 的变化");
    Ok(0)
}

fn ensure_gitignore(env: &Path) -> Result<()> {
    let gi = env.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".agit/" || l.trim() == ".agit") {
        return Ok(());
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(".agit/\n");
    std::fs::write(&gi, s)?;
    println!("  代码仓库 .gitignore 追加: .agit/");
    Ok(())
}

fn scaffold_state(agent: &Path) -> Result<()> {
    std::fs::write(agent.join(".gitattributes"), format!("{GITATTR}\n"))?;
    std::fs::write(
        agent.join("agent.toml"),
        "# Agent 身份：id、rules、skill、tool contract\nid = \"unnamed-agent\"\n",
    )?;
    let state = agent.join("state");
    std::fs::create_dir_all(state.join("facts"))?;
    std::fs::write(state.join("goals.md"), "# 目标\n")?;
    std::fs::write(state.join("constraints.md"), "# 约束\n")?;
    std::fs::write(state.join("progress.md"), "# 进度\n")?;
    std::fs::write(state.join("artifacts.md"), "# Artifact 引用\n")?;
    std::fs::write(state.join("facts/.gitkeep"), "")?;
    Ok(())
}

fn install_hook(agent: &Path, name: &str, exe: &Path, args: &str) -> Result<()> {
    let hooks = agent.join(".git/hooks");
    std::fs::create_dir_all(&hooks)?;
    let p = hooks.join(name);
    std::fs::write(
        &p,
        format!("#!/bin/sh\n# installed by agit\nexec \"{}\" {}\n", exe.display(), args),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} 失败: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}
