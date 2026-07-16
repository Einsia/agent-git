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

/// The runtimes agit speaks. Peers — there is no first among them, so they are listed alphabetically
/// wherever a user reads them, and no code path may fall back to one of them as a default.
pub const RUNTIMES: [&str; 2] = ["claude-code", "codex"];

/// The runtimes named the way the user should always see them: alphabetically, in one breath.
pub fn runtime_list() -> String {
    RUNTIMES.join(", ")
}

/// Whether a runtime holds any session **this project owns**.
///
/// Ownership is the cwd recorded in the transcript, the same rule the collectors use — the dump
/// directory merely existing is not enough, since claude's slug dir can be occupied entirely by a
/// colliding project.
pub fn has_live_sessions(rt: &str, env: &Path) -> bool {
    match rt {
        "claude-code" => !claude_code::project_sessions(env).is_empty(),
        "codex" => !crate::adapter::codex::project_rollouts(env).is_empty(),
        _ => false,
    }
}

/// The runtimes with sessions for this project, alphabetically.
pub fn live_runtimes(env: &Path) -> Vec<&'static str> {
    RUNTIMES.into_iter().filter(|rt| has_live_sessions(rt, env)).collect()
}

/// The runtimes with sessions for this project already in the Agent Store, alphabetically.
pub fn store_runtimes(agent: &Path) -> Vec<&'static str> {
    RUNTIMES
        .into_iter()
        .filter(|rt| {
            std::fs::read_dir(agent.join(SESSIONS_SUBDIR).join(rt))
                .map(|mut d| d.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "jsonl")).unwrap_or(false)))
                .unwrap_or(false)
        })
        .collect()
}

/// Resolve the one runtime a single-runtime command acts on. There is NO default: an explicit flag
/// wins, else the only runtime actually present, else we ask. Picking a runtime because it is spelled
/// "claude" is exactly the framing bug this exists to prevent, so ambiguity is never resolved silently.
///
/// `present` is the caller's notion of presence (live dumps for capture, the store for merge), and
/// `what` completes the sentence "which runtime should agit …?".
pub fn resolve_runtime(explicit: Option<&str>, present: &[&'static str], what: &str) -> Result<String> {
    use std::io::{stdin, stdout, BufRead, IsTerminal, Write};
    if let Some(r) = explicit {
        let rt = normalize(r);
        if !RUNTIMES.contains(&rt.as_str()) {
            bail!("unknown runtime `{r}`. Registered: {}", runtime_list());
        }
        return Ok(rt);
    }
    match present {
        [] => bail!("No {} sessions found to {what}.", runtime_list()),
        [only] => Ok(only.to_string()),
        many => {
            let names = many.join(", ");
            if !stdin().is_terminal() {
                bail!("{names} all have sessions — say which with --from <runtime>.");
            }
            println!("Sessions exist for {names}. Which runtime should agit {what}?");
            for (i, rt) in many.iter().enumerate() {
                println!("  {}) {rt}", i + 1);
            }
            print!("Choice [1-{}]: ", many.len());
            let _ = stdout().flush();
            let mut line = String::new();
            stdin().lock().read_line(&mut line).ok();
            let pick = line.trim();
            // accept either the number or the runtime's name
            many.iter()
                .position(|rt| *rt == pick)
                .or_else(|| pick.parse::<usize>().ok().filter(|n| (1..=many.len()).contains(n)).map(|n| n - 1))
                .map(|i| many[i].to_string())
                .ok_or_else(|| anyhow::anyhow!("no runtime picked; rerun with --from <runtime> ({names})"))
        }
    }
}

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

/// `agit a snap [--from <runtime>]` — mirror session dumps into the Agent Store, once.
///
/// With no `--from` there is nothing to default to: claude-code and codex are peers, so snap captures
/// BOTH (the shape `watch` already uses), skipping quietly whichever has no sessions for this project.
/// An explicit `--from` is a different contract — the user named a runtime, so its absence is an error.
pub fn snap(runtime: Option<&str>, capture_harness: bool) -> Result<i32> {
    // Validate an explicit runtime BEFORE any filesystem walk: `--from bogus` used to reach the
    // per-runtime path and die with a confusing "isn't wired up yet", and under `--watch` it became a
    // silent permanent no-op — the watcher polls a runtime that can never exist. Fail-open on a typo is
    // exactly what no-default-runtime exists to prevent, so every path funnels through one check.
    if let Some(r) = runtime {
        // through the SAME funnel: resolve_runtime returns on the explicit branch before reading
        // `present`, so an unknown name fails here instead of dying later as "isn't wired up yet".
        let r = resolve_runtime(Some(r), &[], "snap")?;
        return sync(&r, capture_harness);
    }
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let live = live_runtimes(&env);
    if live.is_empty() {
        bail!(
            "No {} sessions found for this project.\n\
             (has it been run in either yet? `agit adapter` lists the runtimes; `--from <runtime>` forces one.)",
            runtime_list()
        );
    }
    for rt in &live {
        snap_one(rt, &env, &agent, capture_harness)?;
    }
    Ok(0)
}

/// `agit a snap --from <runtime>` — mirror one named runtime's dump into the Agent Store.
/// `capture_harness` also captures the project's MCP/skills/config (redacting secrets); `--no-harness` skips it.
pub fn sync(runtime: &str, capture_harness: bool) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    snap_one(&normalize(runtime), &env, &agent, capture_harness)
}

