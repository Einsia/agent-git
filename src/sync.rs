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
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Max dialogue rounds (one round = B replies once + A replies once). Default mode opens with a
/// mutual context handoff, so it needs headroom beyond the reconcile itself.
const MAX_ROUNDS: usize = 6;
/// `--quick`: skip the handoff, go straight to conflict-hunting with fewer rounds.
const QUICK_ROUNDS: usize = 4;
/// Cap on the diff (chars) fed to each agent.
const DIFF_CAP: usize = 6000;

fn normalize(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
}

/// One side of the dialogue: its resumed session id, the tree it runs in, its diff since the ancestor,
/// and a brief of its *other* same-branch sessions (a branch's work may span several sessions).
struct Side {
    id: String,
    cwd: PathBuf,
    diff: String,
    brief: String,
}

pub fn run(reference: &str, runtime: &str, both: bool, quick: bool) -> Result<i32> {
    let env = scope::env_root()?;
    let agent = scope::root_for(Scope::Agent)?;
    let rt = normalize(runtime);
    if rt != "claude-code" && rt != "codex" {
        bail!("sync supports claude-code or codex on both sides (pick with --from <rt>).");
    }
    let cli = if rt == "codex" { "codex" } else { "claude" };
    if !which(cli) {
        bail!("sync needs `{cli}` on this machine (both sides of the dialogue are real resumed {cli} sessions).");
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

    // 2. Gather ALL of each side's sessions (a branch's work may span several). A = local (not from the
    //    peer); B = the peer's incoming sessions from <ref>.
    let a_all = local_sessions(&agent, &rt, &incoming);
    if a_all.is_empty() {
        bail!("no local session to represent A (does this branch have its own session yet?)");
    }
    let mut b_all: Vec<(String, String)> = Vec::new();
    for rel in &incoming {
        let (rc, content) = scope::git_in_status(&agent, &["show", &format!("{reference}:{rel}")]);
        if rc == 0 && !content.trim().is_empty() {
            b_all.push((rel.rsplit('/').next().unwrap_or(rel).to_string(), content));
        }
    }
    if b_all.is_empty() {
        bail!("could not read any peer session from `{reference}`.");
    }

    // 3. Branches, then pick the "voice" session per side (richest on that branch) + brief the rest.
    let branch_a = env_branch(&env).or_else(|| a_all.iter().find_map(|(_, c)| any_branch(c)));
    let branch_b = branch_a.as_deref().and_then(|a| b_all.iter().find_map(|(_, c)| peer_branch(c, a)));
    let (a_voice, a_brief) = pick_side(&rt, &a_all, branch_a.as_deref());
    let (b_voice, b_brief) = pick_side(&rt, &b_all, branch_b.as_deref());

    // 4. Two worktrees + diffs (when both code branches are present in the repo and differ).
    let (mut a, mut b) = (
        Side { id: convo::fresh_id("sync-a"), cwd: env.clone(), diff: String::new(), brief: a_brief },
        Side { id: convo::fresh_id("sync-b"), cwd: env.clone(), diff: String::new(), brief: b_brief },
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

    // 5. Revive the voice session per side, bound to its own tree (never touching the user's real sessions).
    install_copy(&rt, &a_voice, &a.id, &a.cwd)?;
    install_copy(&rt, &b_voice, &b.id, &b.cwd)?;
    eprintln!(
        "Reviving both sessions (read-only): A={} ({} local) … B={} ({} incoming) …",
        &a.id[..8], a_all.len(), &b.id[..8], b_all.len()
    );

    // 5–6. Dialogue → synthesize → emit → inline resolution. Worktrees must stay alive through the
    // decision turn (which resumes A in its tree), so clean them up only after this whole block.
    let result = (|| -> Result<i32> {
        let transcript = run_dialogue(&rt, &a, &b, quick)?;
        let (resolved, open) = synthesize(&transcript)?;
        let archive = save_transcript(&agent, &rt, &a.id, &b.id, &transcript)?;
        println!("── sync result ──");
        if !resolved.is_empty() {
            println!("Agreed:\n{resolved}");
        }
        println!("\nTranscript archived → {}", archive.display());
        println!("Merged state (both contexts + the reconciliation) — resume it to continue:");
        println!("  {}", resume_cmd(&rt, &env, &a.id));
        if both {
            if let Err(e) = emit_both(&rt, &b, &env) {
                eprintln!("(--both) couldn't materialize B's merged state: {e}");
            }
        }
        resolve_inline(&rt, &a, &open)
    })();
    for wt in &worktrees {
        let _ = scope::git_in_status(&env, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
    }
    result
}

/// If there are open conflicts and we're at a terminal, walk the user through deciding each and record
/// the decisions into A's session (the merged state). Non-interactive: just surface them.
fn resolve_inline(rt: &str, a: &Side, open: &str) -> Result<i32> {
    use std::io::{stdin, stdout, BufRead, IsTerminal, Write};
    let items: Vec<String> = open
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*']).trim().to_string())
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none") && !l.eq_ignore_ascii_case("n/a"))
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
        rt,
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
/// Default mode opens with a mutual context handoff (A briefs B on its whole working context, not just
/// its diff; B hands its context back) before reconciling. `--quick` skips straight to conflict-hunting.
fn run_dialogue(rt: &str, a: &Side, b: &Side, quick: bool) -> Result<Vec<(char, String)>> {
    let rounds = if quick { QUICK_ROUNDS } else { MAX_ROUNDS };
    let mut transcript: Vec<(char, String)> = Vec::new();
    // default mode opens with a HANDOFF (a summarization turn) → no tools, so the clamp is real.
    let mut msg = turn_with(rt, &a.cwd, &a.id, &open_prompt(rt, &a.diff, &a.brief, quick), quick)?;
    println!("\nA → {msg}\n");
    transcript.push(('A', msg.clone()));

    let mut first_b = true;
    for _ in 0..rounds {
        let bp = if first_b { relay_first(rt, &msg, &b.diff, &b.brief, quick) } else { relay(&msg) };
        let bmsg = turn_with(rt, &b.cwd, &b.id, &bp, !(first_b && !quick))?;
        println!("B → {bmsg}\n");
        transcript.push(('B', bmsg.clone()));
        // In the default mode B's first reply is its handoff summary, so A receives it framed as one.
        let ap = if first_b && !quick { relay_handoff(rt, "B", &bmsg) } else { relay(&bmsg) };
        first_b = false;
        let amsg = turn(rt, &a.cwd, &a.id, &ap)?;
        println!("A → {amsg}\n");
        transcript.push(('A', amsg.clone()));
        msg = amsg;
        if is_done(&bmsg) && is_done(&msg) {
            break;
        }
    }
    Ok(transcript)
}

/// The command to resume a merged session, per runtime.
fn resume_cmd(rt: &str, cwd: &Path, id: &str) -> String {
    match rt {
        "codex" => format!("(cd {} && codex exec resume {id})", cwd.display()),
        _ => format!("(cd {} && claude --resume {id})", cwd.display()),
    }
}

/// `--both`: B's session already carries the reconciliation (it took every B turn). Re-bind that merged
/// state to the repo under a fresh id so B's branch can also resume with the combined context.
fn emit_both(rt: &str, b: &Side, env: &Path) -> Result<()> {
    if rt == "codex" {
        bail!("--both is claude-code only for now");
    }
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

/// Run one read-only turn of a resumed session (dispatch by runtime); return its reply text.
/// Handoff turns answer with an <analysis> scratchpad the peer must never see, so every reply is stripped
/// here — the one place the transcript, the relay, and `is_done` all read from.
fn turn(rt: &str, cwd: &Path, session_id: &str, prompt: &str) -> Result<String> {
    turn_with(rt, cwd, session_id, prompt, true)
}

/// `tools = false` actually withholds the tools, rather than merely asking the model not to use them.
/// The vendors' no-tools clamp is backed by a real constraint — compaction genuinely runs without tools,
/// so their "tool calls will be REJECTED" is true. A handoff turn that only *says* it while `--allowedTools
/// Read Grep Glob` is passed is a bluff: the model can burn its one turn on a Read and answer nothing.
fn turn_with(rt: &str, cwd: &Path, session_id: &str, prompt: &str, tools: bool) -> Result<String> {
    let reply = match rt {
        "codex" => turn_codex(cwd, session_id, prompt),
        _ => turn_claude(cwd, session_id, prompt, tools),
    }?;
    Ok(strip_analysis_or_raw(&reply))
}

/// The tools each runtime actually grants a reconcile turn — the clamp must name these, not the other
/// runtime's (codex has no Read/Grep/Glob; naming them there is noise the model can't act on).
fn tool_names(rt: &str) -> &'static str {
    match rt {
        "codex" => "shell, apply_patch",
        _ => "Read, Grep, Glob",
    }
}

/// Claude: `claude --resume <id> -p <prompt>` with read-only tools.
fn turn_claude(cwd: &Path, session_id: &str, prompt: &str, tools: bool) -> Result<String> {
    let mut cmd = Command::new("claude");
    cmd.current_dir(cwd)
        .args(["--resume", session_id, "-p", prompt, "--output-format", "json"]);
    if tools {
        cmd.args(["--allowedTools", "Read", "Grep", "Glob"]);
    }
    let out = cmd
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

/// Codex: `codex exec --sandbox read-only … -o <file> resume <id> -` (prompt via stdin, clean reply from -o).
fn turn_codex(cwd: &Path, session_id: &str, prompt: &str) -> Result<String> {
    use std::io::Write as _;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out_file = std::env::temp_dir().join(format!("agit-sync-codex-{}-{nanos}.txt", std::process::id()));
    let mut child = Command::new("codex")
        .current_dir(cwd)
        .args(["exec", "--sandbox", "read-only", "--skip-git-repo-check", "--color", "never", "-o"])
        .arg(&out_file)
        .args(["resume", session_id, "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to start `codex` (is it on PATH?)")?;
    child.stdin.take().context("no codex stdin")?.write_all(prompt.as_bytes())?;
    let status = child.wait()?;
    let reply = std::fs::read_to_string(&out_file).ok();
    let _ = std::fs::remove_file(&out_file);
    if !status.success() {
        bail!("`codex exec resume` exited non-zero");
    }
    reply.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).context("codex produced no reply")
}

const RULES: &str = "Rules: at most 4 sentences per turn; put any real conflict (something that can't both be \
true, or that would break at merge) on its own line starting with 'CONFLICT:'; you have READ-ONLY access to \
your branch's tree (Read/Grep/Glob) — check the code instead of guessing; end your message with 'DONE' when \
you have nothing left to raise or resolve.";

// ── The handoff prompt ──
//
// A handoff IS a compaction, so the structure below is borrowed from the vendors' shipped compaction
// prompts rather than hand-rolled:
//   * claude-code 2.1.211's `Bkg` variant (the "up_to"/prefix summarizer, reached via Gvu(_, "up_to")):
//     the analysis→summary shape, the 9-section schema, the security-verbatim clause, and the
//     three-layer no-tools clamp (`Tto`'s preamble + the body + the `jvu` reminder). `Bkg`, not the main
//     `Nkg`, because Bkg summarizes an earlier portion for a reader who will see newer messages the
//     summarizer cannot — semantically a handoff. Nkg assumes the summarizer IS the resumer, and drifts
//     when a different agent picks the work up (hence Bkg's tail: Work Completed / Context for
//     Continuing Work, not Nkg's Current Work / Optional Next Step).
//   * codex 0.144.4 (core/src/compact.rs): the split between a summarizer-side prompt and a separate
//     receiver-side one, and the receiver's "build on the work … avoid duplicating work" framing.
//
// Two deliberate departures from the sources:
//   1. Codex's receiver prompt promises "you also have access to the state of the tools that were used by
//      that language model" — true for compaction, FALSE here: each agent reads only its own worktree, so
//      that sentence is inverted below. Left as-is it invites an agent to read its own copy of a file and
//      mistake it for the peer's.
//   2. Codex's 425-byte prompt is purely additive (an `Include:` list, no drop list, no schema) because it
//      carries tool state out of band. Nothing but text crosses this boundary, so the drop list and the
//      fixed schema are ours to supply — unsaid substrate gets dropped at the model's discretion.

/// Layer 1 of the no-tools clamp (claude's `Tto` preamble), naming the tools this RUNTIME grants —
/// codex has no Read/Grep/Glob, so naming them there is noise the model cannot act on. Backed by a real
/// constraint: `turn_with(.., tools=false)` actually withholds them on a handoff turn, so "will be
/// REJECTED" is true rather than a bluff. A summarization turn that calls a tool burns the turn.
fn no_tools_preamble(rt: &str) -> String {
    format!(
        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

- Do NOT use {}, or ANY other tool.
- You already have all the context you need in the conversation above.
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.

",
        tool_names(rt)
    )
}

/// Layer 3 of the clamp (claude's `jvu`, appended last).
const NO_TOOLS_REMINDER: &str = "

REMINDER: Do NOT call any tools. Respond with plain text only — an <analysis> block followed by a <summary> \
block. Tool calls will be rejected and you will fail the task.";

/// The forced scratchpad pre-pass. Stripped from the reply before anything sees it (see `strip_analysis`);
/// it exists to make the model walk the conversation chronologically, not to be read.
const HANDOFF_ANALYSIS: &str = "Before your final summary, wrap your analysis in <analysis> tags to organize \
your thoughts and ensure you've covered all necessary points. In your analysis process:

1. Chronologically analyze each message and section of the conversation. For each, thoroughly identify:
   - What you were explicitly asked for, and your intent
   - Your approach, the key decisions you made, and the alternatives you considered and rejected
   - Specific details like file names, full code snippets, function signatures, and file edits
   - Errors you ran into and how you fixed them
   - Any correction you were given, especially where you were told to do something differently
   - Any security-relevant instruction or constraint you were given (e.g. sensitive files or data to avoid, \
operations that must not be performed, credential or secret handling rules). These MUST be preserved \
verbatim in the summary so they continue to apply after the handoff.
2. Double-check for technical accuracy and completeness, addressing each required element thoroughly.";

/// The handoff's fixed section schema, in order. Claude's `Bkg` sections 1–7 with its prefix-summarizer
/// tail (8. Work Completed / 9. Context for Continuing Work), reworded for a branch-to-branch handoff.
const HANDOFF_SECTIONS: [(&str, &str); 9] = [
    ("Primary Request and Intent", "What you were asked to do on this branch, in detail — the goal, not just the diff."),
    ("Key Technical Concepts", "The technologies, patterns, and invariants this work relies on."),
    ("Files and Code Sections", "Files you examined, changed, or created. Include the code that matters and why each file matters."),
    ("Errors and fixes", "Errors you hit and how you fixed them, in one line each."),
    ("Problem Solving", "Problems solved, and any troubleshooting still in flight."),
    (
        "All user messages",
        "Every instruction your human gave you on this branch that is not a tool result — these carry \
         feedback and changing intent. Preserve security-relevant instructions and constraints VERBATIM so \
         they still bind after the handoff. The reconciliation turns of this merge (this prompt, and \
         anything relayed from the other agent) are not your human's instructions: do not list them.",
    ),
    ("Pending Tasks", "What you were explicitly asked for that is not done."),
    ("Work Completed", "What is actually finished and settled by the end of your branch's work."),
    (
        "Context for Continuing Work",
        "The decisions, rejected alternatives, and state the other agent needs in order to continue and to \
         reconcile with you without redoing your reasoning.",
    ),
];

/// What must NOT cross. Codex omits this half; it can afford to, we cannot (see the module comment).
const HANDOFF_DROP: &str = "Leave OUT — it costs the other agent context and misleads:
- Approaches you superseded, unless the other agent might plausibly retry one; then say it was rejected and why.
- Errors you already fixed, beyond the one-line record in section 4.
- Verbose tool output, file dumps, and command logs already reflected on disk — the diff below is ground truth for those.
- Anything you cannot support from the conversation or your diff. Do not speculate to fill a section; write 'none'.";

fn section_schema() -> String {
    HANDOFF_SECTIONS
        .iter()
        .enumerate()
        .map(|(i, (name, what))| format!("{}. {name}: {what}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Receiver side (codex's bridge prompt), with its tool-state promise inverted — see the module comment.
fn receiver_frame(peer: &str) -> String {
    format!(
        "Agent {peer} produced a summary of its thinking process. Use this to build on the work that has \
         already been done and avoid duplicating work. Note that you do NOT have access to the state of the \
         tools {peer} used: {peer} worked in its own tree, and your Read/Grep/Glob see only YOUR branch's \
         tree — so {peer}'s summary and the diffs you are given are your only evidence about {peer}'s side. \
         Never assume a file you can read is {peer}'s version of it. Here is the summary produced by \
         {peer}, use the information in this summary to assist with your own analysis:"
    )
}

/// The summarizer-side handoff. `incoming` = the peer's handoff, when this side is answering one.
fn handoff_prompt(rt: &str, role: &str, peer: &str, incoming: Option<&str>, diff: &str, brief: &str) -> String {
    let preamble = no_tools_preamble(rt);
    let received = match incoming {
        Some(msg) => format!("{}\n\n\"{msg}\"\n\n", receiver_frame(peer)),
        None => String::new(),
    };
    format!(
        "{preamble}{received}You are agent {role}. You and agent {peer} each worked from a common \
         starting point on separate branches of this repo, and are about to merge. Your task is to create a \
         detailed handoff summary of YOUR work. It will be placed at the start of the reconciliation; \
         {peer}'s context and the turns that reconcile the two branches follow after it (you do not see \
         them here). Summarize thoroughly so that {peer}, reading only your summary, can understand what \
         you did and why, and reconcile it against their own work. Your diff is ground truth for WHAT \
         changed — the reasoning is the part {peer} cannot see.\n\n\
         {HANDOFF_ANALYSIS}\n\n\
         Your summary must contain these sections, in this order:\n\n{}\n\n\
         {HANDOFF_DROP}\n\n\
         Output an <analysis> block, then a <summary> block containing the numbered sections above.\
         {}{}{NO_TOOLS_REMINDER}",
        section_schema(),
        diff_block(diff),
        brief_block(brief)
    )
}

/// Strip the scratchpad and unwrap the summary (claude's `Ukg`): drop `<analysis>…</analysis>`, rewrite
/// `<summary>…</summary>` to its body, collapse blank runs. The analysis is a thinking aid — it is never
/// relayed to the peer and never archived.
fn strip_analysis(reply: &str) -> String {
    static ANALYSIS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<analysis>.*?</analysis>").unwrap());
    static SUMMARY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<summary>(.*?)</summary>").unwrap());
    static BLANKS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());

    // The summary is authoritative when present: take it and drop everything else. This is what makes an
    // UNCLOSED <analysis> safe — the paired regex below cannot match one, so without this branch the whole
    // scratchpad would flow on to the peer *and* into save_transcript(), i.e. into a store that gets pushed
    // to the team. Truncation (an unclosed tag) is exactly when the model rambled longest.
    if let Some(c) = SUMMARY.captures(reply) {
        let body = c[1].trim();
        if !body.is_empty() {
            return BLANKS.replace_all(body, "\n\n").trim().to_string();
        }
    }
    let out = ANALYSIS.replace_all(reply, "");
    // No closing tag and no summary: hard-drop from the opening tag rather than pass the scratchpad through.
    let out = match out.find("<analysis>") {
        Some(i) => out[..i].to_string(),
        None => out.into_owned(),
    };
    let out = SUMMARY.replace(&out, |c: &regex::Captures| c[1].trim().to_string());
    BLANKS.replace_all(out.as_ref(), "\n\n").trim().to_string()
}

/// Strip the scratchpad, but never hand the dialogue an empty turn. A reply that is *only* an analysis
/// block (the model spent its whole turn thinking) would otherwise strip to "", and the peer would then be
/// told "here is the summary produced by A" followed by nothing — silent, total context loss.
fn strip_analysis_or_raw(reply: &str) -> String {
    let stripped = strip_analysis(reply);
    if stripped.is_empty() && !reply.trim().is_empty() {
        // Salvage: no summary survived, so relay the reply with the tags flattened rather than nothing.
        return reply.replace("<analysis>", "").replace("</analysis>", "").trim().to_string();
    }
    stripped
}

fn diff_block(diff: &str) -> String {
    if diff.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nYour branch's diff since the common ancestor (ground truth — trust this over your memory):\n```diff\n{diff}\n```")
    }
}

fn brief_block(brief: &str) -> String {
    if brief.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{brief}")
    }
}

fn open_prompt(rt: &str, diff_a: &str, brief_a: &str, quick: bool) -> String {
    if quick {
        return format!(
            "You are agent A. You and another agent, B, each changed this repo from a common starting point, on \
             separate branches, and are about to merge. Reconcile with B and find the real conflicts. {RULES}{}{}\n\n\
             Start: briefly say what you changed since the fork, and ask B what they changed so you can check for conflicts.",
            diff_block(diff_a),
            brief_block(brief_a)
        );
    }
    handoff_prompt(rt, "A", "B", None, diff_a, brief_a)
}

fn relay_first(rt: &str, other: &str, diff_b: &str, brief_b: &str, quick: bool) -> String {
    if quick {
        return format!(
            "The other agent said: \"{other}\"\n\nRespond under the same rules. {RULES}{}{}",
            diff_block(diff_b),
            brief_block(brief_b)
        );
    }
    handoff_prompt(rt, "B", "A", Some(other), diff_b, brief_b)
}

fn relay(other: &str) -> String {
    format!("The other agent said: \"{other}\"\n\nRespond under the same rules. {RULES}")
}

/// The peer's message IS a handoff summary: frame it as one (codex's receiver side) and open the
/// reconciliation. Used for the single turn that follows the peer's handoff.
fn relay_handoff(rt: &str, peer: &str, other: &str) -> String {
    format!(
        "{}\n\n\"{other}\"\n\nNow reconcile it against your own work: raise anything that can't both be \
         true, or that would break at merge, as a 'CONFLICT:' line. {RULES}",
        receiver_frame(peer)
    )
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

/// All local (not from the peer) sessions of this runtime, as (filename, content).
fn local_sessions(agent: &Path, rt: &str, incoming: &[String]) -> Vec<(String, String)> {
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
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            std::fs::read_to_string(e.path()).ok().map(|c| (name, c))
        })
        .collect()
}

/// Pick the "voice" session for a side and brief its other same-branch sessions.
/// A branch's work may span several sessions; we resume the richest one and tell it about the rest
/// (the diff remains the code ground-truth, so this only adds reasoning awareness, nothing lossy-critical).
fn pick_side(rt: &str, sessions: &[(String, String)], branch: Option<&str>) -> (String, String) {
    // Prefer sessions actually on this branch; if none match (branch unrecorded), consider them all.
    let on_branch: Vec<&(String, String)> = match branch {
        Some(b) => {
            let f: Vec<&(String, String)> =
                sessions.iter().filter(|(_, c)| any_branch(c).as_deref() == Some(b)).collect();
            if f.is_empty() { sessions.iter().collect() } else { f }
        }
        None => sessions.iter().collect(),
    };
    // Voice = the richest (most lines) — a proxy for the most complete context.
    let voice = on_branch.iter().max_by_key(|(_, c)| c.lines().count()).unwrap();
    let others: Vec<&&(String, String)> = on_branch.iter().filter(|s| s.0 != voice.0).collect();

    let mut brief = String::new();
    if !others.is_empty() {
        brief.push_str("Your earlier sessions on this branch (context — you are resuming the latest one):\n");
        for (name, content) in others.iter().map(|s| (&s.0, &s.1)) {
            let first = side_first_prompt(rt, content).unwrap_or_else(|| "(no prompt)".into());
            brief.push_str(&format!("- {}: {}\n", &name[..name.len().min(8)], convo::truncate(&first, 120)));
        }
    }
    (voice.1.clone(), brief)
}

/// The first real user prompt of a session (for the brief).
fn side_first_prompt(rt: &str, content: &str) -> Option<String> {
    let ir = match rt {
        "codex" => crate::adapter::codex::parse_rollout(content, "x"),
        _ => crate::adapter::claude_code::parse_jsonl(content, "x"),
    };
    ir.prompts.into_iter().next()
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
            // claude records "gitBranch"; codex records it under payload.git.branch (session_meta).
            let b = v
                .get("gitBranch")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("payload").and_then(|p| p.get("git")).and_then(|g| g.get("branch")).and_then(|x| x.as_str()));
            if let Some(b) = b {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_analysis_drops_the_scratchpad_and_unwraps_the_summary() {
        let reply = "<analysis>\nchronological walk, a thinking aid only\n</analysis>\n\n\
                     <summary>\n1. Primary Request and Intent:\n   ship the parser\n</summary>";
        let out = strip_analysis(reply);
        assert!(!out.contains("<analysis>"), "{out}");
        assert!(!out.contains("thinking aid"), "the analysis body must never be persisted: {out}");
        assert!(!out.contains("<summary>") && !out.contains("</summary>"), "{out}");
        assert!(out.starts_with("1. Primary Request and Intent:"), "{out}");
        assert!(out.ends_with("ship the parser"), "{out}");
    }

    #[test]
    fn strip_analysis_collapses_blank_runs_and_keeps_the_done_tail() {
        let out = strip_analysis("<analysis>x</analysis>\n\n\n\n\nnothing left to raise. DONE");
        assert_eq!(out, "nothing left to raise. DONE");
        // the dialogue loop's contract survives stripping (is_done reads the last 12 chars)
        assert!(is_done(&out));
    }

    #[test]
    fn strip_analysis_leaves_an_ordinary_reconcile_turn_alone() {
        let msg = "CONFLICT: we both renamed sync -> merge\nDONE";
        assert_eq!(strip_analysis(msg), msg);
    }

    #[test]
    fn handoff_keeps_the_nine_sections_in_order() {
        let p = open_prompt("claude-code", "", "", false);
        let mut prev = 0;
        for (i, (name, _)) in HANDOFF_SECTIONS.iter().enumerate() {
            let at = p.find(&format!("{}. {name}", i + 1)).unwrap_or_else(|| panic!("missing section: {name}"));
            assert!(at > prev, "section out of order: {name}");
            prev = at;
        }
    }

    #[test]
    fn handoff_uses_the_prefix_summarizer_tail_not_the_main_variant() {
        let p = open_prompt("claude-code", "", "", false);
        assert!(p.contains("8. Work Completed") && p.contains("9. Context for Continuing Work"), "{p}");
        // claude's main (Nkg) tail assumes the summarizer is also the resumer — wrong model for a handoff
        assert!(!p.contains("Current Work") && !p.contains("Optional Next Step"), "{p}");
    }

    /// The scratchpad must never survive a TRUNCATED reply. An unclosed <analysis> makes the paired
    /// regex fail to match, so the raw thinking would otherwise be relayed to the peer AND archived by
    /// save_transcript into a store that gets pushed to the team.
    #[test]
    fn unclosed_analysis_never_leaks_the_scratchpad() {
        let leak = "<analysis>secret reasoning the peer must not see";
        assert_eq!(strip_analysis(leak), "", "unclosed analysis leaked: {}", strip_analysis(leak));

        let with_summary = "<analysis>thinking\n<summary>the handoff</summary>";
        assert_eq!(strip_analysis(with_summary), "the handoff", "summary is authoritative when present");
        assert!(!strip_analysis(with_summary).contains("thinking"));

        // …but a turn that is ONLY analysis must not become an empty relay (total context loss).
        let only = "<analysis>all my thinking</analysis>";
        assert_eq!(strip_analysis(only), "");
        assert_eq!(strip_analysis_or_raw(only), "all my thinking", "salvage rather than relay nothing");
        assert_eq!(strip_analysis_or_raw("   "), "");
    }

    #[test]
    fn handoff_clamps_tools_on_all_three_layers() {
        let p = open_prompt("claude-code", "", "", false);
        assert!(p.starts_with("CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."), "{p}");
        assert!(p.contains("Do NOT use Read, Grep, Glob"), "the clamp must name the tools `turn` grants");
        // and it must name the RIGHT runtime's tools: codex has no Read/Grep/Glob to withhold
        let c = open_prompt("codex", "", "", false);
        assert!(c.contains("Do NOT use shell, apply_patch"), "{c}");
        assert!(!c.contains("Read, Grep, Glob"), "claude's tool names leaked onto the codex path: {c}");
        assert!(p.contains("Output an <analysis> block, then a <summary> block"), "body must restate the shape");
        assert!(p.trim_end().ends_with("Tool calls will be rejected and you will fail the task."), "{p}");
    }

    #[test]
    fn handoff_demands_security_constraints_cross_verbatim() {
        let p = open_prompt("claude-code", "", "", false);
        assert!(p.contains("credential or secret handling rules"), "{p}");
        // demanded twice, as claude does: once in the analysis pre-pass, once in the section that carries them
        assert!(p.contains("MUST be preserved verbatim in the summary"), "{p}");
        assert!(p.contains("VERBATIM so they still bind after the handoff"), "{p}");
    }

    #[test]
    fn handoff_names_what_to_drop() {
        let p = open_prompt("claude-code", "", "", false);
        assert!(p.contains("Leave OUT"), "{p}");
        assert!(p.contains("Approaches you superseded"), "{p}");
        assert!(p.contains("Errors you already fixed"), "{p}");
        assert!(p.contains("already reflected on disk"), "{p}");
    }

    #[test]
    fn b_side_handoff_is_framed_by_the_receiver_prompt_and_carries_its_own_diff() {
        let p = relay_first("claude-code", "A's summary", "diff --git a/x b/x", "", false);
        assert!(p.starts_with("CRITICAL: Respond with TEXT ONLY."), "clamp must lead: {p}");
        assert!(p.contains("build on the work that has already been done and avoid duplicating work"), "{p}");
        assert!(p.contains("A's summary") && p.contains("diff --git a/x b/x"), "{p}");
        assert!(p.contains("You are agent B."), "{p}");
    }

    #[test]
    fn the_receiver_side_does_not_claim_the_peers_tool_state() {
        let p = relay_handoff("claude-code", "B", "B's summary");
        // codex's framing, minus the promise that is false here: each agent sees only its own worktree
        assert!(p.contains("build on the work that has already been done and avoid duplicating work"), "{p}");
        assert!(p.contains("do NOT have access to the state of the tools B used"), "{p}");
        assert!(p.contains("CONFLICT:") && p.contains("DONE"), "reconcile turns keep the loop's conventions");
    }

    #[test]
    fn quick_keeps_the_old_narrow_conflict_hunt_opening() {
        let a = open_prompt("claude-code", "", "", true);
        assert!(!a.contains("CRITICAL"), "--quick is not a summarization turn: {a}");
        assert!(!a.contains("Work Completed") && !a.contains("<analysis>"), "{a}");
        assert!(a.contains("find the real conflicts") && a.contains("CONFLICT:") && a.contains("DONE"), "{a}");
        let b = relay_first("claude-code", "hi", "", "", true);
        assert!(!b.contains("CRITICAL") && b.contains("Respond under the same rules"), "{b}");
    }
}
