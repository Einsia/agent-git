//! Raw session dump management (new model: don't distill facts, version the agent's full session directly).
//!
//! Claude Code dumps its entire session into ~/.claude/projects/<slug>/ on its own:
//!   <uuid>.jsonl              full transcript
//!   <uuid>/subagents/*.jsonl  subagent transcripts
//!   <uuid>/tool-results/*.txt large tool results
//!   memory/                   memory
//! `agit -a sync` mirrors this blob into the Agent Store's sessions/<runtime>/, after which commit/push/pull work as usual.

use crate::adapter::claude_code;
use crate::scope::{self, Scope};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const SESSIONS_SUBDIR: &str = "sessions";

/// Locate the runtime session dump directory for the current project.
fn source_dir(runtime: &str, cwd: &Path) -> Result<PathBuf> {
    match runtime {
        "claude-code" | "claude" | "cc" => {
            let dir = claude_code::projects_dir()?.join(claude_code::slug_for(cwd));
            if !dir.exists() {
                bail!(
                    "Could not find the Claude Code session directory for this project: {}\n\
                     (has this project not been run in Claude Code yet?)",
                    dir.display()
                );
            }
            Ok(dir)
        }
        other => bail!("session dump for runtime `{other}` isn't wired up yet (see src/session.rs)"),
    }
}

/// `agit -a sync [--from <runtime>]` — mirror the runtime's session dump into the Agent Store.
pub fn sync(runtime: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let dst = agent.join(SESSIONS_SUBDIR).join(&rt);
    std::fs::create_dir_all(&dst)?;

    // Runtimes differ in storage model: Claude splits directories by project slug (mirror the whole tree); Codex splits by date,
    // with all projects mixed together (filter this project's rollouts by session_meta.cwd, mirror only those).
    let (stats, source_desc) = match rt.as_str() {
        "claude-code" => {
            let src = source_dir(runtime, &env)?;
            (mirror(&src, &dst)?, src.display().to_string())
        }
        "codex" => codex_collect(&env, &dst)?,
        other => bail!("session dump for runtime `{other}` isn't wired up yet (see src/session.rs)"),
    };

    // Scan for secrets before writing to disk — dumping every session means everything the agent has cat'd is in here
    let hits = crate::scan::scan_tree(&dst)?;

    println!("Mirrored the session dump for {}:", rt);
    println!("  source : {source_desc}");
    println!("  target : {}", dst.display());
    println!("  files  : {} files ({} updated / {} added), {} bytes", stats.total, stats.updated, stats.added, stats.bytes);
    if hits > 0 {
        eprintln!("  ⚠ Found {hits} likely secrets — the session transcript carries sensitive content the agent has seen.");
        eprintln!("     This will be blocked again before push; run `agit -a scan` first to check, or clear it from the transcript.");
    }
    println!("\n  Commit: agit -a add -A && agit -a commit -m 'sync {rt} sessions'");
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

/// Codex sync: scan ~/.codex/sessions and flatten only **this project's** rollouts
/// (session_meta.cwd == env root) into dst/<id>.jsonl. Filtering by cwd is a privacy
/// bottom line — never pull in another project's sessions.
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
    let desc = format!("{root} (cwd={} matched {} rollouts)", env.display(), rollouts.len());
    Ok((st, desc))
}

/// Recursively mirror src → dst (decide whether to overwrite by size + mtime only, which is good enough).
fn mirror(src: &Path, dst: &Path) -> Result<Stats> {
    let mut st = Stats { total: 0, added: 0, updated: 0, bytes: 0 };
    mirror_into(src, dst, &mut st)?;
    Ok(st)
}

fn mirror_into(src: &Path, dst: &Path, st: &mut Stats) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
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
                    // Re-copy if size **or** mtime changed. Checking size alone would miss same-length in-place edits
                    // (and would contradict this function's "size + mtime" comment); when mtime is unavailable, re-copy conservatively.
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