fn snap_one(rt: &str, env: &Path, agent: &Path, capture_harness: bool) -> Result<i32> {
    let rt = normalize(rt);
    let (stats, source_desc, hits, dst) = mirror_once(&rt, env, agent)?;

    println!("Mirrored the session dump for {}:", rt);
    println!("  source : {source_desc}");
    println!("  target : {}", dst.display());
    println!("  files  : {} files ({} updated / {} added), {} bytes", stats.total, stats.updated, stats.added, stats.bytes);
    if hits > 0 {
        eprintln!("  ⚠ Found {hits} likely secrets — the session transcript carries sensitive content the agent has seen.");
        eprintln!("     This will be blocked again before push; run `agit -a scan` first to check, or clear it from the transcript.");
    }

    // Capture the harness (MCP servers / skills / config) alongside the sessions, redacting secrets.
    if capture_harness {
        match crate::harness::capture(agent, env, &rt) {
            Ok(r) if r.files > 0 => {
                println!(
                    "  harness: {} files ({} secret field(s) redacted)",
                    r.files,
                    r.redactions.len()
                );
                for w in &r.warnings {
                    eprintln!("  ⚠ {w}");
                }
            }
            Ok(_) => {}
            // Harness capture must never fail the snap — the session dump is already mirrored.
            Err(e) => eprintln!("  ⚠ harness capture skipped: {e:#}"),
        }
    }

    println!("\n  Commit: agit a add -A && agit a commit -m 'snap {rt} sessions'");
    Ok(0)
}

/// The mirror step shared by one-shot `snap` and the `--watch` loop: copy the runtime's dump into the
/// Agent Store and secret-scan it. Returns (stats, human source description, secret hits, destination dir).
fn mirror_once(rt: &str, env: &Path, agent: &Path) -> Result<(Stats, String, usize, PathBuf)> {
    let dst = agent.join(SESSIONS_SUBDIR).join(rt);
    std::fs::create_dir_all(&dst)?;
    // Runtimes differ in storage model, but the ownership rule is the same for both: mirror ONLY this
    // project's sessions, decided by the cwd recorded in the transcript. Claude splits by project slug
    // (which collides — see claude_collect); Codex splits by date with all projects mixed.
    let (stats, source_desc) = match rt {
        "claude-code" => claude_collect(env, &dst)?,
        "codex" => codex_collect(env, &dst)?,
        other => bail!("session dump for runtime `{other}` isn't wired up yet (see src/session.rs)"),
    };
    let hits = crate::scan::scan_tree(&dst)?;
    Ok((stats, source_desc, hits, dst))
}

