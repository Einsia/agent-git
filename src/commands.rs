//! agit native verbs (commands beyond passthrough where agit adds value).
//! Native commands under the session model: scan (secret gate), workspace (pairing), clone, adapter, convert.
//! See docs/architecture.md.

use crate::adapter;
use crate::scan;
use crate::scope::{self, Scope};
use anyhow::Result;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;


// ─────────────────────── Adapter: runtime ↔ AgentState ───────────────────────

pub fn adapter_list() -> Result<i32> {
    println!("Registered runtime adapters:");
    for (name, desc) in adapter::list() {
        println!("  {name:<14} {desc}");
    }
    Ok(0)
}

/// Scan for secrets within a scope. Scans the Agent Store's facts by default; --staged scans only what is staged.
pub fn scan_cmd(scope: Scope, staged: bool, paths: &[PathBuf]) -> Result<i32> {
    scan_root(&scope::root_for(scope)?, staged, paths)
}

/// hook-only: scan the git repo that cwd lives in, without going through scope discovery.
/// pre-commit/pre-push run inside the Agent Store, so cwd is it, and we scan it directly.
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
    let allow = scan::Allowlist::load(root);
    let mut total = 0;
    let mut report = |name: &str, findings: Vec<scan::Finding>| {
        for f in findings {
            if total == 0 {
                eprintln!("Found suspected secrets:");
            }
            eprintln!("  {name}:{}  [{}]  {}", f.line, f.rule, f.excerpt);
            total += 1;
        }
    };

    if staged && paths.is_empty() {
        // Key point: pre-commit must scan **what is about to be committed**, i.e. the blob in the index, not the working tree.
        // The old code took the filename from `git diff --cached` but read_to_string'd the working tree -- if the blob is staged
        // and the working tree is then reverted to a clean version (git add -p / editing the transcript to strip the secret after staging), the secret still lands in the repo.
        // `-z` separates with NUL and does no octal quoting, so filenames with special characters aren't missed either.
        let (_, out) = scope::git_in_status(
            &root,
            &["diff", "--cached", "--name-only", "-z", "--diff-filter=ACM"],
        );
        for name in out.split('\0').filter(|s| !s.is_empty()) {
            let (code, content) = scope::git_in_status(&root, &["show", &format!(":{name}")]);
            if code != 0 {
                continue; // can't extract this blob (very rare), skip rather than abort
            }
            // Entropy is on for everything now: jsonl is parsed and only STRING VALUES are scanned,
            // with shape allowlists, so UUIDs/paths/requestIds no longer drown the signal (see scan.rs).
            report(name, scan::scan_text_allow(&content, true, &allow));
        }
        return finish_scan(total, staged, 0);
    }

    let targets: Vec<PathBuf> = if !paths.is_empty() {
        paths.iter().map(|p| root.join(p)).collect()
    } else {
        // Scan EVERY file in the Agent Store: an extension gate skipped .env/.pem/.key/.sh/.yaml and
        // extensionless files, which hold secrets just as well. Binaries are detected by content in scan_file.
        WalkDir::new(&root)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .collect()
    };

    for t in &targets {
        if !t.exists() {
            continue;
        }
        let rel = t.strip_prefix(&root).unwrap_or(t).display().to_string();
        report(&rel, scan::scan_file_allow(t, &allow)?);
    }

    finish_scan(total, staged, targets.len())
}

/// scan_root wrap-up: unifies the "found/not found" report and exit code.
fn finish_scan(total: usize, staged: bool, scanned: usize) -> Result<i32> {
    if total > 0 {
        eprintln!("\n{total} of them. Once the AgentState is pushed, a teammate who pulls carries them along.");
        eprintln!("Fix it. Or use --no-verify to bypass this hook and explicitly own the consequences.");
        return Ok(1);
    }
    if !staged {
        println!("Scanned {scanned} files, no secrets found.");
    }
    Ok(0)
}

