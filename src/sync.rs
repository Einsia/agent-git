//! `agit -a sync <ref>` —— reconcile two diverged agent branches by DIALOGUE.
//!
//! No structured distillation, no CLAUDE.md. How it works (spike + first real run verified,
//! see docs/plans/2026-07-16-...):
//!   1. Three-dot diff to find the sessions the peer added since the common ancestor (the divergent tail).
//!   2. Copy each side's "latest" session into a fresh-id session bound to that side's own worktree
//!      (via the convert machinery, which rewrites id/cwd — the user's real sessions are never touched).
//!   3. Two worktrees: each agent runs in ITS OWN branch's checked-out tree, carrying its OWN diff since the
//!      common ancestor (ground truth). Read-only (Read/Grep/Glob) — resolve by reading code, escalate real conflicts.
//!   4. Output: the A-side session now contains the whole reconciliation → that IS the resumable merged state;
//!      the transcript is also archived for provenance.
//!
//! MVP: both sides claude-code; one "latest" session per side. Two worktrees + diffs when both code branches
//! are present in the repo; otherwise it falls back to a single tree (and says so).

use crate::convo::{self, ConvertOpts};
use crate::scope::{self, Scope};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Max dialogue rounds (one round = B replies once + A replies once).
const MAX_ROUNDS: usize = 4;
/// Cap on the diff (chars) fed to each agent.
const DIFF_CAP: usize = 6000;

fn normalize(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
}

/// One side of the dialogue: its resumed session id, the tree it runs in, its diff since the ancestor.
struct Side {
    id: String,
    cwd: PathBuf,
    diff: String,
}

pub fn run(reference: &str, runtime: &str, both: bool) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    if rt != "claude-code" {
        bail!("sync currently supports claude-code on both sides only (codex side is not wired yet).");
    }
    if !which("claude") {
        bail!("sync needs `claude` on this machine (both sides of the dialogue are real resumed claude sessions).");
    }
    if scope::git_in_status(&agent, &["rev-parse", "--verify", "--quiet", reference]).0 != 0 {
        bail!("Agent Store has no ref `{reference}`. Run `agit -a fetch <remote>` first.");
    }

    // 1. Divergent tail: sessions the peer added since the common ancestor (three-dot diff).
    let sdir = format!("sessions/{rt}");
    let (_, diff) =
        scope::git_in_status(&agent, &["diff", "--name-only", &format!("HEAD...{reference}"), "--", &sdir]);
    let incoming: Vec<String> = diff.lines().filter(|l| l.ends_with(".jsonl")).map(String::from).collect();
    if incoming.is_empty() {
        println!("{reference} added no sessions since the common ancestor — nothing to sync.");
        return Ok(0);
    }

    // 2. One representative session per side: A = latest local (not from the peer); B = latest incoming.
    let a_path = latest_local_session(&agent, &rt, &incoming)
        .context("no local session to represent A (does this branch have its own session yet?)")?;
    let a_src = std::fs::read_to_string(&a_path)?;
    let b_rel = incoming.last().unwrap();
    let (rcb, b_src) = scope::git_in_status(&agent, &["show", &format!("{reference}:{b_rel}")]);
    if rcb != 0 || b_src.trim().is_empty() {
        bail!("could not read the peer session `{b_rel}`.");
    }

    // 3. Two worktrees + diffs (when both code branches are present in the repo and differ).
    let branch_a = env_branch(&env).or_else(|| any_branch(&a_src));
    let branch_b = branch_a.as_deref().and_then(|a| peer_branch(&b_src, a));
    let (mut a, mut b) = (
        Side { id: convo::fresh_id("sync-a"), cwd: env.clone(), diff: String::new() },
        Side { id: convo::fresh_id("sync-b"), cwd: env.clone(), diff: String::new() },
    );
    let mut worktrees: Vec<PathBuf> = Vec::new();
    let grounded = ground_on_worktrees(&env, &branch_a, &branch_b, &mut a, &mut b, &mut worktrees)?;
    if grounded {
        eprintln!(
            "Two worktrees: A@{} · B@{} (each carries its own diff since the common ancestor).",
            branch_a.as_deref().unwrap_or("?"),
            branch_b.as_deref().unwrap_or("?")
        );
    } else {
        eprintln!("! Both code branches aren't in this repo (or share a name) — falling back to a single tree: the agents only see the current checkout, not each other's code.");
    }

    // 4. Revive both sessions, bound to their own trees (never touching the user's real sessions).
    install_copy(&rt, &a_src, &a.id, &a.cwd)?;
    install_copy(&rt, &b_src, &b.id, &b.cwd)?;
    eprintln!("Reviving both sessions (read-only): A={} … B={} …", &a.id[..8], &b.id[..8]);

    // 5–6. Dialogue → synthesize → emit → inline resolution. Worktrees must stay alive through the
    // decision turn (which resumes A in its tree), so clean them up only after this whole block.
    let result = (|| -> Result<i32> {
        let transcript = run_dialogue(&a, &b)?;
        let (resolved, open) = synthesize(&transcript)?;
        let archive = save_transcript(&agent, &rt, &a.id, &b.id, &transcript)?;
        println!("── sync result ──");
        if !resolved.is_empty() {
            println!("Agreed:\n{resolved}");
        }
        println!("\nTranscript archived → {}", archive.display());
        println!("Merged state (both contexts + the reconciliation) — resume it to continue:");
        println!("  (cd {} && claude --resume {})", env.display(), a.id);
        if both {
            if let Err(e) = emit_both(&rt, &b, &env) {
                eprintln!("(--both) couldn't materialize B's merged state: {e}");
            }
        }
        resolve_inline(&a, &open)
    })();
    for wt in &worktrees {
        let _ = scope::git_in_status(&env, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
    }
    result
}