/// Claude capture: mirror only the sessions this project owns out of `~/.claude/projects/<slug>/`.
///
/// `slug_for` collapses every non-alphanumeric char, so `/a/b.c`, `/a/b-c` and `/a/b/c` all share ONE
/// slug directory. Tree-mirroring it copied a *different* project's transcripts into this Agent Store,
/// and a push then shipped them to this project's teammates. Ownership comes from each transcript's
/// launch cwd (`claude_code::project_sessions`), so a colliding neighbour is skipped.
fn claude_collect(env: &Path, dst: &Path) -> Result<(Stats, String)> {
    let src = source_dir("claude-code", env)?;
    let owned = claude_code::project_sessions(env);
    let mut stats = Stats::default();
    for (path, id) in &owned {
        copy_if_changed(path, &dst.join(format!("{id}.jsonl")), &mut stats)?;
        // a session's sidecars (subagents/, tool-results/) live under a dir named for its id
        let side = src.join(id);
        if side.is_dir() {
            let s = mirror(&side, &dst.join(id))?;
            stats.total += s.total;
            stats.added += s.added;
            stats.updated += s.updated;
            stats.bytes += s.bytes;
        }
    }
    // `memory/` hangs off the slug, not off any session, so under a collision it belongs to nobody in
    // particular and cannot be attributed. Carry it only when we have the slug dir to ourselves.
    let mem = src.join("memory");
    if mem.is_dir() && claude_code::slug_dir_is_exclusive(env) {
        let s = mirror(&mem, &dst.join("memory"))?;
        stats.total += s.total;
        stats.added += s.added;
        stats.updated += s.updated;
        stats.bytes += s.bytes;
    }
    Ok((stats, format!("{} ({} owned sessions)", src.display(), owned.len())))
}

/// `agit -a snap --watch [--interval N]` — **fully automatic snap**: watch the runtime's session dump and,
/// whenever it changes and then settles, mirror + auto-commit into the Agent Store. Runs until Ctrl-C.
/// Runtime-agnostic; the pre-commit secret hook still applies (a snap carrying a secret is refused, with a warning).
/// `snap --watch --from <rt>`: validate first. An unknown name here is the worst case — the loop runs
/// forever polling a dump that cannot exist, reporting nothing, looking healthy.
pub fn snap_watch_checked(runtime: &str, interval_secs: u64) -> Result<i32> {
    let rt = resolve_runtime(Some(runtime), &[], "watch")?;
    snap_watch(&rt, interval_secs)
}

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

#[derive(Default)]
struct Stats {
    total: usize,
    added: usize,
    updated: usize,
    bytes: u64,
}

/// Codex sync: scan ~/.codex/sessions and flatten only **this project's** rollouts
/// (session_meta.cwd == env root) into dst/<id>.jsonl. Filtering by cwd is a privacy
/// bottom line — never pull in another project's sessions.
/// Copy one file if the destination is missing, a different size, or older. Shared by both runtimes'
/// collectors. Missing mtimes are treated as "re-copy", which is the conservative direction.
fn copy_if_changed(src: &Path, dp: &Path, st: &mut Stats) -> Result<()> {
    if let Some(parent) = dp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let smeta = std::fs::metadata(src)?;
    match std::fs::metadata(dp) {
        Err(_) => {
            std::fs::copy(src, dp)?;
            st.added += 1;
        }
        Ok(dmeta) => {
            let newer = match (smeta.modified(), dmeta.modified()) {
                (Ok(s), Ok(d)) => s > d,
                _ => true,
            };
            if dmeta.len() != smeta.len() || newer {
                std::fs::copy(src, dp)?;
                st.updated += 1;
            }
        }
    }
    st.total += 1;
    st.bytes += smeta.len();
    Ok(())
}