/// `agit clone <url>` -- pull the team's Agent Store into .agit/agent and install the drivers/hooks.
/// A single command for consuming someone else's context: clone + init (idempotent).
pub fn clone_agent(url: &str) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = env.join(scope::AGENT_DIR);
    if agent.join(".git").exists() {
        anyhow::bail!(
            "{} already exists. To swap in the remote context, remove it first, or just agit -a pull.",
            agent.display()
        );
    }
    std::fs::create_dir_all(agent.parent().unwrap())?;
    // Inherit stdio: keep git's progress visible, credential prompts answerable, and real stderr reaching the terminal on failure.
    // A capturing .output() would swallow all of that -- clone is the one place that reaches the remote, where flying blind is least acceptable.
    let code = scope::git_in_inherit(&env, &["clone", url, &agent.to_string_lossy()]);
    if code != 0 {
        anyhow::bail!("git clone {url} failed (exit code {code}). The git error above is the reason.");
    }
    println!("Pulled Agent Store ← {url}");
    // install driver / hook (init is idempotent; it fills in config on an existing clone)
    crate::init::run()?;
    println!("\nSee what you got: agit -a log   (or agit -a merge origin/main to merge conversations)");
    Ok(0)
}

// ─────────────────────── convert: convert sessions across runtimes ───────────────────────

/// Infer the runtime from the source file's content (session_meta=codex; sessionId/parentUuid=claude).
fn infer_runtime(text: &str) -> Option<&'static str> {
    for line in text.lines().take(20) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            return Some("codex");
        }
        if v.get("sessionId").is_some() || v.get("parentUuid").is_some() {
            return Some("claude-code");
        }
    }
    None
}

/// `agit convert <src> --to <rt> [--from <rt>] [--cwd P] [--write]`
/// The install id for a materialized session: **always a UUID**, for every runtime.
///
/// This is a regression lock, not a style choice. `codex exec resume` advertises "UUID **or thread
/// name**", so agit briefly installed codex sessions under a proper name (`feature-a-3f9a2c`). That is
/// broken, and it fails OPEN — verified against codex 0.144.4 with a fact only the history could know:
///   * UUID id, file on disk, absent from codex's index → resume RECALLED the fact (codex reads the file)
///   * proper-name id, identical file      → resume answered from thin air, ZERO history, exit 0
/// Root cause: a non-UUID is resolved as a thread name via ~/.codex/state_5.sqlite (`threads` has
/// id/rollout_path/title — no name column), and a file agit drops on disk is never indexed there. So a
/// named install silently starts a FRESH session. A UUID, by contrast, is matched against the rollout
/// files themselves and works.
///
/// Proper names therefore survive only as a human-facing LABEL (see convo::proper_name), never as the id.
fn install_id(_to_rt: &str, _branch: Option<&str>, _seed: &str) -> String {
    crate::convo::fresh_id("session")
}

