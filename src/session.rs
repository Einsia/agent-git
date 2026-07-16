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

/// `agit -a snap [--from <runtime>]` — mirror the runtime's session dump into the Agent Store, once.
pub fn sync(runtime: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let (stats, source_desc, hits, dst) = mirror_once(&rt, &env, &agent)?;

    println!("Mirrored the session dump for {}:", rt);
    println!("  source : {source_desc}");
    println!("  target : {}", dst.display());
    println!("  files  : {} files ({} updated / {} added), {} bytes", stats.total, stats.updated, stats.added, stats.bytes);
    if hits > 0 {
        eprintln!("  ⚠ Found {hits} likely secrets — the session transcript carries sensitive content the agent has seen.");
        eprintln!("     This will be blocked again before push; run `agit -a scan` first to check, or clear it from the transcript.");
    }
    println!("\n  Commit: agit -a add -A && agit -a commit -m 'snap {rt} sessions'");
    Ok(0)
}

/// The mirror step shared by one-shot `snap` and the `--watch` loop: copy the runtime's dump into the
/// Agent Store and secret-scan it. Returns (stats, human source description, secret hits, destination dir).
fn mirror_once(rt: &str, env: &Path, agent: &Path) -> Result<(Stats, String, usize, PathBuf)> {
    let dst = agent.join(SESSIONS_SUBDIR).join(rt);
    std::fs::create_dir_all(&dst)?;
    // Runtimes differ in storage model: Claude splits directories by project slug (mirror the whole tree);
    // Codex splits by date with all projects mixed (filter this project's rollouts by session_meta.cwd).
    let (stats, source_desc) = match rt {
        "claude-code" => {
            let src = source_dir(rt, env)?;
            (mirror(&src, &dst)?, src.display().to_string())
        }
        "codex" => codex_collect(env, &dst)?,
        other => bail!("session dump for runtime `{other}` isn't wired up yet (see src/session.rs)"),
    };
    let hits = crate::scan::scan_tree(&dst)?;
    Ok((stats, source_desc, hits, dst))
}

/// `agit -a snap --watch [--interval N]` — **fully automatic snap**: watch the runtime's session dump and,
/// whenever it changes and then settles, mirror + auto-commit into the Agent Store. Runs until Ctrl-C.
/// Runtime-agnostic; the pre-commit secret hook still applies (a snap carrying a secret is refused, with a warning).
pub fn snap_watch(runtime: &str, interval_secs: u64) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    let interval = std::time::Duration::from_secs(interval_secs.max(1));
    let watch = source_path(&rt, &env);

    println!("Auto-snapping {rt} on every change (settling window {interval_secs}s). Ctrl-C to stop.");
    if watch.as_deref().map(|p| !p.exists()).unwrap_or(true) {
        println!("  (waiting for {rt} sessions to appear…)");
    }

    let mut last_sig = String::new();
    let mut pending = false;
    let mut count: u64 = 0;
    loop {
        let sig = watch.as_deref().map(dir_signature).unwrap_or_default();
        if sig != last_sig {
            // changed since last check → wait one more tick for it to settle (debounce a burst of edits)
            pending = true;
            last_sig = sig;
        } else if pending {
            match mirror_once(&rt, &env, &agent) {
                Ok((stats, _, hits, _)) if stats.added + stats.updated > 0 => commit_snap(&agent, &rt, hits, &mut count),
                Ok(_) => {}
                Err(e) => eprintln!("  snap failed: {e:#}"),
            }
            pending = false;
        }
        std::thread::sleep(interval);
    }
}

/// Stage + commit the mirrored dump. Nothing staged → no-op. Commit blocked by the pre-commit secret hook → warn.
fn commit_snap(agent: &Path, rt: &str, hits: usize, count: &mut u64) {
    let _ = scope::git_in_status(agent, &["add", "-A"]);
    // `diff --cached --quiet` exits 1 when something is staged, 0 when nothing is.
    if scope::git_in_status(agent, &["diff", "--cached", "--quiet"]).0 == 0 {
        return;
    }
    let ts = now_iso();
    let (rc, _) = scope::git_in_status(agent, &["commit", "-m", &format!("auto-snap {rt} {ts}")]);
    if rc == 0 {
        *count += 1;
        println!("  ● snapped {ts}  (#{count})");
    } else {
        eprintln!(
            "  ⚠ auto-snap not committed{} — mirrored to disk but the pre-commit hook refused it. `agit -a scan` to see.",
            if hits > 0 { " (likely secrets)" } else { "" }
        );
        let _ = scope::git_in_status(agent, &["reset", "-q"]); // unstage so we don't spin on it
    }
}

/// Where a runtime's session dump for this project lives (no existence check — the watcher waits for it).
fn source_path(rt: &str, env: &Path) -> Option<PathBuf> {
    match rt {
        "claude-code" => claude_code::projects_dir().ok().map(|d| d.join(claude_code::slug_for(env))),
        "codex" => crate::adapter::codex::sessions_root().ok(),
        _ => None,
    }
}

/// A cheap change signature of a directory tree: sorted (path, size, mtime) of every file.
fn dir_signature(dir: &Path) -> String {
    let mut parts: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            let mt = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
            Some(format!("{}:{}:{mt}", e.path().display(), m.len()))
        })
        .collect();
    parts.sort();
    parts.join("|")
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
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
