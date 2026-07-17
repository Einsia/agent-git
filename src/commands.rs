//! agit native verbs (commands beyond passthrough where agit adds value).
//! Native commands under the session model: scan (secret gate), workspace (pairing), clone, adapter, convert.
//! See docs/architecture.md.

use crate::adapter;
use crate::agent;
use crate::scan;
use crate::scope::{self, Scope};
use crate::ui;
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
            root,
            &["diff", "--cached", "--name-only", "-z", "--diff-filter=ACM"],
        );
        for name in out.split('\0').filter(|s| !s.is_empty()) {
            let (code, content) = scope::git_in_status(root, &["show", &format!(":{name}")]);
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
        WalkDir::new(root)
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
        let rel = t.strip_prefix(root).unwrap_or(t).display().to_string();
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

/// The shared front half of convert / materialize / resume: mint the install id, convert `src` into
/// runtime `to`, and resolve the destination cwd. Returns `(new_id, output, ir, cwd)`. It does NOT
/// install — the callers differ on what comes next (dry-run preview, launch, or marking the id as
/// agit-generated), so installing stays theirs.
fn convert_for_install(
    text: &str,
    src: &Path,
    from: &str,
    to: &str,
    cwd_override: Option<String>,
) -> Result<(String, String, crate::convo::ConversationIR, PathBuf)> {
    use crate::convo::{self, ConvertOpts};
    let new_id = install_id(to, convo::peek_branch(text).as_deref(), text);
    let opts = ConvertOpts { cwd: cwd_override, new_id: new_id.clone() };
    let (out, ir) = convo::convert(src, from, to, &opts)?;
    let cwd = match opts.cwd.clone().or_else(|| ir.cwd.clone()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir()?,
    };
    Ok((new_id, out, ir, cwd))
}

pub fn convert_cmd(
    src: &Path,
    from: Option<String>,
    to: &str,
    cwd_override: Option<String>,
    write: bool,
) -> Result<i32> {
    use crate::convo;

    let text = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("failed to read source session {}: {e}", src.display()))?;
    let from = match from {
        Some(f) => f,
        None => infer_runtime(&text)
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("can't recognize the source runtime, pass --from claude-code|codex explicitly"))?,
    };

    let (new_id, out, ir, cwd) = convert_for_install(&text, src, &from, to, cwd_override)?;
    let cross = convo::is_cross_vendor(&from, to);

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
            // Moving HEAD under a concurrent snap is what the store lock exists for: one store is
            // shared by every repo that tracks the agent, and a watcher in another repo is a writer.
            let _lock = crate::session::lock_store(&agent)?;
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

/// Convert a session file to runtime `to`, install it into that runtime's native store so it can be
/// resumed, and return the id it installed under with the resume handle. Shared by `start` and the
/// auto-convert worker; both need the id, because it is the only key capture can attribute by (§6).
pub fn materialize_id(
    src: &Path,
    from: &str,
    to: &str,
    cwd_override: Option<String>,
) -> Result<(String, crate::register::ResumeHandle)> {
    let text = std::fs::read_to_string(src)?;
    let (new_id, out, _ir, cwd) = convert_for_install(&text, src, from, to, cwd_override)?;
    let h = crate::register::install(to, &new_id, &cwd, &out)?;
    // Record that agit produced this id, so the watcher never re-converts its own output (which would
    // otherwise feed back: A→B, then snap B, then B→A, forever).
    mark_generated(&new_id);
    Ok((new_id, h))
}

/// The id-registry of sessions agit itself materialized (one id per line).
///
/// Machine-local, like the launch record and for the same reason: `materialize` mints an id long
/// before capture decides which agent's store the session belongs to, and one store is shared by many
/// environments. Keyed inside a single store, the guard below would miss every id routed elsewhere —
/// and a missed guard is not a cosmetic loss, it is the A→B→A conversion loop running forever.
fn generated_file(home: &Path) -> PathBuf {
    home.join("generated")
}

fn mark_generated(id: &str) {
    let Ok(home) = scope::agit_home() else { return };
    use std::io::Write;
    let _ = std::fs::create_dir_all(&home);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(generated_file(&home))
        .and_then(|mut f| writeln!(f, "{id}"));
}