pub fn convert_cmd(
    src: &Path,
    from: Option<String>,
    to: &str,
    cwd_override: Option<String>,
    write: bool,
) -> Result<i32> {
    use crate::convo::{self, ConvertOpts};

    let text = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("failed to read source session {}: {e}", src.display()))?;
    let from = match from {
        Some(f) => f,
        None => infer_runtime(&text)
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("can't recognize the source runtime, pass --from claude-code|codex explicitly"))?,
    };

    let new_id = install_id(to, convo::peek_branch(&text).as_deref(), &text);
    let opts = ConvertOpts {
        cwd: cwd_override,
        new_id: new_id.clone(),
    };
    let (out, ir) = convo::convert(src, &from, to, &opts)?;
    let cross = convo::is_cross_vendor(&from, to);

    // Target cwd (which project it installs under / where resume lands). current_dir() is evaluated lazily: called only when the source has no cwd either,
    // and its failure shouldn't needlessly abort the conversion when the cwd is already known.
    let cwd = match opts.cwd.clone().or_else(|| ir.cwd.clone()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };

    // The output = a new copy of sensitive content; scan it at high precision before writing to disk (jsonl: entropy detection off)
    let hits = scan::scan_text_opts(&out, true).len();

    println!("convert {from} → {to}{}", if cross { " (cross-vendor: content-level, drops encrypted reasoning and narrated tools)" } else { " (same vendor: byte-level replay)" });
    println!("  source   : {}", src.display());
    println!("  new id   : {new_id}");
    println!("  turns/ln : {} events → {} lines", ir.events.len(), out.lines().count());
    println!("  dest cwd : {}", cwd.display());
    if hits > 0 {
        eprintln!("  ⚠ scanned {hits} suspected secrets in the output -- a new copy of content the source session saw, so be careful not to leak it.");
    }

    if !write {
        let preview: String = out.lines().take(3).collect::<Vec<_>>().join("\n");
        println!("\n  preview (first 3 lines):\n{preview}");
        println!("\n  -- dry-run, nothing written. Add --write to install and print the resume command.");
        return Ok(0);
    }

    let h = crate::register::install(to, &new_id, &cwd, &out)?;
    println!("\n  written: {}", h.path.display());
    println!("  resume : {}", h.resume_cmd);
    Ok(0)
}

/// Print the current WorkspaceRevision (Agent↔Environment pairing).
pub fn workspace_show() -> Result<i32> {
    let head = scope::workspace_dir()?.join("HEAD.json");
    if !head.exists() {
        println!("No WorkspaceRevision yet. One is generated automatically after either repo commits.");
        return Ok(0);
    }
    println!("{}", std::fs::read_to_string(head)?);
    Ok(0)
}

pub fn workspace_log() -> Result<i32> {
    let log = scope::workspace_dir()?.join("log.jsonl");
    if !log.exists() {
        println!("No WorkspaceRevision yet.");
        return Ok(0);
    }
    print!("{}", std::fs::read_to_string(log)?);
    Ok(0)
}

/// Read the workspace log into a revision list (newest first).
fn workspace_revisions() -> Result<Vec<serde_json::Value>> {
    let log = scope::workspace_dir()?.join("log.jsonl");
    if !log.exists() {
        return Ok(vec![]);
    }
    let mut revs: Vec<serde_json::Value> = std::fs::read_to_string(log)?
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    revs.reverse(); // newest first
    Ok(revs)
}