fn codex_collect(env: &Path, dst: &Path) -> Result<(Stats, String)> {
    let rollouts = crate::adapter::codex::project_rollouts(env);
    let mut st = Stats::default();
    for (src, id) in &rollouts {
        copy_if_changed(src, &dst.join(format!("{id}.jsonl")), &mut st)?;
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

// ── unified watcher: watch BOTH runtimes' live dumps, auto-snap, auto-convert both ways ──

/// `agit watch` — the fully hands-off loop. Watches both runtimes' live session dumps directly, and on
/// each settle: auto-snaps (mirror + commit, harness included) and (unless --no-convert) auto-converts
/// every session both ways so it's immediately resumable in either CLI. Foreground; Ctrl-C to stop.
pub fn watch(interval_secs: u64, do_convert: bool, capture_harness: bool) -> Result<i32> {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let interval = Duration::from_secs(interval_secs.max(1));
    let runtimes = RUNTIMES;
    let mut last: HashMap<&str, String> = HashMap::new();
    let mut pending: HashMap<&str, bool> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut count = 0u64;
    println!(
        "Watching {} every {}s: auto-snap{}. Ctrl-C to stop.",
        runtime_list(),
        interval.as_secs(),
        if do_convert { " + auto-convert both ways" } else { "" }
    );
    loop {
        for rt in runtimes {
            let sig = source_path(rt, &env).map(|p| dir_signature(&p)).unwrap_or_default();
            // first sight of a runtime counts as "changed" so pre-existing sessions get captured on start
            let changed = last.get(rt).map(|l| l != &sig).unwrap_or(true);
            if changed {
                last.insert(rt, sig);
                pending.insert(rt, true);
            } else if pending.get(rt).copied().unwrap_or(false) {
                pending.insert(rt, false);
                match mirror_once(rt, &env, &agent) {
                    Ok((stats, _, hits, _)) if stats.added + stats.updated > 0 => {
                        if capture_harness {
                            let _ = crate::harness::capture(&agent, &env, rt);
                        }
                        commit_snap(&agent, rt, hits, &mut count);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("  snap {rt} failed: {e:#}"),
                }
            }
        }
        if do_convert {
            crate::commands::convert_pass(&agent, &mut seen);
        }
        std::thread::sleep(interval);
    }
}

fn watch_rundir() -> Result<PathBuf> {
    // keep the pid/log inside the agent repo's .git so they're never tracked or scanned
    Ok(scope::root_for(Scope::Agent)?.join(".git"))
}

fn read_pid(p: &Path) -> Option<u32> {
    std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse().ok())
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `agit watch --daemon` — spawn the watcher detached (own process group, stdio to a log inside the
/// agent repo's .git) so it keeps running after the launching shell exits.
pub fn watch_daemon(interval_secs: u64, do_convert: bool, capture_harness: bool) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let env = scope::env_root()?;
    let rundir = watch_rundir()?;
    let logp = rundir.join("agit-watch.log");
    let pidp = rundir.join("agit-watch.pid");
    if let Some(pid) = read_pid(&pidp) {
        if pid_alive(pid) {
            println!("agit watch already running (pid {pid}). Stop it with: agit watch --stop");
            return Ok(0);
        }
    }
    let exe = std::env::current_exe().context("cannot locate the agit binary to spawn")?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&logp)?;
    let log2 = log.try_clone()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("watch").arg("--interval").arg(interval_secs.to_string());
    if !do_convert {
        cmd.arg("--no-convert");
    }
    if !capture_harness {
        cmd.arg("--no-harness");
    }
    cmd.current_dir(&env) // child resolves the same repos from the project root
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2)
        .process_group(0); // new process group → survives the launching shell's SIGHUP
    let child = cmd.spawn().context("failed to spawn the background watcher")?;
    let pid = child.id();
    std::fs::write(&pidp, pid.to_string())?;
    println!("agit watch started in the background (pid {pid}).");
    println!("  log:    {}", logp.display());
    println!("  status: agit watch --status   ·   stop: agit watch --stop");
    Ok(0)
}

/// `agit watch --stop` — kill the background watcher recorded for this project.
pub fn watch_stop() -> Result<i32> {
    let pidp = watch_rundir()?.join("agit-watch.pid");
    match read_pid(&pidp) {
        Some(pid) => {
            let killed = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let _ = std::fs::remove_file(&pidp);
            if killed {
                println!("Stopped agit watch (pid {pid}).");
            } else {
                println!("No live process for pid {pid}; cleared the stale pidfile.");
            }
            Ok(0)
        }
        None => {
            println!("No background watcher is recorded for this project.");
            Ok(0)
        }
    }
}

/// `agit watch --status` — report whether the background watcher is running.
pub fn watch_status() -> Result<i32> {
    let rundir = watch_rundir()?;
    match read_pid(&rundir.join("agit-watch.pid")) {
        Some(pid) if pid_alive(pid) => {
            println!("agit watch is running (pid {pid}).");
            println!("  log: {}", rundir.join("agit-watch.log").display());
        }
        _ => println!("agit watch is not running for this project."),
    }
    Ok(0)
}

#[cfg(test)]
mod claude_ownership_tests {
    use super::*;

    /// The leak this fix exists for: `/tmp/<x>/proj-a` and `/tmp/<x>/proj.a` collapse to ONE claude slug
    /// dir, so snapping project A used to mirror B's transcript into A's store (and push it to A's team).
    #[test]
    fn snap_only_takes_sessions_this_project_owns() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let a = base.path().join("proj-a"); // → slug -tmp-…-proj-a
        let b = base.path().join("proj.a"); // → slug -tmp-…-proj-a  (SAME)
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(
            claude_code::slug_for(&a),
            claude_code::slug_for(&b),
            "test is meaningless unless the two paths really collide"
        );

        // one shared slug dir holding a transcript from each project, plus one with no cwd at all
        let slug_dir = home.path().join(".claude/projects").join(claude_code::slug_for(&a));
        std::fs::create_dir_all(&slug_dir).unwrap();
        let rec = |cwd: &std::path::Path, msg: &str| {
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"{}\",\"message\":{{\"content\":\"{msg}\"}}}}\n",
                cwd.display()
            )
        };
        std::fs::write(slug_dir.join("mine.jsonl"), rec(&a, "mine")).unwrap();
        std::fs::write(slug_dir.join("theirs.jsonl"), rec(&b, "SECRET_OF_B")).unwrap();
        // drift: launch cwd is A, later records cd into a subdir → still A's
        std::fs::write(
            slug_dir.join("drift.jsonl"),
            rec(&a, "start") + &rec(&a.join("sub"), "after cd"),
        )
        .unwrap();
        std::fs::write(slug_dir.join("nocwd.jsonl"), "{\"type\":\"queue-operation\"}\n").unwrap();

        temp_env(home.path(), || {
            let owned: Vec<String> = claude_code::project_sessions(&a).into_iter().map(|(_, id)| id).collect();
            assert!(owned.contains(&"mine.jsonl".replace(".jsonl", "")), "own session must be captured");
            assert!(owned.contains(&"drift".to_string()), "cd-drift must not lose ownership");
            assert!(!owned.contains(&"theirs".to_string()), "LEAK: captured the colliding project's session");
            assert!(!owned.contains(&"nocwd".to_string()), "unattributable transcript must fail closed");
            // memory/ is slug-level: unattributable while a foreign session shares the dir
            assert!(!claude_code::slug_dir_is_exclusive(&a), "a foreign session shares this slug");
        });
    }

    /// $HOME drives claude's projects dir; set it only for the closure. Serialized because it is process-global.
    fn temp_env(home: &Path, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("HOME").ok();
        std::env::set_var("HOME", home);
        f();
        match old {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}


#[cfg(test)]
mod resolve_runtime_tests {
    use super::*;

    /// The pure branches of the no-default-runtime rule. `[only]` is the highest-risk line in the
    /// feature — it decides a runtime WITHOUT asking — so it is pinned here. (The `many` branch reads
    /// stdin and is covered by the integration suite.)
    #[test]
    fn explicit_wins_and_an_unknown_name_fails_loudly() {
        assert_eq!(resolve_runtime(Some("codex"), &[], "snap").unwrap(), "codex");
        // aliases normalize; presence is irrelevant on the explicit branch
        assert_eq!(resolve_runtime(Some("cc"), &[], "snap").unwrap(), "claude-code");
        assert_eq!(resolve_runtime(Some("claude"), &["codex"], "snap").unwrap(), "claude-code");

        let e = resolve_runtime(Some("bogus"), &["codex"], "snap").unwrap_err().to_string();
        assert!(e.contains("bogus"), "{e}");
        assert!(e.contains("claude-code") && e.contains("codex"), "the error must name the real runtimes: {e}");
    }

    #[test]
    fn none_present_fails_and_the_only_one_present_is_chosen_without_asking() {
        let e = resolve_runtime(None, &[], "snap").unwrap_err().to_string();
        assert!(e.contains("claude-code") && e.contains("codex"), "{e}");

        // exactly one present → chosen. NOT because it is claude: assert it for BOTH, so a silent
        // claude default could never satisfy this test.
        assert_eq!(resolve_runtime(None, &["codex"], "snap").unwrap(), "codex");
        assert_eq!(resolve_runtime(None, &["claude-code"], "snap").unwrap(), "claude-code");
    }
}