fn load_generated() -> std::collections::HashSet<String> {
    scope::agit_home()
        .ok()
        .and_then(|h| std::fs::read_to_string(generated_file(&h)).ok())
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
pub fn convert_pass(agent: &Path, env: &Path, seen: &mut std::collections::HashSet<String>) {
    let generated = load_generated();
    // Whose sessions these are. The converted copy is agit's own output on behalf of THIS agent, so it
    // gets a launch record of its own: without one, capture reads it as hand-started and files it under
    // the repo's DEFAULT agent — which lands one agent's transcript in another agent's store, and
    // pushes it to that team. A legacy store has no identity to record, and needs none: it is the only
    // store its sessions can go to.
    let owner = agent::read_identity(agent).ok();
    // `store_sessions`, not a glob of `sessions/<rt>/`: it reads BOTH store layouts, so an
    // env-partitioned store does not silently stop auto-converting, and it already excludes a
    // session's sidecars (`<id>/subagents/*.jsonl`), which the old `max_depth(1)` was there for.
    let sessions = store_sessions(agent);
    // Every ordered pair of distinct runtimes, from the registry — so a third runtime is converted to
    // and from the others with no change here.
    let runtimes = crate::session::runtimes();
    let mut pairs = Vec::new();
    for &from in &runtimes {
        for &to in &runtimes {
            if from != to {
                pairs.push((from, to));
            }
        }
    }
    for (from, to) in pairs {
        for e in sessions.iter().filter(|s| s.runtime == from) {
            // never re-convert a session agit itself produced — that's the feedback-loop guard.
            let stem = e.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if generated.contains(&stem) {
                continue;
            }
            let content = match std::fs::read_to_string(&e.path) {
                Ok(c) if !c.trim().is_empty() => c,
                _ => continue,
            };
            // key on source content — an unchanged session converts once; once marked seen (even on
            // error) it won't spin re-attempting the same input.
            let key = format!("{from}->{to}:{}", &crate::convo::sha256_hex(&content)[..16]);
            if !seen.insert(key) {
                continue;
            }
            match materialize_id(&e.path, from, to, None) {
                Ok((new_id, h)) => {
                    if let Some(o) = &owner {
                        if let Err(err) = record_launch(&new_id, &o.aid, &o.name, env, to) {
                            eprintln!("  ⚠ {from}→{to} launch record not written ({err:#}) — capture will attribute this copy by repo default.");
                        }
                    }
                    println!("  ● {from}→{to}  {}", h.resume_cmd);
                }
                Err(err) => eprintln!("  ⚠ {from}→{to} {}: {err:#}", e.path.display()),
            }
        }
    }
}

pub fn convert_watch(interval_secs: u64) -> Result<i32> {
    use std::time::Duration;
    // Converting is not attribution — it makes THIS agent's sessions resumable in either CLI — so the
    // resolved agent is the right answer, and capture no longer writes the store `-a` resolves to.
    let agent = crate::session::convert_target()?;
    let env = scope::env_root()?;
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut seen = std::collections::HashSet::new();
    println!(
        "Auto-converting sessions both ways (claude-code ↔ codex) every {}s. Ctrl-C to stop.",
        interval.as_secs()
    );
    loop {
        convert_pass(&agent, &env, &mut seen);
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
    use crate::convo;

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

    let (new_id, out, ir, cwd) = convert_for_install(&text, src, from, &to, cwd_override)?;

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


// ─────────────────────── The launch record (§6): whose session is this? ───────────────────────

/// One launch: `session-id → {aid, env, runtime, started}`.
///
/// Why this exists at all: the runtime dumps per PROJECT (`~/.claude/projects/<cwd-slug>/`), not per
/// agent, so two agents working in one repo write to the SAME folder. The active pointer cannot tell
/// their sessions apart — attributing capture by it misfiles silently, into the wrong agent, and pushes
/// a transcript to the wrong team. `agit start` launched the session, so it alone knows whose it is.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Launch {
    pub session: String,
    /// Identity, not the label: a rename must not orphan the record.
    pub aid: String,
    pub name: String,
    pub env: String,
    pub runtime: String,
    pub started: String,
}

/// How a session was attributed to an agent. Capture must be able to SAY which — a guess reported as a
/// fact is the failure mode this whole record exists to remove.
pub enum Attribution {
    /// `agit start` wrote a record: authoritative.
    Launched(Launch),
    /// No record (a plain `claude`/`codex` session) → the repo's default agent, reported as a fallback.
    RepoDefault { aid: String, name: String },
}

impl Attribution {
    pub fn aid(&self) -> &str {
        match self {
            Attribution::Launched(l) => &l.aid,
            Attribution::RepoDefault { aid, .. } => aid,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Attribution::Launched(l) => &l.name,
            Attribution::RepoDefault { name, .. } => name,
        }
    }
    /// The line capture prints. `None` when the record is authoritative — there is nothing to disclose.
    pub fn note(&self) -> Option<String> {
        match self {
            Attribution::Launched(_) => None,
            Attribution::RepoDefault { name, .. } => Some(format!(
                "not started by agit, so it has no launch record — filing it under this repo's default agent `{name}`. \
                 Start it with `agit start --agent <name>` to attribute it exactly."
            )),
        }
    }
}

/// Machine-local, spans repos: a session id is a UUID, so one file can never collide, and capture in any
/// environment can read it. Append-only; the last record for an id wins.
fn launches_file(home: &Path) -> PathBuf {
    home.join("launches.jsonl")
}

fn record_launch_at(home: &Path, l: &Launch) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(home)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(launches_file(home))?;
    writeln!(f, "{}", serde_json::to_string(l)?)?;
    Ok(())
}

fn lookup_launch_at(home: &Path, session: &str) -> Option<Launch> {
    let text = std::fs::read_to_string(launches_file(home)).ok()?;
    text.lines()
        .filter_map(|l| serde_json::from_str::<Launch>(l.trim()).ok()).rfind(|l| l.session == session)
}

/// Write the launch record. Called by `start` BEFORE the runtime is exec'd: a session captured before
/// its record exists is a session attributed by guesswork.
pub fn record_launch(session: &str, aid: &str, name: &str, env: &Path, runtime: &str) -> Result<()> {
    record_launch_at(
        &scope::agit_home()?,
        &Launch {
            session: session.to_string(),
            aid: aid.to_string(),
            name: name.to_string(),
            env: env.display().to_string(),
            runtime: runtime.to_string(),
            started: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        },
    )
}

pub fn lookup_launch(session: &str) -> Option<Launch> {
    lookup_launch_at(&scope::agit_home().ok()?, session)
}

/// Attribute a captured session to an agent: the launch record if there is one, else the repo's default
/// — never the active pointer, which cannot tell two agents in one dump folder apart.
pub fn attribute_session(session: &str) -> Result<Attribution> {
    if let Some(l) = lookup_launch(session) {
        return Ok(Attribution::Launched(l));
    }
    let a = agent::resolve(None)?;
    Ok(Attribution::RepoDefault { aid: a.aid, name: a.name })
}

// ─────────────────────── start: the agent's latest session, here ───────────────────────

/// A session in the store, wherever it was recorded.
pub struct StoredSession {
    pub path: PathBuf,
    pub runtime: &'static str,
    /// The environment it was recorded in (`sessions/<env-slug>/<rt>/`), when the store is partitioned.
    pub env_slug: Option<String>,
    /// When it last did anything, as the STORE records it: the `<id>.agit.json` sidecar, else the
    /// transcript's own last timestamp. `None` only for a session that records no time at all.
    pub last_activity: Option<chrono::DateTime<chrono::Utc>>,
    pub mtime: std::time::SystemTime,
}

impl StoredSession {
    /// What to show a user as "when". mtime is the fallback, not the source: see `latest_session`.
    pub fn recency(&self) -> std::time::SystemTime {
        self.last_activity.map(std::time::SystemTime::from).unwrap_or(self.mtime)
    }
}

/// `<id>.jsonl` → `<id>.agit.json` — the sidecar snap writes beside every captured session (§6).
pub fn sidecar_path(transcript: &Path) -> PathBuf {
    transcript.with_extension("agit.json")
}

/// When a stored session last did anything, read from content **git preserves**: the sidecar first
/// (one line, always there for anything snap captured), else the transcript's own last timestamp,
/// which costs a scan but keeps a store written before sidecars existed orderable.
fn recorded_activity(transcript: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let ts = sidecar_last_activity(transcript).or_else(|| crate::session::last_activity(transcript))?;
    chrono::DateTime::parse_from_rfc3339(ts.trim())
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

fn sidecar_last_activity(transcript: &Path) -> Option<String> {
    let text = std::fs::read_to_string(sidecar_path(transcript)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("last_activity")?.as_str().map(str::to_string)
}

/// The agent's latest session from ANY environment.
///
/// Read from the store's files, **never from git-log topology**. Verified: `git log -1 --name-only`
/// prints no file names at all on a merge commit (it prints the header and stops), so a log-derived
/// leaf-finder returns nothing exactly after a merge or a pull — the moment `start` matters most.
///
/// A candidate's parent directory must BE the runtime dir, which holds for both the flat
/// `sessions/<rt>/` layout and the partitioned `sessions/<env>/<rt>/` one, and excludes a session's
/// sidecars (`<id>/subagents/*.jsonl`), which are not sessions.
pub fn store_sessions(store: &Path) -> Vec<StoredSession> {
    let mut out = Vec::new();
    for e in WalkDir::new(store.join(crate::session::SESSIONS_SUBDIR))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = e.path();
        if p.extension().map(|x| x != "jsonl").unwrap_or(true) {
            continue;
        }
        let Some(parent) = p.parent() else { continue };
        let Some(rt) = crate::session::runtimes()
            .into_iter()
            .find(|rt| parent.file_name().map(|n| n == *rt).unwrap_or(false))
        else {
            continue;
        };
        let Some(mtime) = e.metadata().ok().and_then(|m| m.modified().ok()) else { continue };
        let env_slug = parent
            .parent()
            .and_then(|g| g.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| n != crate::session::SESSIONS_SUBDIR);
        out.push(StoredSession {
            last_activity: recorded_activity(p),
            path: p.to_path_buf(),
            runtime: rt,
            env_slug,
            mtime,
        });
    }
    out
}

/// The most recent session, ordered by what the store RECORDS — never by the filesystem.
///
/// git does not preserve mtimes: after the `git clone` that `agit a clone` performs, every session
/// carries the checkout time, identical to the nanosecond, so an mtime order collapses into whatever
/// order the directory walk happened to produce. A session the store records a time for therefore
/// always beats one it does not, and mtime only breaks a tie.
pub fn latest_session(store: &Path) -> Option<StoredSession> {
    store_sessions(store).into_iter().max_by_key(|s| (s.last_activity, s.mtime))
}

// ─────────────────────── ahead/behind — one implementation, shared by status/pull/fetch ───────────────────────

/// How the store's HEAD stands against its tracked upstream: `(ahead, behind)` — the commits HEAD has
/// that the upstream lacks, and the commits the upstream has that HEAD lacks. `None` when there is no
/// resolvable upstream to compare against (a store with no tracking branch, e.g. never pushed).
///
/// The ONE ahead/behind primitive: `agit a status` reads it for its summary, `diverged` derives the
/// pull/merge decision from it, and both go through `scope::git_in_status` so a missing upstream is a
/// clean `None`, never a hard error.
pub fn ahead_behind(store: &Path) -> Option<(u64, u64)> {
    let (code, out) =
        scope::git_in_status(store, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]);
    if code != 0 {
        return None;
    }
    // `--left-right --count @{u}...HEAD` prints "<behind>\t<ahead>": the left side is the upstream.
    let mut counts = out.split_whitespace().filter_map(|s| s.parse::<u64>().ok());
    let behind = counts.next()?;
    let ahead = counts.next()?;
    Some((ahead, behind))
}

/// True when HEAD and its upstream each hold commits the other does not — the one case a fast-forward
/// cannot cover and a textual merge must not. An absent or unresolvable upstream is not "diverged".
///
/// Promoted out of `main.rs` so `agit a pull`, `agit a fetch` and `agit a status` share the single
/// `ahead_behind` above rather than each reimplementing the rev-list.
pub fn diverged(store: &Path) -> bool {
    matches!(ahead_behind(store), Some((ahead, behind)) if ahead > 0 && behind > 0)
}

/// Best-effort summary of the sessions on the tracked upstream that HEAD does not yet have. Silent if
/// there is no upstream to compare against (a fetch with an explicit refspec and no tracking branch).
///
/// Promoted out of `main.rs` alongside `diverged` so `agit a fetch` and any other session-aware reader
/// narrate incoming work the same way.
pub fn report_incoming(store: &Path) {
    let (code, out) = scope::git_in_status(
        store,
        &["diff", "--name-only", "--diff-filter=A", "HEAD..@{u}", "--", "sessions"],
    );
    if code != 0 {
        return;
    }
    let new: Vec<&str> = out.lines().filter(|l| l.ends_with(".jsonl")).collect();
    if new.is_empty() {
        println!("  up to date — no new sessions on the remote.");
        return;
    }
    // Break the count down per runtime by asking the registry which runtimes exist, not by naming any
    // — a new adapter shows up here for free.
    let breakdown: Vec<String> = crate::adapter::names()
        .iter()
        .filter_map(|rt| {
            let n = new.iter().filter(|f| f.contains(&format!("/{rt}/"))).count();
            (n > 0).then(|| format!("{rt}: {n}"))
        })
        .collect();
    let suffix = if breakdown.is_empty() { String::new() } else { format!(" ({})", breakdown.join(", ")) };
    println!("  {} new session(s) on the remote{suffix}.", new.len());
    println!("  integrate with: agit a pull");
}

// ─────────────────────── status: this repo's agents at a glance ───────────────────────

/// A one-line summary of the active store's position against its upstream, for `agit a status`.
fn upstream_line(store: &Path) -> String {
    match ahead_behind(store) {
        None => "no upstream yet — agit a push to publish".to_string(),
        Some((0, 0)) => "up to date with the remote".to_string(),
        Some((ahead, 0)) => format!("{ahead} unpushed — agit a push to publish"),
        Some((0, behind)) => format!("{behind} behind — agit a pull to integrate"),
        Some((ahead, behind)) => {
            format!("{ahead} ahead, {behind} behind — diverged; agit a merge to reconcile")
        }
    }
}

/// `agit a status` — a per-repo overview: which agents this repo works with (from the committed
/// binding), which one is active here, each one's session count and last activity, whether a watcher is
/// live for it, and where the active store stands against its remote.
///
/// Read-only and resilient: a bound agent this machine has not cloned is shown as such rather than
/// erroring, and the integrity check that `resolve` runs is allowed to fail softly (the overview still
/// lists the rest).
pub fn agent_status() -> Result<i32> {
    let env = scope::env_root()?;
    let binding = agent::Binding::load(&env)?;
    let local = agent::list().unwrap_or_default();
    // The active agent as commands actually resolve it (pointer → $AGIT_AGENT → default). `ok()` so a
    // recreated-remote integrity error never blanks the whole overview.
    let resolved = agent::resolve(None).ok();
    let active_aid = resolved.as_ref().map(|a| a.aid.clone());

    println!("{}", ui::dim(&format!("repo {}", ui::tilde(&env))));

    // The agents this repo works with = the committed binding; fall back to the resolved active agent
    // when there is no binding yet (a repo that only ran `agit a init` before this landed).
    let bound: Vec<(String, String)> = match &binding {
        Some(b) if !b.agents.is_empty() => {
            b.agents.iter().map(|e| (e.name.clone(), e.id.clone())).collect()
        }
        _ => match &resolved {
            Some(a) => vec![(a.name.clone(), a.aid.clone())],
            None => {
                println!("no agents bound to this repo — agit a init <name> mints one.");
                return Ok(0);
            }
        },
    };

    let rows: Vec<Vec<String>> = bound
        .iter()
        .map(|(name, aid)| {
            let store = local.iter().find(|a| &a.aid == aid).map(|a| a.store.clone());
            let (status, sessions, last) = match &store {
                Some(store) => {
                    let sessions = store_sessions(store);
                    let status = match crate::session::watching_pid(aid) {
                        Some(_) => ui::accent("● watching"),
                        None => ui::dim("·").to_string(),
                    };
                    let last = sessions
                        .iter()
                        .map(|s| s.recency())
                        .max()
                        .map(ui::ago)
                        .unwrap_or_else(|| "—".into());
                    (status, sessions.len().to_string(), last)
                }
                None => (ui::dim("not cloned").to_string(), "—".into(), "—".into()),
            };
            let here = if Some(aid) == active_aid.as_ref() { "  (active)" } else { "" };
            vec![name.clone(), status, sessions, format!("{last}{here}")]
        })
        .collect();
    println!("{}", ui::table(&["AGENT", "STATUS", "SESSIONS", "LAST"], &rows));

    if let Some(d) = binding.as_ref().and_then(|b| b.default.clone()) {
        println!("{}", ui::dim(&format!("default: {d}")));
    }

    // Where the active store stands against its remote — the unpushed/ahead-behind the overview exists
    // to surface. Only for a cloned active agent (a store to ask git about).
    if let Some(a) = resolved.as_ref().filter(|a| a.store.join(".git").exists()) {
        println!("\n{} {}", ui::bold(&a.name), ui::dim(&upstream_line(&a.store)));
    }
    Ok(0)
}

/// Where a stored session came from, and what it was about — the two things the `start` header carries
/// beyond the agent's own name (§11c).
///
/// The origin is the cwd the session RECORDED, not `env_slug`: the slug is the store's partition name
/// (`web`), while the header has room for the path the agent actually worked in (`~/code/web`), and a
/// store written in the flat layout has no slug at all. The slug remains the fallback.
///
/// One parse yields both, and it is the same parse the rest of agit reads sessions with — a header that
/// disagreed with `agit -a info` about what a session is would be worse than no header.
fn origin_and_gist(s: &StoredSession) -> (Option<String>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(&s.path) else { return (s.env_slug.clone(), None) };
    let ir = match s.runtime {
        "codex" => crate::adapter::codex::parse_rollout(&content, "x"),
        _ => crate::adapter::claude_code::parse_jsonl(&content, "x"),
    };
    let origin = ir
        .cwd
        .map(|c| ui::tilde(Path::new(&c)))
        .or_else(|| s.env_slug.clone());
    let gist = ir
        .prompts
        .into_iter()
        .map(|p| ui::one_line(&p, 72))
        .find(|p| !p.is_empty());
    (origin, gist)
}

/// `agit start [--agent X] [--as claude-code|codex]` — launch a real session in THIS repo already
/// carrying the agent's context, with no file paths and no ids typed (§5.1).
pub fn start_cmd(agent_sel: Option<&str>, as_rt: Option<&str>) -> Result<i32> {
    let env = scope::env_root()?;
    let ag = agent::resolve(agent_sel)?;
    match latest_session(&ag.store) {
        Some(s) => start_carrying(&ag, &env, s, as_rt),
        None => start_fresh(&ag, &env, as_rt),
    }
}

fn start_carrying(ag: &agent::Agent, env: &Path, s: StoredSession, as_rt: Option<&str>) -> Result<i32> {
    // Explicit --as wins; otherwise the session's OWN runtime — a session knows what produced it. No
    // default: the runtimes are peers (§5.3).
    let rt = crate::session::resolve_runtime(as_rt, &[s.runtime], "start")?;

    let here = ui::tilde(env);
    println!("┌ {} · {} · {rt}", ui::bold(&ag.name), ui::accent(&here));
    // The origin is the point of §5.1's cross-environment carry: a frontend agent continuing in the
    // backend repo carries a session recorded somewhere else, and the header is where you find that out.
    // Suppressed when it IS here — the line above already said where "here" is.
    let (origin, gist) = origin_and_gist(&s);
    let from = match origin.filter(|o| *o != here) {
        Some(o) => format!(" (from {o}, {})", ui::ago(s.recency())),
        None => format!(" ({})", ui::ago(s.recency())),
    };
    println!("└ carrying its latest session{}", ui::dim(&from));
    if let Some(g) = gist {
        println!("    {}", ui::dim(&format!("\"{g}\"")));
    }

    // Rebind cwd to this repo but KEEP the paths it recorded elsewhere: those are its real memory of
    // that other codebase, not stale strings. That is `resume --env` without `--relocate` (which is only
    // correct when the SAME project moved).
    let (id, h) = materialize_id(&s.path, s.runtime, &rt, Some(env.display().to_string()))?;

    // The record must exist before the runtime does. Its absence is not fatal — a session that captures
    // to the default agent beats no session — but it is never silent.
    if let Err(e) = record_launch(&id, &ag.aid, &ag.name, env, &rt) {
        eprintln!("  ⚠ launch record not written ({e:#}) — capture will attribute this session by repo default.");
    }
    exec(&h.resume_cmd)
}

/// No sessions yet: start FRESH but bound to the agent, and say so.
fn start_fresh(ag: &agent::Agent, env: &Path, as_rt: Option<&str>) -> Result<i32> {
    let rt = crate::session::resolve_runtime(as_rt, &[], "start").map_err(|e| {
        anyhow::anyhow!("{e}\n  `{}` has no sessions yet, so there is no runtime to continue in — name one: agit start --as claude-code|codex", ag.name)
    })?;
    let cli = if rt == "codex" { "codex" } else { "claude" };
    println!("┌ {} · {} · {rt}", ui::bold(&ag.name), ui::accent(&ui::tilde(env)));
    println!("└ no sessions yet — starting FRESH, bound to this agent.");
    // The runtime mints the id, so there is nothing to write a launch record against yet: this session
    // will be attributed to the repo's default agent when captured. Said, never assumed.
    eprintln!(
        "  note: a fresh session gets its id from {cli}, so it has no launch record — capture files it \
         under this repo's default agent. Once it is snapped, `agit start --agent {}` carries it exactly.",
        ag.name
    );
    exec_in(cli, env)
}

/// Launch the runtime directly. No shell, so the repo path needs no quoting: `env` comes from `git
/// rev-parse --show-toplevel`, and under `sh -c` a path holding a space, `$`, a backtick or `;` would
/// break the command or inject into it.
fn exec_in(cli: &str, dir: &Path) -> Result<i32> {
    let status = std::process::Command::new(cli)
        .current_dir(dir)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch `{cli}` in {}: {e}", dir.display()))?;
    Ok(status.code().unwrap_or(0))
}

fn exec(cmd: &str) -> Result<i32> {
    let status = std::process::Command::new("sh").arg("-c").arg(cmd).status()?;
    Ok(status.code().unwrap_or(0))
}

#[cfg(test)]
mod launch_record_tests {
    use super::*;

    fn launch(session: &str, aid: &str, name: &str) -> Launch {
        Launch {
            session: session.into(),
            aid: aid.into(),
            name: name.into(),
            env: "/code/web".into(),
            runtime: "claude-code".into(),
            started: "2026-07-16T00:00:00Z".into(),
        }
    }

    /// The correctness core of multi-agent: two agents in ONE repo dump into the SAME folder, so the id
    /// is the only thing that can tell their sessions apart.
    #[test]
    fn two_agents_in_one_repo_are_told_apart_by_session_id() {
        let home = tempfile::tempdir().unwrap();
        record_launch_at(home.path(), &launch("sess-frontend", "agt_01", "frontend")).unwrap();
        record_launch_at(home.path(), &launch("sess-api", "agt_02", "api")).unwrap();

        assert_eq!(lookup_launch_at(home.path(), "sess-frontend").unwrap().aid, "agt_01");
        assert_eq!(lookup_launch_at(home.path(), "sess-api").unwrap().aid, "agt_02");
        assert!(lookup_launch_at(home.path(), "never-launched").is_none(), "no record must not resolve to a neighbour");
    }

    #[test]
    fn the_log_is_append_only_and_the_last_record_wins() {
        let home = tempfile::tempdir().unwrap();
        record_launch_at(home.path(), &launch("s", "agt_01", "frontend")).unwrap();
        record_launch_at(home.path(), &launch("s", "agt_02", "api")).unwrap();
        assert_eq!(lookup_launch_at(home.path(), "s").unwrap().aid, "agt_02", "re-launch supersedes");
        assert_eq!(
            std::fs::read_to_string(launches_file(home.path())).unwrap().lines().count(),
            2,
            "history is kept, not rewritten"
        );
    }

    /// A record is authoritative and says nothing; a fallback must always disclose itself.
    #[test]
    fn attribution_by_default_agent_is_never_silent() {
        let launched = Attribution::Launched(launch("s", "agt_01", "frontend"));
        assert_eq!(launched.aid(), "agt_01");
        assert!(launched.note().is_none(), "an authoritative record has nothing to disclose");

        let fallback = Attribution::RepoDefault { aid: "agt_09".into(), name: "api".into() };
        let note = fallback.note().expect("a guess MUST be reported");
        assert!(note.contains("no launch record"), "{note}");
        assert!(note.contains("api"), "the note must name the agent it filed under: {note}");
    }
}

#[cfg(test)]
mod start_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        // Per-invocation identity only: a global git config would clobber the developer's real one.
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// The bug this keys on: git does not preserve mtimes. `agit a clone` CLONES the store, and a clone
    /// stamps every file at checkout time — verified identical to the nanosecond — so an mtime-ordered
    /// leaf-finder returns whichever session WalkDir happened to hand back last. Recency must therefore
    /// come from recorded CONTENT. This is acceptance criterion 3: bob clones and picks up alice's LATEST.
    #[test]
    fn recency_survives_a_clone_which_flattens_every_mtime() {
        let src = tempfile::tempdir().unwrap();
        let d = src.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        let s = d.join("sessions/web/claude-code");

        // `old` is genuinely older BY CONTENT, but is written second so that both write order and
        // WalkDir order favour it — the test must not pass by luck.
        write(
            &s.join("new.jsonl"),
            "{\"sessionId\":\"n\",\"timestamp\":\"2026-07-16T12:00:00Z\",\"type\":\"user\"}\n",
        );
        write(
            &s.join("old.jsonl"),
            "{\"sessionId\":\"o\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"type\":\"user\"}\n",
        );
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "sessions"]);

        let dst = tempfile::tempdir().unwrap();
        let clone = dst.path().join("c");
        git(d, &["clone", "-q", ".", &clone.to_string_lossy()]);

        // The premise, asserted rather than remembered: after a clone there is no recency left in the
        // fs. Checkout stamps both files as it writes them, so they land together — but "together" is
        // scheduling, not an atomic act, and git promises no more than that. Demanding the two stamps
        // match to the NANOSECOND made this fail whenever a loaded machine straddled a clock tick. What
        // must hold is that the gap carries no information: 6 YEARS apart by content, indistinguishable
        // on disk.
        let m = |n: &str| {
            std::fs::metadata(clone.join("sessions/web/claude-code").join(n)).unwrap().modified().unwrap()
        };
        let (a, b) = (m("old.jsonl"), m("new.jsonl"));
        let gap = a.duration_since(b).or_else(|_| b.duration_since(a)).unwrap_or_default();
        assert!(
            gap < std::time::Duration::from_secs(60),
            "a clone must erase the content-age gap (these are 6 years apart by content), got {gap:?} — \
             otherwise this test proves nothing"
        );

        let latest = latest_session(&clone).expect("a cloned store still has sessions");
        assert!(
            latest.path.ends_with("new.jsonl"),
            "picked {:?} — ordering fell back to the filesystem, which a clone has erased",
            latest.path
        );
    }

    /// Both store layouts resolve, from one walk. A flat store is never migrated (§12 step 2), so
    /// `sessions/<rt>/` is not a legacy path to be read second — it is one of two live layouts, and a
    /// reader that understood only the partitioned one would report an existing store as EMPTY.
    #[test]
    fn latest_session_spans_environments_and_ignores_sidecars() {
        let store = tempfile::tempdir().unwrap();
        let s = store.path().join("sessions");
        // the partitioned layout (sessions/<env>/<rt>/) and the flat one both resolve
        write(&s.join("web/claude-code/old.jsonl"), "{}\n");
        write(&s.join("api/codex/new.jsonl"), "{}\n");
        // a session's sidecars are not sessions
        write(&s.join("api/codex/new/subagents/sub.jsonl"), "{}\n");
        // and a merge transcript is not one either
        write(&s.join("merges/a-b.md"), "# transcript\n");

        // make ordering explicit rather than trusting write order
        let newer = std::time::SystemTime::now();
        let older = newer - std::time::Duration::from_secs(7200);
        filetime(&s.join("web/claude-code/old.jsonl"), older);
        filetime(&s.join("api/codex/new.jsonl"), newer);
        filetime(&s.join("api/codex/new/subagents/sub.jsonl"), newer + std::time::Duration::from_secs(60));

        let all = store_sessions(store.path());
        assert_eq!(all.len(), 2, "only real sessions: {:?}", all.iter().map(|x| &x.path).collect::<Vec<_>>());

        let latest = latest_session(store.path()).unwrap();
        assert!(latest.path.ends_with("api/codex/new.jsonl"), "picked {:?}", latest.path);
        assert_eq!(latest.runtime, "codex", "the runtime comes from the session, not a default");
        assert_eq!(latest.env_slug.as_deref(), Some("api"), "an env-partitioned store reports where it ran");
        // the newest FILE is a sidecar; it must not become the session we carry
        assert!(!latest.path.to_string_lossy().contains("subagents"));
    }

    /// The regression the design calls out: `git log -1 --name-only` prints NOTHING on a merge commit,
    /// so a log-derived leaf-finder finds nothing exactly after a merge/pull. Asserted here against real
    /// git, so the trap stays proven rather than remembered.
    #[test]
    fn the_leaf_survives_a_merge_commit_because_it_is_not_read_from_git_log() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        write(&d.join("sessions/claude-code/base.jsonl"), "{}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "base"]);

        git(d, &["checkout", "-qb", "peer"]);
        write(&d.join("sessions/claude-code/theirs.jsonl"), "{}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "theirs"]);

        git(d, &["checkout", "-q", "main"]);
        write(&d.join("sessions/claude-code/mine.jsonl"), "{}\n");
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "mine"]);
        git(d, &["merge", "-q", "--no-ff", "peer", "-m", "merge peer"]);

        // the trap, demonstrated in-repo: the merge commit names no files at all
        assert!(git(d, &["log", "-1", "--format=", "--name-only", "HEAD"]).is_empty(), "git changed: a merge commit now names files");
        assert!(!git(d, &["log", "-1", "--format=", "--name-only", "HEAD^"]).is_empty(), "a non-merge commit does name them");

        // …and the leaf-finder is unaffected, because it reads the files.
        let latest = latest_session(d).expect("a merge must not hide the agent's sessions");
        assert_eq!(latest.runtime, "claude-code");
        assert_eq!(store_sessions(d).len(), 3, "every session survives the merge");
    }

    fn filetime(p: &Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    /// Acceptance criterion 3: bob clones the agent and picks up alice's LATEST session. `agit a clone`
    /// is a `git clone`, and git does not preserve mtimes — every file gets the checkout time — so an
    /// mtime-ordered leaf-finder returns whatever the directory walk happened to hand back last.
    /// Asserted against real git, so the trap stays proven rather than remembered.
    #[test]
    fn the_latest_session_survives_a_real_clone_because_recency_is_recorded_not_mtimed() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        git(&store, &["init", "-q", "-b", "main", "."]);

        // alice's two sessions. `new` is the one bob must pick up.
        let s = store.join("sessions/claude-code");
        write(&s.join("old.jsonl"), "{\"timestamp\":\"2026-07-10T10:00:00.000Z\"}\n");
        write(&s.join("old.agit.json"), "{\"last_activity\":\"2026-07-10T10:00:00.000Z\"}\n");
        write(&s.join("new.jsonl"), "{\"timestamp\":\"2026-07-16T10:00:00.000Z\"}\n");
        write(&s.join("new.agit.json"), "{\"last_activity\":\"2026-07-16T10:00:00.000Z\"}\n");
        git(&store, &["add", "-A"]);
        git(&store, &["commit", "-qm", "alice's sessions"]);
        assert_eq!(latest_session(&store).unwrap().path.file_stem().unwrap(), "new");

        // bob: `agit a clone` → git clone.
        let clone = d.path().join("clone");
        let out = Command::new("git").args(["clone", "-q"]).arg(&store).arg(&clone).output().unwrap();
        assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(store_sessions(&clone).len(), 2, "the clone must carry both sessions");

        // The sidecars travelled (git carries content), so recency did too.
        assert_eq!(
            latest_session(&clone).expect("bob must find alice's sessions").path.file_stem().unwrap(),
            "new"
        );

        // …and it is genuinely not the filesystem talking: hand the STALE session the newest mtime.
        // After a checkout the mtimes carry no recency at all, so this is not a contrived ordering —
        // it is simply one the filesystem is free to report.
        let cs = clone.join("sessions/claude-code");
        filetime(&cs.join("old.jsonl"), std::time::SystemTime::now());
        filetime(&cs.join("new.jsonl"), std::time::SystemTime::now() - std::time::Duration::from_secs(7200));
        let latest = latest_session(&clone).unwrap();
        assert_eq!(latest.path.file_stem().unwrap(), "new", "mtime overruled the recorded activity: {:?}", latest.path);
    }

    /// A store captured before sidecars existed must still order: the transcript records its own time.
    #[test]
    fn recency_falls_back_to_the_transcripts_own_last_timestamp_when_there_is_no_sidecar() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("sessions/codex");
        write(
            &s.join("first.jsonl"),
            "{\"timestamp\":\"2026-07-01T00:00:00.000Z\",\"type\":\"session_meta\"}\n\
             {\"timestamp\":\"2026-07-02T00:00:00.000Z\",\"type\":\"event_msg\"}\n",
        );
        write(&s.join("second.jsonl"), "{\"timestamp\":\"2026-07-03T00:00:00.000Z\",\"type\":\"session_meta\"}\n");
        // the filesystem says the opposite of what the transcripts say
        filetime(&s.join("first.jsonl"), std::time::SystemTime::now());
        filetime(&s.join("second.jsonl"), std::time::SystemTime::now() - std::time::Duration::from_secs(7200));

        assert_eq!(
            latest_session(d.path()).unwrap().path.file_stem().unwrap(),
            "second",
            "recency must come from the transcript, not from the file"
        );
        // the session's LAST activity, not its first
        let all = store_sessions(d.path());
        let first = all.iter().find(|x| x.path.ends_with("first.jsonl")).unwrap();
        assert_eq!(first.last_activity.unwrap().to_rfc3339(), "2026-07-02T00:00:00+00:00");
    }

    /// A session that records no time at all must not outrank one that does — mtime is the last
    /// resort, and after a clone it is pure noise.
    #[test]
    fn a_recorded_time_beats_a_session_that_records_none() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("sessions/claude-code");
        write(&s.join("timed.jsonl"), "{\"timestamp\":\"2026-07-01T00:00:00.000Z\"}\n");
        write(&s.join("untimed.jsonl"), "{\"type\":\"queue-operation\"}\n");
        filetime(&s.join("untimed.jsonl"), std::time::SystemTime::now());
        filetime(&s.join("timed.jsonl"), std::time::SystemTime::now() - std::time::Duration::from_secs(7200));

        assert_eq!(latest_session(d.path()).unwrap().path.file_stem().unwrap(), "timed");
    }
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