/// `agit workspace restore [<N|agent-rev>]` -- roll both repos back together to the joint state recorded by
/// a WorkspaceRevision (the "undo" half of JointVersionControl). With no argument, list the available revisions.
pub fn workspace_restore(selector: Option<&str>) -> Result<i32> {
    let revs = workspace_revisions()?;
    if revs.is_empty() {
        anyhow::bail!("No WorkspaceRevision to restore yet. One is generated automatically after either repo commits.");
    }

    let short = |s: &str| s.chars().take(9).collect::<String>();
    let Some(sel) = selector else {
        // No selector given: list the joint states to choose from (newest first).
        println!("Restorable joint states (newest first):\n");
        for (i, r) in revs.iter().enumerate() {
            let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
            let trig = r.get("trigger").and_then(|v| v.as_str()).unwrap_or("?");
            let ar = r.get("agent_rev").and_then(|v| v.as_str()).unwrap_or("");
            let ec = r.get("env").and_then(|e| e.get("head_commit")).and_then(|v| v.as_str()).unwrap_or("");
            println!("  {:>2}. {ts}  {trig:14}  agent {} · env {}", i + 1, short(ar), short(ec));
        }
        println!("\nUse `agit workspace restore <number>` or `restore <agent-rev prefix>` to roll back.");
        return Ok(0);
    };

    // Selector: a pure number = index (1 = newest); otherwise match by agent_rev prefix.
    let chosen = if let Ok(n) = sel.parse::<usize>() {
        revs.get(n.wrapping_sub(1))
    } else {
        revs.iter().find(|r| {
            r.get("agent_rev").and_then(|v| v.as_str()).map(|a| a.starts_with(sel)).unwrap_or(false)
        })
    };
    let Some(rev) = chosen else {
        anyhow::bail!("No WorkspaceRevision matching `{sel}`. Run `agit workspace restore` to see the options.");
    };

    let agent_rev = rev.get("agent_rev").and_then(|v| v.as_str()).unwrap_or("");
    let env_commit = rev
        .get("env")
        .and_then(|e| e.get("head_commit"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if env_commit.is_empty() {
        anyhow::bail!("This revision has no env.head_commit, so it can't be restored.");
    }

    let env = scope::env_root()?;
    println!("Restoring joint state → env {} · agent {}", short(env_commit), short(agent_rev));
    println!("(Both repos will checkout to that commit, entering detached HEAD; git will refuse to overwrite uncommitted changes.)\n");

    // Environment first, then Agent Store. Stop on the first failure so the user sees git's real error.
    println!("Environment:");
    let ec = scope::git_in_inherit(&env, &["checkout", env_commit]);
    if ec != 0 {
        anyhow::bail!("Environment checkout failed (exit code {ec}). Commit or stash your unsaved changes first.");
    }
    if !agent_rev.is_empty() {
        if let Ok(agent) = scope::agent_root() {
            println!("Agent Store:");
            let ac = scope::git_in_inherit(&agent, &["checkout", agent_rev]);
            if ac != 0 {
                anyhow::bail!("Agent Store checkout failed (exit code {ac}). Environment was already rolled back, Agent Store untouched.");
            }
        }
    }
    println!("\nBack at that joint state. To build on it, create a branch with `agit checkout -b <branch>` / `agit -a checkout -b <branch>`.");
    Ok(0)
}

// ─────────────────────── graph: the Workspace-State timeline + relations ───────────────────────

/// `agit graph` -- render the WorkspaceRevision DAG: each joint state, plus the Agent↔Environment /
/// Agent↔Agent edges recorded at that point.
pub fn workspace_graph() -> Result<i32> {
    let mut revs = workspace_revisions()?;
    if revs.is_empty() {
        println!("No WorkspaceRevisions yet. One is generated automatically after either repo moves a ref.");
        return Ok(0);
    }
    revs.reverse(); // oldest first, so the timeline reads top-to-bottom
    let short = |s: &str| s.chars().take(9).collect::<String>();
    println!("Workspace timeline ({} revisions, oldest first):\n", revs.len());
    for r in &revs {
        let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
        let trig = r.get("trigger").and_then(|v| v.as_str()).unwrap_or("?");
        let ar = r.get("agent_rev").and_then(|v| v.as_str()).unwrap_or("");
        let ec = r.get("env").and_then(|e| e.get("head_commit")).and_then(|v| v.as_str()).unwrap_or("");
        println!("● {ts}  {trig}");
        println!("│   agent {}  ·  env {}", if ar.is_empty() { "∅".into() } else { short(ar) }, short(ec));
        if let Some(rels) = r.get("relations").and_then(|v| v.as_array()) {
            for e in rels {
                if let Some(e) = e.as_str() {
                    println!("│   ⇄ {e}");
                }
            }
        }
        println!("│");
    }
    Ok(0)
}

// ─────────────────────── resume: load a session into a runtime and continue ───────────────────────

/// Convert a session file to runtime `to` and install it into that runtime's native store so it can be
/// resumed. Returns the resume handle. Shared by `convert --write`, `resume`, and the auto-convert worker.
pub fn materialize(src: &Path, from: &str, to: &str, cwd_override: Option<String>) -> Result<crate::register::ResumeHandle> {
    use crate::convo::{self, ConvertOpts};
    let text = std::fs::read_to_string(src)?;
    let new_id = install_id(to, convo::peek_branch(&text).as_deref(), &text);
    let opts = ConvertOpts { cwd: cwd_override, new_id: new_id.clone() };
    let (out, ir) = convo::convert(src, from, to, &opts)?;
    let cwd = match opts.cwd.clone().or_else(|| ir.cwd.clone()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };
    let h = crate::register::install(to, &new_id, &cwd, &out)?;
    // Record that agit produced this id, so the watcher never re-converts its own output (which would
    // otherwise feed back: A→B, then snap B, then B→A, forever).
    mark_generated(&new_id);
    Ok(h)
}

/// The id-registry of sessions agit itself materialized (one id per line, inside the agent repo's .git).
fn generated_file(agent: &Path) -> PathBuf {
    agent.join(".git").join("agit-generated")
}

fn mark_generated(id: &str) {
    if let Ok(agent) = crate::scope::root_for(crate::scope::Scope::Agent) {
        use std::io::Write;
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(generated_file(&agent))
            .and_then(|mut f| writeln!(f, "{id}"));
    }
}

fn load_generated(agent: &Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(generated_file(agent))
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// `agit convert --watch [--interval N]` — the auto-convert background worker. Watches the Agent Store's
/// per-runtime session trees and, for each session not yet mirrored into the OTHER runtime, converts and
/// installs it under its proper name — so any captured session is immediately resumable in either CLI
/// (`codex exec resume <name>` / `claude --resume <id>`). Conversion is a pure deterministic transform
/// (no model calls). Ctrl-C to stop.
/// One cross-runtime conversion pass over the Agent Store: for each session not already converted
/// (tracked by source-content hash in `seen`), materialize it into the OTHER runtime under its proper
/// name. Shared by `convert --watch` and the unified `agit watch` loop.
pub fn convert_pass(agent: &Path, seen: &mut std::collections::HashSet<String>) {
    let generated = load_generated(agent);
    for (from, to) in [("claude-code", "codex"), ("codex", "claude-code")] {
        let dir = agent.join("sessions").join(from);
        let files = walkdir::WalkDir::new(&dir)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false));
        for e in files {
            // never re-convert a session agit itself produced — that's the feedback-loop guard.
            let stem = e.path().file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if generated.contains(&stem) {
                continue;
            }
            let content = match std::fs::read_to_string(e.path()) {
                Ok(c) if !c.trim().is_empty() => c,
                _ => continue,
            };
            // key on source content — an unchanged session converts once; once marked seen (even on
            // error) it won't spin re-attempting the same input.
            let key = format!("{from}->{to}:{}", &crate::convo::sha256_hex(&content)[..16]);
            if !seen.insert(key) {
                continue;
            }
            match materialize(e.path(), from, to, None) {
                Ok(h) => println!("  ● {from}→{to}  {}", h.resume_cmd),
                Err(err) => eprintln!("  ⚠ {from}→{to} {}: {err:#}", e.file_name().to_string_lossy()),
            }
        }
    }
}

pub fn convert_watch(interval_secs: u64) -> Result<i32> {
    use std::time::Duration;
    let agent = crate::scope::root_for(crate::scope::Scope::Agent)?;
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut seen = std::collections::HashSet::new();
    println!(
        "Auto-converting sessions both ways (claude-code ↔ codex) every {}s. Ctrl-C to stop.",
        interval.as_secs()
    );
    loop {
        convert_pass(&agent, &mut seen);
        std::thread::sleep(interval);
    }
}

/// `agit resume <src-session> [--as <rt>] [--cwd <path>] [--exec]` -- the universal loader: install a
/// session so a runtime can resume it (converting across runtimes when `--as` differs from the source),
/// then print or (with --exec) launch the resume command. A thin, first-class wrapper over convert/register.
pub fn resume_cmd(
    src: &Path,
    as_rt: Option<String>,
    cwd_override: Option<String>,
    env_override: Option<String>,
    exec: bool,
    relocate: bool,
) -> Result<i32> {
    use crate::convo::{self, ConvertOpts};

    let text = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("failed to read source session {}: {e}", src.display()))?;
    let from = infer_runtime(&text)
        .ok_or_else(|| anyhow::anyhow!("could not detect the source runtime; the file doesn't look like a claude-code or codex session"))?;
    // Default target = the source runtime (a plain resume, no conversion); --as forces a different one.
    let to = as_rt.unwrap_or_else(|| from.to_string());

    // --env rebinds the Agent State to a DIFFERENT Environment than the one it ran in: the session is
    // re-pointed at that checkout, so the same agent context can continue against another repo/clone.
    let (cwd_override, rebound) = match env_override {
        Some(e) => {
            let p = std::fs::canonicalize(&e)
                .map_err(|_| anyhow::anyhow!("no such environment path: {e}"))?;
            let (rc, _) = crate::scope::git_in_status(&p, &["rev-parse", "--is-inside-work-tree"]);
            if rc != 0 {
                anyhow::bail!("{} is not a git repository — an Environment is a code repo", p.display());
            }
            (Some(p.to_string_lossy().into_owned()), Some(p))
        }
        None => (cwd_override, None),
    };

    let new_id = install_id(&to, convo::peek_branch(&text).as_deref(), &text);
    let opts = ConvertOpts { cwd: cwd_override, new_id: new_id.clone() };
    let (out, ir) = convo::convert(src, from, &to, &opts)?;
    let cwd = match opts.cwd.clone().or_else(|| ir.cwd.clone()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };

    // --relocate: rewrite the ORIGINAL environment's path prefix everywhere in the transcript (bash
    // commands, file_paths, narrated tools), not just the session's own cwd. Only correct when the
    // SAME project moved to a new path — pointing an agent at a different repo must keep its paths,
    // since those are its real memory of the other codebase.
    let out = match (relocate, ir.cwd.as_deref()) {
        (true, Some(old)) => convo::swap_path(&out, old, &cwd.to_string_lossy()),
        _ => out,
    };

    let hits = scan::scan_text_opts(&out, true).len();
    if hits > 0 {
        eprintln!("  ⚠ {hits} suspected secret(s) in the materialized session -- a fresh copy of what the source saw.");
    }
    let h = crate::register::install(&to, &new_id, &cwd, &out)?;
    println!("Installed → {}", h.path.display());
    if let Some(envp) = &rebound {
        let origin = ir.cwd.as_deref().unwrap_or("(unknown)");
        println!("Environment: {}  (rebound from {origin})", envp.display());
        if relocate {
            println!("  relocated: rewrote {origin} → {} throughout the transcript", envp.display());
        } else {
            eprintln!(
                "  note: the session's own cwd is rebound; paths it recorded under {origin} are kept as-is\n         (they're its memory of that codebase). Pass --relocate if this is the SAME project moved."
            );
        }
    }
    println!("Resume: {}", h.resume_cmd);

    if exec {
        println!("\nLaunching…\n");
        let status = std::process::Command::new("sh").arg("-c").arg(&h.resume_cmd).status()?;
        return Ok(status.code().unwrap_or(0));
    }
    Ok(0)
}


#[cfg(test)]
mod install_id_tests {
    use super::*;

    /// Regression lock. codex advertises "UUID or thread name", but a name-id install silently starts a
    /// FRESH session (verified against codex 0.144.4: name-resume answered with zero history, exit 0).
    /// Only a UUID is matched against the rollout files on disk. Never hand a runtime a non-UUID id.
    #[test]
    fn install_id_is_always_a_uuid_even_for_codex() {
        for rt in ["codex", "claude-code"] {
            let id = install_id(rt, Some("feature-a"), "seed-content");
            assert!(!id.starts_with("feature-a"), "{rt}: got a proper name, not a uuid: {id}");
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(parts.len(), 5, "{rt}: not uuid-shaped: {id}");
            assert_eq!(
                (parts[0].len(), parts[1].len(), parts[2].len(), parts[3].len(), parts[4].len()),
                (8, 4, 4, 4, 12),
                "{rt}: not 8-4-4-4-12: {id}"
            );
            assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{rt}: non-hex: {id}");
        }
    }
}