/// If there are open conflicts and we're at a terminal, walk the user through deciding each and record
/// the decisions into A's session (the merged state). Non-interactive: just surface them.
fn resolve_inline(a: &Side, open: &str) -> Result<i32> {
    use std::io::{stdin, stdout, BufRead, IsTerminal, Write};
    let items: Vec<String> = open
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*']).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if items.is_empty() {
        println!("\nNothing left for you to decide.");
        return Ok(0);
    }
    if !stdin().is_terminal() {
        println!("\nNeeds your decision:\n{open}");
        return Ok(1);
    }
    println!("\n{} open conflict(s). Decide each (blank = leave open):", items.len());
    let mut decisions: Vec<String> = Vec::new();
    let sin = stdin();
    for (i, it) in items.iter().enumerate() {
        println!("\n[{}/{}] {it}", i + 1, items.len());
        print!("  your call> ");
        let _ = stdout().flush();
        let mut line = String::new();
        if sin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let d = line.trim();
        if !d.is_empty() {
            decisions.push(format!("- {it}\n  → decided: {d}"));
        }
    }
    if decisions.is_empty() {
        println!("\nAll left open:\n{open}");
        return Ok(1);
    }
    // Bake the human's decisions into A's session so `resume` continues with them settled.
    let joined = decisions.join("\n");
    let _ = turn(
        &a.cwd,
        &a.id,
        &format!(
            "The human resolved the open conflicts as follows. Record these as the agreed decisions going \
             forward (do not edit files):\n{joined}\nReply 'noted'."
        ),
    );
    println!("\nRecorded {} decision(s) into the merged state.", decisions.len());
    if decisions.len() < items.len() {
        Ok(1) // some left open
    } else {
        Ok(0)
    }
}

/// Try to bind each side to its own branch's worktree + compute its diff. Fills a/b/worktrees on success.
fn ground_on_worktrees(
    env: &Path,
    branch_a: &Option<String>,
    branch_b: &Option<String>,
    a: &mut Side,
    b: &mut Side,
    worktrees: &mut Vec<PathBuf>,
) -> Result<bool> {
    let (Some(ba), Some(bb)) = (branch_a, branch_b) else { return Ok(false) };
    if ba == bb {
        return Ok(false); // same branch → nothing diverged to reconcile against
    }
    let (Some(ta), Some(tb)) = (branch_tip(env, ba), branch_tip(env, bb)) else { return Ok(false) };
    let (mrc, mbase) = scope::git_in_status(env, &["merge-base", ba, bb]);
    if mrc == 0 && !mbase.is_empty() {
        a.diff = git_diff(env, &mbase, ba);
        b.diff = git_diff(env, &mbase, bb);
    }
    // A: if the current checkout is already on ba, use env; otherwise give it a detached worktree.
    if env_branch(env).as_deref() == Some(ba.as_str()) {
        a.cwd = env.to_path_buf();
    } else {
        let wa = add_worktree(env, &ta, "a")?;
        worktrees.push(wa.clone());
        a.cwd = wa;
    }
    let wb = add_worktree(env, &tb, "b")?;
    worktrees.push(wb.clone());
    b.cwd = wb;
    Ok(true)
}

