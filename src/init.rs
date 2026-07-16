//! `agit init` — sets up the Agent Store and pairing infrastructure alongside the current code repo.

use crate::scope::{self, AGENT_DIR};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub fn run() -> Result<i32> {
    let env = scope::env_root().context("agit init must be run inside a git repository (your code repo)")?;
    let agent = env.join(AGENT_DIR);

    // 1. Environment side: keep .agit/ out of the code history
    ensure_gitignore(&env)?;

    // 2. Build the Agent Store (a standalone git repo) to hold session dumps
    let fresh = !agent.join(".git").exists();
    if fresh {
        std::fs::create_dir_all(&agent)?;
        git(&agent, &["init", "-q", "-b", "main"])?;
        let _ = git(&agent, &["config", "user.name", "agit"]);
        let _ = git(&agent, &["config", "user.email", "agit@local"]);
        scaffold(&agent)?;
    }

    // 3. Secret-scanning hook — dumping every session means the transcripts may carry secrets the agent has seen
    let exe = std::env::current_exe().context("could not locate agit's own path")?;
    install_hook(&agent, "pre-commit", &exe, "hook-scan --staged")?;
    install_hook(&agent, "pre-push", &exe, "hook-scan")?;

    if fresh {
        git(&agent, &["add", "-A"])?;
        git(&agent, &["commit", "-q", "-m", "agit: initialize Agent Store"])?;
    }

    println!("agit is ready.");
    println!("  Environment : {}", env.display());
    println!("  Agent Store : {}", agent.display());
    println!();
    println!("  agit -a snap            mirror this project's session dump in (add --watch to auto-snap continuously)");
    println!("  agit -a push / pull     sync sessions with your team");
    println!("  agit -a merge <ref>      merge this branch's agent with the other side's conversation (only asks on real conflicts)");
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
    println!("  appended to the code repo .gitignore: .agit/");
    Ok(())
}

fn scaffold(agent: &Path) -> Result<()> {
    std::fs::write(
        agent.join("agent.toml"),
        "# Agent identity\nid = \"unnamed-agent\"\n",
    )?;
    std::fs::create_dir_all(agent.join("sessions"))?;
    std::fs::write(agent.join("sessions/.gitkeep"), "")?;
    Ok(())
}

/// POSIX sh single-quote escaping: the only dangerous character is `'` itself; break out with `'\''` and rejoin.
/// Double quotes wouldn't stop `$` / backticks / `"` in a path; inside single quotes these are all literal.
fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn install_hook(agent: &Path, name: &str, exe: &Path, args: &str) -> Result<()> {
    let hooks = agent.join(".git/hooks");
    std::fs::create_dir_all(&hooks)?;
    let p = hooks.join(name);
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\n# installed by agit\nexec {} {}\n",
            sh_single_quote(&exe.to_string_lossy()),
            args
        ),
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
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}