/// Run the dialogue, returning the full transcript. Split out so worktree cleanup can run unconditionally.
fn run_dialogue(a: &Side, b: &Side) -> Result<Vec<(char, String)>> {
    let mut transcript: Vec<(char, String)> = Vec::new();
    let mut msg = turn(&a.cwd, &a.id, &open_prompt(&a.diff))?;
    println!("\nA → {msg}\n");
    transcript.push(('A', msg.clone()));

    let mut first_b = true;
    for _ in 0..MAX_ROUNDS {
        let bp = if first_b { relay_first(&msg, &b.diff) } else { relay(&msg) };
        first_b = false;
        let bmsg = turn(&b.cwd, &b.id, &bp)?;
        println!("B → {bmsg}\n");
        transcript.push(('B', bmsg.clone()));
        let amsg = turn(&a.cwd, &a.id, &relay(&bmsg))?;
        println!("A → {amsg}\n");
        transcript.push(('A', amsg.clone()));
        msg = amsg;
        if is_done(&bmsg) && is_done(&msg) {
            break;
        }
    }
    Ok(transcript)
}

/// `--both`: B's session already carries the reconciliation (it took every B turn). Re-bind that merged
/// state to the repo under a fresh id so B's branch can also resume with the combined context.
fn emit_both(rt: &str, b: &Side, env: &Path) -> Result<()> {
    let path = crate::adapter::claude_code::projects_dir()?
        .join(crate::adapter::claude_code::slug_for(&b.cwd))
        .join(format!("{}.jsonl", b.id));
    let content = std::fs::read_to_string(&path)?;
    let b_merged = convo::fresh_id("sync-b-merged");
    install_copy(rt, &content, &b_merged, env)?;
    println!("B's merged state — resume it on B's branch:");
    println!("  (cd {} && claude --resume {b_merged})", env.display());
    Ok(())
}

/// Copy a claude session into a fresh-id session bound to `cwd` (same-vendor replay rewrites id/cwd).
fn install_copy(rt: &str, src: &str, new_id: &str, cwd: &Path) -> Result<()> {
    let ir = convo::read_conversation(rt, src)?;
    let opts = ConvertOpts { cwd: Some(cwd.to_string_lossy().into_owned()), new_id: new_id.to_string() };
    let bytes = convo::write_conversation(rt, &ir, &opts)?;
    crate::register::install(rt, new_id, cwd, &bytes)?;
    Ok(())
}

/// Run one turn of a resumed claude session (read-only tools); return its reply text.
fn turn(cwd: &Path, session_id: &str, prompt: &str) -> Result<String> {
    let out = Command::new("claude")
        .current_dir(cwd)
        .args(["--resume", session_id, "-p", prompt, "--output-format", "json", "--allowedTools", "Read", "Grep", "Glob"])
        .output()
        .context("failed to start `claude` (is it on PATH?)")?;
    if !out.status.success() {
        bail!("`claude --resume` exited non-zero: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(r) = v.get("result").and_then(|r| r.as_str()) {
            return Ok(r.trim().to_string());
        }
    }
    Ok(text.trim().to_string())
}

const RULES: &str = "Rules: at most 4 sentences per turn; put any real conflict (something that can't both be \
true, or that would break at merge) on its own line starting with 'CONFLICT:'; you have READ-ONLY access to \
your branch's tree (Read/Grep/Glob) — check the code instead of guessing; end your message with 'DONE' when \
you have nothing left to raise or resolve.";

fn diff_block(diff: &str) -> String {
    if diff.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nYour branch's diff since the common ancestor (ground truth — trust this over your memory):\n```diff\n{diff}\n```")
    }
}

fn open_prompt(diff_a: &str) -> String {
    format!(
        "You are agent A. You and another agent, B, each changed this repo from a common starting point, on \
         separate branches, and are about to merge. Reconcile with B and find the real conflicts. {RULES}{}\n\n\
         Start: briefly say what you changed since the fork, and ask B what they changed so you can check for conflicts.",
        diff_block(diff_a)
    )
}

fn relay_first(other: &str, diff_b: &str) -> String {
    format!("The other agent said: \"{other}\"\n\nRespond under the same rules. {RULES}{}", diff_block(diff_b))
}

fn relay(other: &str) -> String {
    format!("The other agent said: \"{other}\"\n\nRespond under the same rules. {RULES}")
}

fn is_done(msg: &str) -> bool {
    let tail: String = msg.trim_end().chars().rev().take(12).collect::<String>().chars().rev().collect();
    tail.contains("DONE")
}

/// Synthesize (agreed, still-open) from the dialogue. Uses the one-shot LLM backend; falls back to CONFLICT: markers.
fn synthesize(transcript: &[(char, String)]) -> Result<(String, String)> {
    let convo_text: String =
        transcript.iter().map(|(who, m)| format!("{who}: {m}")).collect::<Vec<_>>().join("\n\n");
    if !crate::llm::available() {
        let open: Vec<String> = transcript
            .iter()
            .flat_map(|(_, m)| m.lines())
            .filter(|l| l.trim_start().starts_with("CONFLICT:"))
            .map(|l| format!("- {}", l.trim_start().trim_start_matches("CONFLICT:").trim()))
            .collect();
        return Ok((String::from("(no LLM backend, not synthesized)"), open.join("\n")));
    }
    let prompt = format!(
        "Below is a reconciliation dialogue between two agents merging parallel branches. Summarize for a human \
         in two sections:\n\
         [RESOLVED] decisions they agreed on (one per line; write 'none' if empty).\n\
         [OPEN] conflicts still needing a human decision (one per line; write 'none' if empty).\n\
         Be terse; don't replay the dialogue.\n\n{convo_text}"
    );
    let reply = crate::llm::ask(&prompt)?;
    Ok(split_sections(&reply))
}

/// Split the synthesis reply into (agreed, open) on the [OPEN] marker.
fn split_sections(reply: &str) -> (String, String) {
    match reply.find("[OPEN]") {
        Some(i) => {
            let resolved = reply[..i].replace("[RESOLVED]", "").trim().to_string();
            let open = reply[i..].replace("[OPEN]", "").trim().to_string();
            let open = if open.trim().eq_ignore_ascii_case("none") { String::new() } else { open };
            (resolved, open)
        }
        None => (reply.trim().to_string(), String::new()),
    }
}

/// Archive the whole dialogue into the Agent Store (versioned provenance: HOW the two agents aligned).
fn save_transcript(agent: &Path, rt: &str, a_id: &str, b_id: &str, transcript: &[(char, String)]) -> Result<PathBuf> {
    let dir = agent.join("sessions").join("sync");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{}.md", &a_id[..8], &b_id[..8]));
    let mut md = format!("# sync reconciliation ({rt})\n\nA={a_id}\nB={b_id}\n\n");
    for (who, m) in transcript {
        md.push_str(&format!("**{who}:** {m}\n\n"));
    }
    std::fs::write(&path, md)?;
    Ok(path)
}

fn latest_local_session(agent: &Path, rt: &str, incoming: &[String]) -> Option<PathBuf> {
    let incoming_names: std::collections::HashSet<&str> =
        incoming.iter().map(|p| p.rsplit('/').next().unwrap_or(p)).collect();
    let dir = agent.join("sessions").join(rt);
    walkdir::WalkDir::new(&dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .filter(|e| !incoming_names.contains(e.file_name().to_string_lossy().as_ref()))
        .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .map(|e| e.path().to_path_buf())
}

// ── git / worktree helpers ──

fn env_branch(env: &Path) -> Option<String> {
    let (rc, b) = scope::git_in_status(env, &["branch", "--show-current"]);
    (rc == 0 && !b.trim().is_empty()).then(|| b.trim().to_string())
}

/// The first `gitBranch` recorded in a session (best-effort branch of the work).
fn any_branch(jsonl: &str) -> Option<String> {
    session_branches(jsonl).into_iter().next()
}

/// A session branch that differs from `exclude` — used to pick the peer's code branch robustly
/// (a session can span branches, so we don't just take the first).
fn peer_branch(jsonl: &str, exclude: &str) -> Option<String> {
    session_branches(jsonl).into_iter().find(|b| b != exclude)
}

fn session_branches(jsonl: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in jsonl.lines().take(400) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if let Some(b) = v.get("gitBranch").and_then(|x| x.as_str()) {
                if !b.is_empty() && seen.insert(b.to_string()) {
                    out.push(b.to_string());
                }
            }
        }
    }
    out
}

fn branch_tip(env: &Path, b: &str) -> Option<String> {
    let (rc, sha) = scope::git_in_status(env, &["rev-parse", "--verify", "--quiet", b]);
    (rc == 0 && !sha.is_empty()).then(|| sha)
}

fn git_diff(env: &Path, from: &str, to: &str) -> String {
    let (_, d) = scope::git_in_status(env, &["diff", &format!("{from}..{to}")]);
    convo::truncate(&d, DIFF_CAP)
}

fn add_worktree(env: &Path, at_commit: &str, tag: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("agit-sync-{tag}-{}", &convo::fresh_id(tag)[..8]));
    let (rc, out) =
        scope::git_in_status(env, &["worktree", "add", "--detach", &path.to_string_lossy(), at_commit]);
    if rc != 0 {
        bail!("git worktree add failed: {out}");
    }
    Ok(path)
}

fn which(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
