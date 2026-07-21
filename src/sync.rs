//! `agit a merge <target>` (alias `sync`) —— reconcile my memory with another MEMORY, by DIALOGUE.
//!
//! The target is an agent name or a ref — **never a code branch**: one agent that worked feature-a then
//! feature-b has ONE memory spanning both, so there is nothing there to reconcile. See the design's §7.
//!
//! No structured distillation, no CLAUDE.md. How it works (spike + first real run verified,
//! see docs/plans/2026-07-16-...):
//!   1. Resolve the target to a memory, and read its AID to decide what merging it MEANS (`merge_mode`):
//!      same agent → dialogue then a git merge (one memory again); different agent → dialogue only.
//!   2. Find the divergent tail: the peer's sessions to reconcile against (`peer_sessions`).
//!   3. Copy each side's "latest" session into a fresh-id session bound to that side's own worktree
//!      (via the convert machinery, which rewrites id/cwd — the user's real sessions are never touched).
//!   4. Two worktrees: each agent runs in ITS OWN branch's checked-out tree, carrying its OWN diff since the
//!      common ancestor (ground truth). Read-only (Read/Grep/Glob) — resolve by reading code, escalate real conflicts.
//!   5. Output: the A-side session now contains the whole reconciliation → that IS the resumable merged state;
//!      the transcript is also archived for provenance.
//!
//! MVP: both sides on one runtime; one "latest" session per side. Two worktrees + diffs when both code
//! branches are present in the repo; otherwise it falls back to a single tree (and says so).

use crate::agent;
use crate::convo::{self, ConvertOpts};
use crate::scope::{self};
use crate::{errln, outln};
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

/// Canonical runtime name, delegating to the one shared alias map (`adapter::normalize`). Unknown
/// names pass through unchanged, preserving the prior behavior.
fn normalize(runtime: &str) -> String {
    crate::adapter::normalize(runtime).map(str::to_string).unwrap_or_else(|| runtime.to_string())
}

/// One side of the dialogue: its resumed session id, the tree it runs in, its diff since the ancestor,
/// and a brief of its *other* same-branch sessions (a branch's work may span several sessions).
struct Side {
    id: String,
    cwd: PathBuf,
    diff: String,
    brief: String,
}

/// How the operand was disambiguated when it named both an agent and a ref (§7). Scripts pass one;
/// a terminal gets the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    Agent,
    Ref,
}

/// A merge target is always another MEMORY — never a code branch (a code branch is a note ON a session,
/// not a thing to reconcile with).
enum Target {
    /// A ref in my own store: a copy of some memory, reachable by git.
    Ref(String),
    /// Another agent, already on disk at `~/.agit/agents/<aid>/`. Deliberately NOT fetched into my
    /// store: dragging a different agent's whole history in here buys nothing.
    Peer(agent::Agent),
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::Ref(r) => r.clone(),
            Target::Peer(a) => a.name.clone(),
        }
    }
}

/// What a merge does to the two stores. Decided by the AID, never by whether a merge-base happens to
/// exist — the design's whole point: `agent.toml` is committed IN the store, so identity is readable at
/// any target.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    /// Same agent, another copy (a teammate's push) → dialogue, then git merge: one memory again.
    Fuse,
    /// A different agent → dialogue ONLY. Both agents stay intact; you merge the CODE yourself.
    DialogueOnly,
}

/// Mode from the two aids. An UNKNOWN aid (a store predating identity) must never match another unknown
/// one: fusing two unrelated memories is unrecoverable, while a needless dialogue-only costs a git merge
/// you can still run by hand. So unknown ⇒ DialogueOnly, with no exception.
///
/// `Target::Ref` used to fall through to Fuse on the reasoning that a ref in my own store is my own
/// history. It isn't: a teammate's copy arrives as a ref in my store too (`agit a fetch`), so a store
/// with no identity would fuse a *different* agent's memory in — the one outcome you cannot undo.
/// Fuse is reachable only by two aids that are present and equal.
fn merge_mode(mine: Option<&str>, theirs: Option<&str>) -> Mode {
    match (mine, theirs) {
        (Some(a), Some(b)) if a == b => Mode::Fuse,
        _ => Mode::DialogueOnly,
    }
}

/// Resolve the operand: a known agent name → that agent's store; else a ref in my store; BOTH → ask;
/// neither → an actionable error.
fn resolve_target(store: &Path, name: &str, prefer: Option<Prefer>) -> Result<Target> {
    let known = agent::list().unwrap_or_default();
    let as_agent = known.iter().find(|a| a.name == name || a.aid == name).cloned();
    let as_ref = scope::git_in_status(store, &["rev-parse", "--verify", "--quiet", name]).0 == 0;

    let known_names = || -> String {
        let names: Vec<&str> = known.iter().map(|a| a.name.as_str()).collect();
        if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
    };

    match (as_agent, as_ref, prefer) {
        // An explicit disambiguator is AUTHORITATIVE, not advisory. It exists so agit never has to
        // guess which memory you meant, so a miss is an error: honouring the other kind would answer
        // a question the user did not ask, and quietly reconcile the wrong memory.
        (Some(a), _, Some(Prefer::Agent)) => Ok(Target::Peer(a)),
        (None, is_ref, Some(Prefer::Agent)) => bail!(
            "--agent says `{name}` is an agent, but no agent by that name is tracked here.{}\n  \
             known agents: {}",
            if is_ref { format!("\n  There IS a ref `{name}`; did you mean `--ref {name}`?") } else { String::new() },
            known_names()
        ),
        (_, true, Some(Prefer::Ref)) => Ok(Target::Ref(name.to_string())),
        (is_agent, false, Some(Prefer::Ref)) => bail!(
            "--ref says `{name}` is a ref, but the Agent Store has no such ref.{}",
            if is_agent.is_some() { format!("\n  There IS an agent `{name}`; did you mean `--agent {name}`?") } else { String::new() }
        ),

        (Some(a), false, None) => Ok(Target::Peer(a)),
        (None, true, None) => Ok(Target::Ref(name.to_string())),
        (Some(a), true, None) => pick_ambiguous(store, name, a),
        (None, false, None) => bail!(
            "`{name}` is neither an agent nor a ref in this Agent Store.\n  \
             known agents: {}\n  \
             a teammate's copy of this agent arrives as a ref; fetch it first: agit a fetch <remote>",
            known_names()
        ),
    }
}

/// Both matched: never guess which memory the user meant.
///
/// The two rows carry what makes the choice informed rather than a coin toss (§11c) — which agent, how
/// much memory it has and how recently it worked; which ref, whose memory is at it, and how far ahead.
/// A bare "1) agent 2) ref" asks the user to guess at exactly the moment agit refused to.
fn pick_ambiguous(store: &Path, name: &str, a: agent::Agent) -> Result<Target> {
    let sessions = crate::commands::store_sessions(&a.store);
    let last = crate::commands::latest_session(&a.store)
        .map(|s| format!(" · last {}", crate::ui::ago(s.recency())))
        .unwrap_or_default();
    let (_, full_ref) = scope::git_in_status(store, &["rev-parse", "--symbolic-full-name", name]);
    // Whose memory is at that ref decides what merging it would MEAN (`merge_mode`), so it is the
    // single most decision-relevant fact about the row.
    let whose = match (aid_of_store(store), aid_at_ref(store, name)) {
        (Some(mine), Some(theirs)) if mine == theirs => "this agent".to_string(),
        (_, Some(theirs)) => format!("agent {}", short_aid(&theirs)),
        (_, None) => "no aid recorded".to_string(),
    };
    let ahead = match ref_ahead_sessions(store, name) {
        Some(n) => format!("{n} sessions ahead"),
        None => "no shared history".to_string(),
    };
    let rows = vec![
        vec![
            "agent".to_string(),
            name.to_string(),
            short_aid(&a.aid),
            format!("{} sessions{last}", sessions.len()),
        ],
        vec!["ref".to_string(), full_ref.trim().to_string(), whose, ahead],
    ];
    let options: Vec<String> = crate::ui::table(&[], &rows).lines().map(str::to_string).collect();
    let picked = crate::ui::pick(
        &format!("\"{name}\" is ambiguous:"),
        &options,
        &format!("Say which: --agent {name} or --ref {name}."),
    )?;
    if picked == 0 {
        Ok(Target::Peer(a))
    } else {
        Ok(Target::Ref(name.to_string()))
    }
}

/// How many sessions a ref carries that this store does not — the "3 sessions ahead" on the picker's
/// ref row. `None` = no shared history, where "ahead" has no meaning to report.
///
/// Counts SESSIONS, not commits: `rev-list --count` would answer with a number about git, and the user
/// is choosing between two memories. Runtime-blind on purpose — the picker runs before `--from` has
/// been applied, and a count that silently excluded the other runtime's sessions would be a lie.
fn ref_ahead_sessions(store: &Path, reference: &str) -> Option<usize> {
    (scope::git_in_status(store, &["merge-base", "HEAD", reference]).0 == 0).then_some(())?;
    let (rc, out) = scope::git_in_status(
        store,
        &[
            "diff",
            "--name-only",
            "--diff-filter=ACMR",
            &format!("HEAD...{reference}"),
            "--",
            crate::session::SESSIONS_SUBDIR,
        ],
    );
    (rc == 0).then(|| {
        let runtimes = crate::session::runtimes();
        out.lines()
            .filter(|l| runtimes.iter().any(|rt| is_session_of(l, rt)))
            .count()
    })
}

fn short_aid(aid: &str) -> String {
    if aid.is_empty() {
        return "(no aid)".to_string();
    }
    let s: String = aid.chars().take(12).collect();
    format!("{s}…")
}

/// The aid a store claims. `None` = no identity, which is NOT an identity that can match: the legacy
/// scaffold wrote `id = "unnamed-agent"` into every store on earth, so a parser that accepted it would
/// make every legacy store "the same agent" — and fuse two unrelated memories. `parse_agent_toml` is
/// the one place that judgement lives; this reuses it rather than growing a second opinion.
fn aid_of_store(store: &Path) -> Option<String> {
    agent::read_identity(store).ok().map(|i| i.aid)
}

/// The aid at any ref: `agent.toml` is committed IN the store, so identity is readable at the target
/// without checking anything out (§7) — which is what lets mode follow the AID, not git history.
fn aid_at_ref(store: &Path, reference: &str) -> Option<String> {
    use crate::hub::identity::{parse_agent_toml, Identity};
    let (rc, out) = scope::git_in_status(store, &["show", &format!("{reference}:agent.toml")]);
    match (rc == 0).then(|| parse_agent_toml(&out)) {
        Some(Identity::Aid(a)) => Some(a),
        _ => None,
    }
}

pub fn run(reference: &str, runtime: &str, both: bool, quick: bool, splice: bool, dry_run: bool, prefer: Option<Prefer>) -> Result<i32> {
    let env = scope::env_root()?;
    // The RESOLVED agent's id-keyed store, not the legacy nested one. Reading `<env>/.agit/agent` made
    // merge operate on a store that nothing writes to any more, and left `mine` permanently None — so
    // the mode was decided by the unknown-identity fallback instead of by identity, which is the one
    // thing this command is supposed to decide by.
    let me = agent::resolve(None)?;
    let agent = me.store.clone();
    let rt = normalize(runtime);
    if rt != "claude-code" && rt != "codex" {
        bail!("sync supports claude-code or codex on both sides (pick with --from <rt>).");
    }
    let cli = if rt == "codex" { "codex" } else { "claude" };
    // The dialogue merge resumes real sessions, so it needs the runtime CLI. `--splice` does not run a
    // model at all, so it does not — and neither does `--dry-run`, which stops before the first turn.
    if !splice && !dry_run && !which(cli) {
        bail!("sync needs `{cli}` on this machine (both sides of the dialogue are real resumed {cli} sessions).");
    }

    // 1. Which memory is this, and what does merging it mean? Identity decides — not git history.
    let target = resolve_target(&agent, reference, prefer)?;
    let mine = aid_of_store(&agent);
    let theirs = match &target {
        Target::Ref(r) => aid_at_ref(&agent, r),
        Target::Peer(a) => Some(a.aid.clone()).filter(|s| !s.is_empty()),
    };
    let mode = merge_mode(mine.as_deref(), theirs.as_deref());
    announce(&target, theirs.as_deref(), mode);

    // 2. The divergent tail: the peer's sessions to reconcile against.
    let (b_all, grounded) = peer_sessions(&agent, &target, &rt)?;
    if b_all.is_empty() {
        // An empty tail means two very different things, and conflating them IS the bug this fixes.
        // Grounded in a shared ancestor, empty is the truth: nothing diverged, we are up to date.
        if grounded {
            outln!("{}: nothing to reconcile (no new {rt} sessions)", target.label());
            // A dry run reports what WOULD happen and mutates nothing — not even the git fuse.
            if dry_run {
                if mode == Mode::Fuse {
                    outln!("(dry run) would fuse histories (same agent); nothing else to do");
                }
                return Ok(0);
            }
            // Same agent still fuses: the peer may be ahead in commits that carry no session.
            return if mode == Mode::Fuse { fuse(&agent, &target) } else { Ok(0) };
        }
        // Ungrounded and empty is NOT evidence of agreement — it is a failed read. Never exit 0 on it.
        bail!(
            "{}: no {rt} sessions and no shared history, nothing to compare (--from <runtime> picks the other runtime)",
            target.label()
        );
    }

    // 2b. Gather ALL of A's sessions (a branch's work may span several); A = local, i.e. not the peer's.
    let incoming_names: Vec<String> = b_all.iter().map(|(n, _)| n.clone()).collect();
    let a_all = local_sessions(&agent, &rt, &incoming_names);
    if a_all.is_empty() {
        bail!("no local session to represent A (does this branch have its own session yet?)");
    }

    // 3. Branches, then pick the "voice" session per side (richest on that branch) + brief the rest.
    let branch_a = env_branch(&env).or_else(|| a_all.iter().find_map(|(_, c)| any_branch(c)));
    let branch_b = branch_a.as_deref().and_then(|a| b_all.iter().find_map(|(_, c)| peer_branch(c, a)));
    let (a_voice, a_brief) = pick_side(&rt, &a_all, branch_a.as_deref());
    let (b_voice, b_brief) = pick_side(&rt, &b_all, branch_b.as_deref());

    // `--dry-run` (alias `--preview`): everything above this point is pure inspection — git reads only,
    // no model, no writes. So this is the last safe branch: report what the merge WOULD do (target, mode,
    // each side's sessions + picked voice, whether the histories would fuse) and return. It installs
    // nothing, revives no session, runs no dialogue, saves no transcript, and leaves no worktree behind.
    if dry_run {
        let a_side = SidePreview { branch: branch_a.as_deref(), sessions: &a_all, voice: &a_voice };
        let b_side = SidePreview { branch: branch_b.as_deref(), sessions: &b_all, voice: &b_voice };
        outln!("{}", dry_run_report(&target.label(), mode, &a_side, &b_side));
        return Ok(0);
    }

    // `--splice`: the no-model merge. Combine both sides' voice sessions into one new session (A's full
    // transcript plus B's tail after the point they forked from, ids normalized) and install it.
    // Resuming it hands a fresh agent both contexts in one window. No dialogue, no reconciliation, no
    // runtime CLI, deterministic.
    if splice {
        let new_id = convo::fresh_id("session");
        let combined = splice_sessions(&rt, &a_voice, &b_voice, &new_id, &env)?;
        crate::register::install(&rt, &new_id, &env, &combined)?;
        outln!("── splice (no model) ──");
        outln!(
            "Combined {} local + {} incoming session(s) into one. Resume it and the agent reads both sides:",
            a_all.len(),
            b_all.len()
        );
        outln!("  {}", resume_cmd(&rt, &env, &new_id));
        // Same agent: fuse the git histories too, so the two copies become one memory again.
        return if mode == Mode::Fuse { Ok(fuse(&agent, &target)?) } else { Ok(0) };
    }

    // 4. Two worktrees + diffs (when both code branches are present in the repo and differ).
    let (mut a, mut b) = (
        Side { id: convo::fresh_id("sync-a"), cwd: env.clone(), diff: String::new(), brief: a_brief },
        Side { id: convo::fresh_id("sync-b"), cwd: env.clone(), diff: String::new(), brief: b_brief },
    );
    let mut worktrees: Vec<PathBuf> = Vec::new();
    let grounded = ground_on_worktrees(&env, &branch_a, &branch_b, &mut a, &mut b, &mut worktrees)?;
    if grounded {
        errln!(
            "Two worktrees: A@{} · B@{} (each carries its own diff since the common ancestor).",
            branch_a.as_deref().unwrap_or("?"),
            branch_b.as_deref().unwrap_or("?")
        );
    } else {
        errln!("{}", crate::ui::warn("warning: single tree (both code branches not present); agents see only this checkout"));
    }

    // 5. Revive the voice session per side, bound to its own tree (never touching the user's real sessions).
    install_copy(&rt, &a_voice, &a.id, &a.cwd)?;
    install_copy(&rt, &b_voice, &b.id, &b.cwd)?;
    errln!(
        "Reviving both sessions (read-only): A={} ({} local) … B={} ({} incoming) …",
        &a.id[..8], a_all.len(), &b.id[..8], b_all.len()
    );

    // 5–6. Dialogue → synthesize → emit → inline resolution. Worktrees must stay alive through the
    // decision turn (which resumes A in its tree), so clean them up only after this whole block.
    let result = (|| -> Result<i32> {
        let transcript = run_dialogue(&rt, &a, &b, quick)?;
        let (resolved, open) = synthesize(&transcript)?;
        let archive = save_transcript(&agent, &rt, &a.id, &b.id, &transcript)?;
        outln!("── merge result ──");
        if !resolved.is_empty() {
            outln!("Agreed:\n{resolved}");
        }
        outln!("\nTranscript archived → {}", archive.display());
        outln!("Merged state (both contexts + the reconciliation); resume it to continue:");
        outln!("  {}", resume_cmd(&rt, &env, &a.id));
        if both {
            if let Err(e) = emit_both(&rt, &b, &env) {
                errln!("(--both) couldn't materialize B's merged state: {e}");
            }
        }
        resolve_inline(&rt, &a, &open, &agent, &b.id)
    })();
    for wt in &worktrees {
        let _ = scope::git_in_status(&env, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
    }
    let code = result?;

    // 7. Same agent → the two copies become one memory again. A different agent NEVER gets here: its
    //    history stays its own, and you merge the CODE yourself.
    if mode == Mode::Fuse {
        return Ok(code.max(fuse(&agent, &target)?));
    }
    Ok(code)
}

/// State the mode. The user must never have to infer whether their agents just got fused.
fn announce(target: &Target, theirs: Option<&str>, mode: Mode) {
    let label = target.label();
    match mode {
        Mode::Fuse => outln!("{label}: same agent, fusing histories"),
        Mode::DialogueOnly => {
            let aid = theirs.map(short_aid).unwrap_or_else(|| "no aid".into());
            outln!("{label}: different agent ({aid}), dialogue only")
        }
    }
}

/// The `--dry-run` preview: what a merge WOULD do, built from the same inspection phases the real run
/// uses (resolve → peer/local sessions → pick the voice per side), with NOTHING run, installed, fused, or
/// left behind. Pure, so the text is testable without a model or a store.
/// One side as the preview sees it: its code branch (if recorded), its sessions, and the picked voice.
struct SidePreview<'a> {
    branch: Option<&'a str>,
    sessions: &'a [(String, String)],
    voice: &'a str,
}

impl SidePreview<'_> {
    /// The filename of the voice session (matched back from its content), for the audit line.
    fn voice_name(&self) -> String {
        self.sessions
            .iter()
            .find(|(_, c)| c.as_str() == self.voice)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| "?".into())
    }
}

fn dry_run_report(target: &str, mode: Mode, a: &SidePreview, b: &SidePreview) -> String {
    let mut s = String::from("── dry run (no model spent; nothing installed, fused, or left behind) ──\n");
    s.push_str(&format!("Target: {target}\n"));
    s.push_str(match mode {
        Mode::Fuse => "Mode: same agent; would reconcile by dialogue, THEN fuse the git histories.\n",
        Mode::DialogueOnly => {
            "Mode: different agent; would reconcile by dialogue only; the git histories stay separate.\n"
        }
    });
    s.push_str(&format!("A @ {}: {} session(s), voice = {}\n", a.branch.unwrap_or("?"), a.sessions.len(), a.voice_name()));
    s.push_str(&format!("B @ {}: {} session(s), voice = {}\n", b.branch.unwrap_or("?"), b.sessions.len(), b.voice_name()));
    s.push_str("Rerun without --dry-run to run the reconciliation.");
    s
}

/// Fuse the histories: one memory, restored by an ordinary git merge.
fn fuse(store: &Path, target: &Target) -> Result<i32> {
    let spec = match target {
        Target::Ref(r) => r.clone(),
        // Same aid on another path: it IS my memory, so fetching it is not "dragging in a stranger".
        Target::Peer(a) => {
            let rc = scope::git_in_inherit(store, &["fetch", &a.store.to_string_lossy(), "HEAD"]);
            if rc != 0 {
                bail!("could not fetch {} from {} (exit {rc}).", a.name, a.store.display());
            }
            "FETCH_HEAD".to_string()
        }
    };
    outln!("\nMerging the histories (same agent):");
    let rc = scope::git_in_inherit(store, &["merge", &spec]);
    if rc != 0 {
        errln!(
            "{}",
            crate::ui::warn(
                "  ⚠ git merge left conflicts in the Agent Store (reconciliation already recorded); resolve the files and agit a commit"
            )
        );
        return Ok(1);
    }
    Ok(0)
}

/// The peer's sessions of this runtime, as (basename, content) — the divergent tail — plus whether the
/// answer is **grounded in a shared ancestor**. That flag is what lets the caller tell "nothing
/// diverged" (trustworthy, exit 0) apart from "nothing was read" (never trustworthy).
///
/// The silent no-op this replaces: with NO merge base, `git diff HEAD...other` exits **128** and prints
/// **nothing on stdout** (verified), and the old code read that empty stdout as "no divergent tail" →
/// "nothing to sync", exit 0, does nothing. Exactly the cross-agent case. So: detect it via
/// `git merge-base` (rc=1 = unrelated histories) and enumerate two-dot instead — and never ignore a
/// non-zero rc from the diff itself.
///
/// `--diff-filter=ACMR` matters: a plain two-dot `--name-only` also lists paths the peer DELETED, which
/// don't exist at that ref, so `git show <ref>:<path>` then fails on them.
fn peer_sessions(store: &Path, target: &Target, rt: &str) -> Result<(Vec<(String, String)>, bool)> {
    match target {
        Target::Ref(r) => {
            let related = scope::git_in_status(store, &["merge-base", "HEAD", r]).0 == 0;
            let range = if related { format!("HEAD...{r}") } else { format!("HEAD..{r}") };
            if !related {
                errln!("No common ancestor with {r}: enumerating its sessions directly (two-dot).");
            }
            let (rc, out) = scope::git_in_status(
                store,
                &["diff", "--name-only", "--diff-filter=ACMR", &range, "--", crate::session::SESSIONS_SUBDIR],
            );
            if rc != 0 {
                bail!("could not compute the divergent tail against `{r}` (git exit {rc}).");
            }
            let mut v = Vec::new();
            for rel in out.lines().filter(|l| is_session_of(l, rt)) {
                let (rc, content) = scope::git_in_status(store, &["show", &format!("{r}:{rel}")]);
                if rc == 0 && !content.trim().is_empty() {
                    v.push((basename(rel), content));
                }
            }
            Ok((v, related))
        }
        // A different memory in a different repo: nothing to diff against, so its sessions ARE the tail.
        // Never "grounded": an empty read here is a failure, not agreement.
        Target::Peer(a) => {
            let (rc, out) =
                scope::git_in_status(&a.store, &["ls-tree", "-r", "--name-only", "HEAD", "--", crate::session::SESSIONS_SUBDIR]);
            let mut v = Vec::new();
            if rc == 0 {
                for rel in out.lines().filter(|l| is_session_of(l, rt)) {
                    let (rc, content) = scope::git_in_status(&a.store, &["show", &format!("HEAD:{rel}")]);
                    if rc == 0 && !content.trim().is_empty() {
                        v.push((basename(rel), content));
                    }
                }
                return Ok((v, false));
            }
            // An agent that has never committed still has a memory on disk.
            for s in crate::commands::store_sessions(&a.store).into_iter().filter(|s| s.runtime == rt) {
                if let Ok(c) = std::fs::read_to_string(&s.path) {
                    if !c.trim().is_empty() {
                        v.push((basename(&s.path.to_string_lossy()), c));
                    }
                }
            }
            Ok((v, false))
        }
    }
}

/// A session of `rt` in either store layout (`sessions/<rt>/` or `sessions/<env>/<rt>/`), excluding a
/// session's sidecars (`<id>/subagents/*.jsonl`), whose parent is the id, not the runtime.
fn is_session_of(path: &str, rt: &str) -> bool {
    path.ends_with(".jsonl") && path.split('/').rev().nth(1) == Some(rt)
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// One open conflict: what it is, and the concrete ways out the dialogue named (if any).
struct Conflict {
    what: String,
    options: Vec<String>,
}

/// Parse the synthesis's `[OPEN]` lines into conflicts. `synthesize` asks for
/// `<conflict> || <option> | <option>`; a line without options is still a conflict, just one the user
/// answers in their own words (the no-LLM fallback path produces exactly those from `CONFLICT:` markers).
fn parse_conflicts(open: &str) -> Vec<Conflict> {
    open.lines()
        .map(|l| l.trim().trim_start_matches(['-', '*']).trim())
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none") && !l.eq_ignore_ascii_case("n/a"))
        .map(|l| match l.split_once("||") {
            Some((what, opts)) => Conflict {
                what: what.trim().to_string(),
                options: opts.split('|').map(|o| o.trim().to_string()).filter(|o| !o.is_empty()).collect(),
            },
            None => Conflict { what: l.to_string(), options: Vec::new() },
        })
        .collect()
}

/// One recorded conflict decision: a listed way out the user accepted, a call they typed in their own
/// words, or a deliberate defer (a rejection of every option, for now).
enum Decision {
    Accepted(String),
    Custom(String),
    Deferred,
}

impl Decision {
    /// The audit-trail verdict line for the ledger.
    fn verdict(&self) -> String {
        match self {
            Decision::Accepted(o) => format!("accepted → {o}"),
            Decision::Custom(d) => format!("custom → {d}"),
            Decision::Deferred => "rejected (left open)".to_string(),
        }
    }
    /// The instruction line baked into A's resumed session, or `None` for a deferral (nothing to settle).
    fn settled(&self, what: &str) -> Option<String> {
        match self {
            Decision::Accepted(o) => Some(format!("- {what}\n  → decided: {o}")),
            Decision::Custom(d) => Some(format!("- {what}\n  → decided: {d}")),
            Decision::Deferred => None,
        }
    }
}

/// If there are open conflicts and we're at a terminal, walk the user through deciding each, PERSIST the
/// accept/reject/choose outcomes as an auditable ledger beside the transcript, and record the settled ones
/// into A's session (the merged state). Non-interactive: just surface them (list + exit 1).
fn resolve_inline(rt: &str, a: &Side, open: &str, agent: &Path, b_id: &str) -> Result<i32> {
    let items = parse_conflicts(open);
    if items.is_empty() {
        outln!("\nNothing left for you to decide.");
        return Ok(0);
    }
    // A merge in CI must never block on a prompt nobody can answer: print what needed deciding, with
    // every option, and leave non-zero for the pipeline to act on.
    if !crate::ui::interactive() {
        outln!("\nNeeds your decision (rerun at a terminal to answer, or decide by hand):");
        for (i, c) in items.iter().enumerate() {
            outln!("\n[{}/{}] {}", i + 1, items.len(), c.what);
            for (n, o) in c.options.iter().enumerate() {
                outln!("    {}) {o}", n + 1);
            }
        }
        return Ok(1);
    }
    let mut ledger: Vec<(&Conflict, Decision)> = Vec::new();
    for (i, c) in items.iter().enumerate() {
        let mut options = c.options.clone();
        options.push("leave open, decide later".to_string());
        let last = options.len() - 1;
        let choice = crate::ui::pick_boxed(
            &format!("conflict {}/{}", i + 1, items.len()),
            &c.what,
            &options,
            "your call",
        )?;
        let decision = match choice {
            // Anything typed instead of a number IS the decision: the options are a shortcut, and the
            // dialogue cannot have foreseen every way out.
            crate::ui::Choice::Typed(d) => Decision::Custom(d),
            crate::ui::Choice::Option(n) if n != last => Decision::Accepted(options[n].clone()),
            // The "leave open" option, an empty answer, or none picked: a conscious deferral, recorded.
            _ => Decision::Deferred,
        };
        ledger.push((c, decision));
    }
    // Persist the whole ledger — rejections and deferrals are decisions too, so every conflict is on the
    // record, not only the resolved ones. Deterministic record-keeping: no model, always written.
    let ledger_path = save_decisions(agent, rt, &a.id, b_id, &ledger)?;
    outln!("\nDecision ledger recorded → {}", ledger_path.display());

    let settled: Vec<String> = ledger.iter().filter_map(|(c, d)| d.settled(&c.what)).collect();
    if settled.is_empty() {
        outln!("\nAll left open:\n{open}");
        return Ok(1);
    }
    // Bake the human's settled decisions into A's session so `resume` continues with them decided.
    let joined = settled.join("\n");
    let _ = turn(
        rt,
        &a.cwd,
        &a.id,
        &format!(
            "The human resolved the open conflicts as follows. Record these as the agreed decisions going \
             forward (do not edit files):\n{joined}\nReply 'noted'."
        ),
    );
    outln!("\nRecorded {} decision(s) into the merged state.", settled.len());
    if settled.len() < items.len() {
        Ok(1) // some left open
    } else {
        Ok(0)
    }
}

/// Persist the accept/reject/choose decisions as an auditable ledger in `<agent>/sessions/sync/`, beside
/// the transcript `save_transcript` writes (same `<a8>-<b8>` stem, a `.decisions.md` suffix). Deterministic
/// record-keeping — no model. Records EVERY conflict, its options, and the verdict, including the ones left
/// open, so the ledger is a complete audit trail of what was decided and what was consciously deferred.
fn save_decisions(agent: &Path, rt: &str, a_id: &str, b_id: &str, ledger: &[(&Conflict, Decision)]) -> Result<PathBuf> {
    let dir = agent.join("sessions").join("sync");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{}.decisions.md", &a_id[..8], &b_id[..8]));
    let mut md = format!("# conflict decisions ({rt})\n\nA={a_id}\nB={b_id}\n\n");
    for (i, (c, decision)) in ledger.iter().enumerate() {
        md.push_str(&format!("## {}. {}\n", i + 1, c.what));
        if !c.options.is_empty() {
            md.push_str("options:\n");
            for o in &c.options {
                md.push_str(&format!("  - {o}\n"));
            }
        }
        md.push_str(&format!("decision: {}\n\n", decision.verdict()));
    }
    std::fs::write(&path, md)?;
    Ok(path)
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
    let mut msg = turn_aloud('A', rt, &a.cwd, &a.id, &open_prompt(rt, &a.diff, &a.brief, quick), quick)?;
    transcript.push(('A', msg.clone()));

    let mut first_b = true;
    for _ in 0..rounds {
        let bp = if first_b { relay_first(rt, &msg, &b.diff, &b.brief, quick) } else { relay(&msg) };
        let bmsg = turn_aloud('B', rt, &b.cwd, &b.id, &bp, !(first_b && !quick))?;
        transcript.push(('B', bmsg.clone()));
        // In the default mode B's first reply is its handoff summary, so A receives it framed as one.
        let ap = if first_b && !quick { relay_handoff("B", &bmsg) } else { relay(&bmsg) };
        first_b = false;
        let amsg = turn_aloud('A', rt, &a.cwd, &a.id, &ap, true)?;
        transcript.push(('A', amsg.clone()));
        msg = amsg;
        if is_done(&bmsg) && is_done(&msg) {
            break;
        }
    }
    Ok(transcript)
}

/// One turn, with the wait made visible and the reply printed as it lands.
///
/// NOT token-by-token, and nothing here pretends otherwise: `turn_claude` and `turn_codex` shell out
/// and collect the whole reply before returning (`.output()`, and codex's `-o <file>`), so there is no
/// token stream to relay without rebuilding both around a streaming vendor flag. What this fixes is the
/// part that read as a hang — a turn takes minutes and the silence was total. The spinner says WHO is
/// thinking and for how long; it lives on stderr, so `agit a merge … > log` still captures a clean
/// transcript, and it is cleared before the reply prints (and on drop, so an error lands on a clean
/// line).
fn turn_aloud(who: char, rt: &str, cwd: &Path, id: &str, prompt: &str, tools: bool) -> Result<String> {
    let spinner = crate::ui::Spinner::start(&format!("{who} ({rt})"));
    let reply = turn_with(rt, cwd, id, prompt, tools);
    let took = spinner.stop();
    let msg = reply?;
    outln!(
        "\n{} {msg}\n{}",
        crate::ui::accent(&format!("{who} →")),
        crate::ui::dim(&format!("  ({}s)", took.as_secs()))
    );
    Ok(msg)
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
    outln!("B's merged state; resume it on B's branch:");
    outln!("  (cd {} && claude --resume {b_merged})", env.display());
    Ok(())
}

/// `--splice`: combine two same-runtime sessions into one. A's transcript in full, then B's events
/// after the shared prefix (the point the two forked from), with B's session id and cwd normalized
/// onto A's so the id/cwd rewrite in `write_conversation` catches every line. Deterministic, no model.
fn splice_sessions(rt: &str, a_text: &str, b_text: &str, new_id: &str, cwd: &Path) -> Result<String> {
    let mut ir = convo::read_conversation(rt, a_text)?;
    let ir_b = convo::read_conversation(rt, b_text)?;
    // The shared prefix is the common ancestor both sessions forked from — keep it once, from A.
    let shared = ir
        .events
        .iter()
        .zip(ir_b.events.iter())
        .take_while(|(x, y)| x.raw == y.raw)
        .count();
    let (a_id, a_cwd) = (ir.session_id.clone(), ir.cwd.clone());
    for e in &ir_b.events[shared..] {
        let mut e = e.clone();
        e.raw = convo::swap_quoted(&e.raw, &ir_b.session_id, &a_id);
        if let (Some(b_cwd), Some(a_cwd)) = (&ir_b.cwd, &a_cwd) {
            e.raw = convo::swap_quoted(&e.raw, b_cwd, a_cwd);
        }
        ir.events.push(e);
    }
    let opts = ConvertOpts { cwd: Some(cwd.to_string_lossy().into_owned()), new_id: new_id.to_string() };
    convo::write_conversation(rt, &ir, &opts)
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

/// The tools withheld from a no-tools turn. `--disallowedTools` is the ONLY flag that actually withholds:
/// verified against claude by asking a fresh `-p` session to read a file with a known word in it —
///   no flag                → read it        (the default set applies; omitting a grant denies nothing)
///   --allowedTools ""      → read it        (the flag is an additive GRANT, not a restriction)
///   --tools ""             → withheld, but the model then emitted raw `<invoke name="Bash">` syntax AS
///                            ITS TEXT ANSWER — which this code would relay to the peer as a summary
///   --disallowedTools …    → clean refusal, and it says so in prose
const WITHHELD_TOOLS: &[&str] = &[
    "Read", "Grep", "Glob", "Bash", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch", "Task",
];

/// `tools = false` must WITHHOLD the tools, not merely ask the model not to use them: the vendors'
/// no-tools clamp is backed by a real constraint (compaction genuinely runs without tools), so their
/// "tool calls will be REJECTED" is true and the model's compliance is not being tested.
///
/// This previously withheld nothing — `tools = false` just skipped `--allowedTools`, which is an
/// additive grant, so the turn kept claude's DEFAULT tools while the prompt told it tool calls would be
/// rejected. That is the exact bluff the clamp exists to avoid, and it was asserted in this comment.
///
/// codex has no equivalent flag: `turn_codex` ignores `tools` and always runs `codex exec --sandbox
/// read-only`, so on codex the clamp is still only textual. That is an honest gap, not a claim.
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
    } else {
        cmd.args(["--disallowedTools"]).args(WITHHELD_TOOLS);
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
your branch's tree (Read/Grep/Glob); check the code instead of guessing; end your message with 'DONE' when \
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
- Tool calls will be REJECTED and will waste your only turn; you will fail the task.
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.

",
        tool_names(rt)
    )
}

/// Layer 3 of the clamp (claude's `jvu`, appended last).
const NO_TOOLS_REMINDER: &str = "

REMINDER: Do NOT call any tools. Respond with plain text only; an <analysis> block followed by a <summary> \
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
    ("Primary Request and Intent", "What you were asked to do on this branch, in detail; the goal, not just the diff."),
    ("Key Technical Concepts", "The technologies, patterns, and invariants this work relies on."),
    ("Files and Code Sections", "Files you examined, changed, or created. Include the code that matters and why each file matters."),
    ("Errors and fixes", "Errors you hit and how you fixed them, in one line each."),
    ("Problem Solving", "Problems solved, and any troubleshooting still in flight."),
    (
        "All user messages",
        "Every instruction your human gave you on this branch that is not a tool result; these carry \
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
const HANDOFF_DROP: &str = "Leave OUT; it costs the other agent context and misleads:
- Approaches you superseded, unless the other agent might plausibly retry one; then say it was rejected and why.
- Errors you already fixed, beyond the one-line record in section 4.
- Verbose tool output, file dumps, and command logs already reflected on disk; the diff below is ground truth for those.
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
         tree; so {peer}'s summary and the diffs you are given are your only evidence about {peer}'s side. \
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
         changed; the reasoning is the part {peer} cannot see.\n\n\
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
        format!("\n\nYour branch's diff since the common ancestor (ground truth; trust this over your memory):\n```diff\n{diff}\n```")
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
///
/// Takes no runtime: unlike `relay_first`, this turn is granted its tools (`turn`), so it carries no
/// no-tools preamble to make runtime-specific.
fn relay_handoff(peer: &str, other: &str) -> String {
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
    // The `||` shape is what turns the conflict prompt from a blank box into a decision: the dialogue
    // is the only thing that knows what the two ways out actually ARE (§11c's "keep uid, frontend
    // updates its caller"). It stays optional — a line without it is still a conflict the user answers
    // in their own words, which is also what the no-LLM path above produces.
    let prompt = format!(
        "Below is a reconciliation dialogue between two agents merging parallel branches. Summarize for a human \
         in two sections:\n\
         [RESOLVED] decisions they agreed on (one per line; write 'none' if empty).\n\
         [OPEN] conflicts still needing a human decision (one per line; write 'none' if empty).\n\
         Write each [OPEN] line as: <the conflict, naming both sides> || <one way out> | <the other way out>\n\
         Each way out must be a concrete action a human can pick, naming who changes what; e.g.\n\
         field name: user_id (frontend) vs uid (api) || keep uid, frontend updates its caller | keep user_id, api reverts the rename\n\
         Omit the `||` and the options only if the dialogue never named a way out.\n\
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

/// All local (not the peer's) sessions of this runtime, as (filename, content). `incoming` is the peer's
/// session filenames — A is defined as everything that is not B.
///
/// Reads the store the same way `peer_sessions` does (via `store_sessions`), so both sides see every
/// environment's sessions under either store layout. A side that only understood `sessions/<rt>/` would
/// silently find nothing once the store is env-partitioned, and merge would bail claiming this agent has
/// no session of its own.
fn local_sessions(agent: &Path, rt: &str, incoming: &[String]) -> Vec<(String, String)> {
    let incoming_names: std::collections::HashSet<&str> =
        incoming.iter().map(|p| p.rsplit('/').next().unwrap_or(p)).collect();
    crate::commands::store_sessions(agent)
        .into_iter()
        .filter(|s| s.runtime == rt)
        .filter_map(|s| {
            let name = basename(&s.path.to_string_lossy());
            if incoming_names.contains(name.as_str()) {
                return None;
            }
            std::fs::read_to_string(&s.path).ok().map(|c| (name, c))
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
    // Voice = the richest (most lines) — a proxy for the most complete context. Callers guard
    // against empty side lists, but stay defensive: no sessions → no voice, no brief (never panic).
    let voice = match on_branch.iter().max_by_key(|(_, c)| c.lines().count()) {
        Some(v) => v,
        None => return (String::new(), String::new()),
    };
    let others: Vec<&&(String, String)> = on_branch.iter().filter(|s| s.0 != voice.0).collect();

    let mut brief = String::new();
    if !others.is_empty() {
        brief.push_str("Your earlier sessions on this branch (context; you are resuming the latest one):\n");
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
    (rc == 0 && !sha.is_empty()).then_some(sha)
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
mod identity_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> (i32, String) {
        // Per-invocation identity: a global git config would clobber the developer's real one.
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn peer(name: &str, aid: &str) -> Target {
        Target::Peer(agent::Agent {
            aid: aid.into(),
            name: name.into(),
            store: PathBuf::from("/x"),
            remote: None,
        })
    }

    /// The clamp must WITHHOLD, not ask. `--allowedTools` is an additive grant, so the old `tools=false`
    /// (omit the flag) left claude's default tools in place while the prompt claimed they were rejected —
    /// verified by having a fresh `-p` session read a file with a known word in it, with and without.
    #[test]
    fn a_no_tools_turn_names_the_tools_it_withholds() {
        for t in ["Read", "Grep", "Glob", "Bash", "Edit", "Write"] {
            assert!(WITHHELD_TOOLS.contains(&t), "a no-tools turn must withhold {t}");
        }
        assert!(
            WITHHELD_TOOLS.contains(&"Task"),
            "Task too: denied the file tools directly, the model tried to reach them via a subagent"
        );
    }

    /// An empty side list must never panic (was `max_by_key(...).unwrap()`). Callers guard against it,
    /// but the guard is defense in depth: no sessions → empty voice, empty brief.
    #[test]
    fn pick_side_on_empty_sessions_does_not_panic() {
        let (voice, brief) = pick_side("claude-code", &[], None);
        assert!(voice.is_empty() && brief.is_empty());
        // A branch filter that matches nothing falls back to the (still empty) full list.
        let (voice, brief) = pick_side("claude-code", &[], Some("main"));
        assert!(voice.is_empty() && brief.is_empty());
    }

    /// Mode follows the AID, never git history — that is what removes the guess.
    #[test]
    fn same_aid_fuses_and_a_different_aid_never_does() {
        assert_eq!(merge_mode(Some("agt_01"), Some("agt_01")), Mode::Fuse);
        // a different agent stays a different agent, whichever way it is named
        assert_eq!(merge_mode(Some("agt_01"), Some("agt_02")), Mode::DialogueOnly);
    }

    /// The unrecoverable direction: a git merge of two unrelated memories cannot be undone, a skipped
    /// one costs a command. So an UNKNOWN aid must never match another unknown one — including via a
    /// ref, which is the case this test used to get wrong.
    #[test]
    fn an_unknown_aid_never_fuses_a_stranger() {
        assert_eq!(merge_mode(None, None), Mode::DialogueOnly);
        assert_eq!(merge_mode(Some("agt_01"), None), Mode::DialogueOnly);
        assert_eq!(merge_mode(None, Some("agt_02")), Mode::DialogueOnly);
        // The refuted case: this asserted Fuse, reasoning that "a ref in MY OWN store is my own
        // history by construction". Its own example disproves it — `origin/main` is a remote-tracking
        // ref, i.e. exactly what `agit a fetch <remote>` writes when a teammate's copy lands here, and
        // that copy may be a different agent entirely. Identity decides, or nothing fuses.
        assert_eq!(merge_mode(None, None), Mode::DialogueOnly);
    }

    /// The legacy scaffold wrote `id = "unnamed-agent"` into every store on earth. If that parsed as an
    /// identity, every pre-identity store would "be" the same agent and merge would fuse strangers.
    #[test]
    fn the_placeholder_id_is_not_an_identity() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        std::fs::write(d.join("agent.toml"), "[agent]\nid = \"unnamed-agent\"\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "legacy"]);

        assert_eq!(aid_of_store(d), None, "the placeholder must read as NO identity");
        assert_eq!(aid_at_ref(d, "HEAD"), None, "…at a ref too");
        assert_eq!(
            merge_mode(aid_of_store(d).as_deref(), aid_of_store(d).as_deref()),
            Mode::DialogueOnly,
            "two legacy stores must NOT fuse into one memory"
        );

        // a real aid, committed in the store, is readable at any ref without a checkout
        std::fs::write(d.join("agent.toml"), "[agent]\nid = \"agt_01J\"\nname = \"frontend\"\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "identity"]);
        assert_eq!(aid_at_ref(d, "HEAD").as_deref(), Some("agt_01J"));
        assert_eq!(aid_of_store(d).as_deref(), Some("agt_01J"));
    }

    /// Both store layouts, and never a session's sidecars (whose parent is the id, not the runtime).
    #[test]
    fn only_real_sessions_of_the_named_runtime_count_as_the_tail() {
        assert!(is_session_of("sessions/claude-code/a.jsonl", "claude-code"));
        assert!(is_session_of("sessions/web/claude-code/a.jsonl", "claude-code"));
        assert!(!is_session_of("sessions/codex/a.jsonl", "claude-code"), "the other runtime is not the tail");
        assert!(!is_session_of("sessions/claude-code/a/subagents/s.jsonl", "claude-code"), "a sidecar is not a session");
        assert!(!is_session_of("merges/a-b.md", "claude-code"));
        assert_eq!(basename("sessions/web/codex/x.jsonl"), "x.jsonl");
    }

    /// THE silent no-op. With no merge base `git diff HEAD...peer` exits 128 printing NOTHING on
    /// stdout, and the old code read that empty stdout as "no divergent tail" → "nothing to sync",
    /// exit 0, did nothing — exactly the cross-agent case. Asserted against real git so the trap stays
    /// proven, then asserted fixed.
    #[test]
    fn unrelated_histories_still_yield_the_peers_sessions() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        std::fs::create_dir_all(d.join("sessions/claude-code")).unwrap();
        std::fs::write(d.join("sessions/claude-code/mine.jsonl"), "{\"a\":1}\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "mine"]);

        // an unrelated history: no merge base, the shape a different agent's copy arrives in
        git(d, &["checkout", "-q", "--orphan", "peer"]);
        git(d, &["rm", "-rq", "--cached", "."]);
        std::fs::remove_file(d.join("sessions/claude-code/mine.jsonl")).unwrap();
        std::fs::write(d.join("sessions/claude-code/theirs.jsonl"), "{\"b\":2}\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "theirs"]);
        git(d, &["checkout", "-q", "main"]);

        // the trap, demonstrated in-repo
        assert_eq!(git(d, &["merge-base", "main", "peer"]).0, 1, "no merge base");
        let (rc, out) = scope::git_in_status(d, &["diff", "--name-only", "HEAD...peer", "--", "sessions"]);
        assert_eq!(rc, 128, "git changed: three-dot with no merge base used to be fatal");
        assert!(out.is_empty(), "…and it prints NOTHING on stdout, which is what read as 'no tail'");

        // the fix: the peer's session is found anyway
        let (tail, grounded) = peer_sessions(d, &Target::Ref("peer".into()), "claude-code").unwrap();
        assert_eq!(tail.len(), 1, "expected exactly the peer's session, got {tail:?}");
        assert_eq!(tail[0].0, "theirs.jsonl");
        assert_eq!(tail[0].1.trim(), "{\"b\":2}");
        // …and NOT my own file, which two-dot lists as a deletion and `git show peer:` cannot read
        assert!(!tail.iter().any(|(n, _)| n == "mine.jsonl"), "a path the peer deleted is not its session");
        // an answer with no shared ancestor is never "grounded": empty here would be a failed read,
        // and the caller must refuse to report it as agreement.
        assert!(!grounded);
    }

    /// The other half of the same bug: an empty tail is only trustworthy when it is grounded in a shared
    /// ancestor. Up-to-date must stay a friendly no-op, or `merge` cries wolf every time you are current.
    #[test]
    fn an_empty_tail_is_only_trusted_when_a_common_ancestor_grounds_it() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        std::fs::create_dir_all(d.join("sessions/claude-code")).unwrap();
        std::fs::write(d.join("sessions/claude-code/base.jsonl"), "{\"base\":1}\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "base"]);
        git(d, &["branch", "uptodate"]);

        let (tail, grounded) = peer_sessions(d, &Target::Ref("uptodate".into()), "claude-code").unwrap();
        assert!(tail.is_empty(), "nothing diverged");
        assert!(grounded, "a shared ancestor makes the empty answer the truth, not a failed read");
    }

    /// The ordinary case must keep working: a real merge base ⇒ only what the peer ADDED since it.
    #[test]
    fn a_related_peer_contributes_only_its_divergent_tail() {
        let store = tempfile::tempdir().unwrap();
        let d = store.path();
        git(d, &["init", "-q", "-b", "main", "."]);
        std::fs::create_dir_all(d.join("sessions/claude-code")).unwrap();
        std::fs::write(d.join("sessions/claude-code/base.jsonl"), "{\"base\":1}\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "base"]);

        git(d, &["checkout", "-qb", "peer"]);
        std::fs::write(d.join("sessions/claude-code/theirs.jsonl"), "{\"b\":2}\n").unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-qm", "theirs"]);
        git(d, &["checkout", "-q", "main"]);

        let (tail, grounded) = peer_sessions(d, &Target::Ref("peer".into()), "claude-code").unwrap();
        assert!(grounded, "a shared ancestor grounds the answer");
        assert_eq!(tail.len(), 1, "only the divergent tail, not the shared base: {tail:?}");
        assert_eq!(tail[0].0, "theirs.jsonl");
        // the shared base is common context, not something to reconcile against
        assert!(!tail.iter().any(|(n, _)| n == "base.jsonl"));
    }
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
        let p = relay_handoff("B", "B's summary");
        // codex's framing, minus the promise that is false here: each agent sees only its own worktree
        assert!(p.contains("build on the work that has already been done and avoid duplicating work"), "{p}");
        assert!(p.contains("do NOT have access to the state of the tools B used"), "{p}");
        assert!(p.contains("CONFLICT:") && p.contains("DONE"), "reconcile turns keep the loop's conventions");
    }

    /// The conflict picker is only as good as this parse: `||` splits the conflict from the ways out,
    /// and a line without one is still a conflict — just one the user answers in their own words.
    #[test]
    fn a_conflict_carries_its_ways_out_when_the_dialogue_named_them() {
        let open = "- field name: user_id (frontend) vs uid (api) || keep uid, frontend updates its caller | keep user_id, api reverts the rename\n\
                    - who owns the retry budget";
        let c = parse_conflicts(open);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].what, "field name: user_id (frontend) vs uid (api)");
        assert_eq!(c[0].options, ["keep uid, frontend updates its caller", "keep user_id, api reverts the rename"]);
        // no `||`: still a real conflict, with no options to shortcut it
        assert_eq!(c[1].what, "who owns the retry budget");
        assert!(c[1].options.is_empty());
    }

    /// "none" is the synthesis saying there is nothing to decide — reading it as a conflict named
    /// "none" would invent work and, worse, exit non-zero on a clean merge.
    #[test]
    fn an_empty_open_section_is_not_a_conflict() {
        for empty in ["", "none", "None", "- none\n", "n/a", "  \n  "] {
            assert!(parse_conflicts(empty).is_empty(), "{empty:?} is not a conflict");
        }
        // the no-LLM fallback path's shape (built from CONFLICT: markers) parses as plain conflicts
        let c = parse_conflicts("- we both renamed sync -> merge");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].what, "we both renamed sync -> merge");
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

    /// The `--dry-run` preview must name the target, the mode (fuse vs dialogue-only), each side's session
    /// count and its picked voice, and the branches — everything a user needs to decide whether to run the
    /// real merge — while promising no model spend. Pure text, no store or model needed.
    #[test]
    fn dry_run_report_names_the_sides_the_voices_and_the_mode() {
        let a_all = vec![
            ("a1.jsonl".to_string(), "one\ntwo\nthree".to_string()),
            ("a2.jsonl".to_string(), "x".to_string()),
        ];
        let b_all = vec![("b1.jsonl".to_string(), "solo".to_string())];
        let a_voice = a_all[0].1.clone(); // the richest local session
        let b_voice = b_all[0].1.clone();

        let a_side = SidePreview { branch: Some("feature-a"), sessions: &a_all, voice: &a_voice };
        let b_side = SidePreview { branch: Some("feature-b"), sessions: &b_all, voice: &b_voice };
        let out = dry_run_report("peer", Mode::Fuse, &a_side, &b_side);
        assert!(out.contains("dry run"), "banners the preview: {out}");
        assert!(out.to_lowercase().contains("no model"), "promises no model spend: {out}");
        assert!(out.contains("Target: peer"), "names the target: {out}");
        assert!(out.contains("fuse the git histories"), "Fuse surfaces the history merge: {out}");
        assert!(out.contains("feature-a") && out.contains("feature-b"), "both branches: {out}");
        assert!(out.contains("A @ feature-a: 2 session(s), voice = a1.jsonl"), "A count + voice: {out}");
        assert!(out.contains("B @ feature-b: 1 session(s), voice = b1.jsonl"), "B count + voice: {out}");

        // A different agent reconciles by dialogue only — the histories are NOT fused. Unrecorded branches.
        let a_none = SidePreview { branch: None, sessions: &a_all, voice: &a_voice };
        let b_none = SidePreview { branch: None, sessions: &b_all, voice: &b_voice };
        let d = dry_run_report("frontend", Mode::DialogueOnly, &a_none, &b_none);
        assert!(d.contains("stay separate") && !d.contains("fuse the git histories"), "dialogue-only: {d}");
        assert!(d.contains("A @ ?:") && d.contains("B @ ?:"), "an unrecorded branch shows as ?: {d}");
    }

    /// The audit ledger records EVERY conflict and its verdict — accepted option, custom call, and a
    /// deferral alike — beside the transcript, so decisions no longer live only inside the resumed session.
    #[test]
    fn the_conflict_ledger_records_every_decision() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let c1 = Conflict {
            what: "field name: user_id (frontend) vs uid (api)".into(),
            options: vec!["keep uid".into(), "keep user_id".into()],
        };
        let c2 = Conflict { what: "who owns the retry budget".into(), options: vec![] };
        let c3 = Conflict { what: "log format".into(), options: vec!["json".into()] };
        let ledger: Vec<(&Conflict, Decision)> = vec![
            (&c1, Decision::Accepted("keep uid".into())),
            (&c2, Decision::Custom("split it 50/50".into())),
            (&c3, Decision::Deferred),
        ];

        let path = save_decisions(store, "claude-code", "sync-a-123456789", "sync-b-987654321", &ledger).unwrap();
        // Written beside the transcript (same dir, distinguishable suffix), not inside a resumed session.
        assert!(path.starts_with(store.join("sessions").join("sync")), "ledger lives beside the transcript: {}", path.display());
        assert!(path.to_string_lossy().ends_with(".decisions.md"), "distinguishable from the .md transcript: {}", path.display());

        let md = std::fs::read_to_string(&path).unwrap();
        assert!(md.contains("accepted → keep uid"), "a chosen way out is recorded: {md}");
        assert!(md.contains("custom → split it 50/50"), "a typed decision is recorded verbatim: {md}");
        assert!(md.contains("rejected (left open)"), "a deferral is a decision too, and recorded: {md}");
        // every conflict is on the record, with the options that were presented
        assert!(md.contains("field name: user_id (frontend) vs uid (api)"), "conflict 1: {md}");
        assert!(md.contains("who owns the retry budget"), "conflict 2 (no options): {md}");
        assert!(md.contains("log format"), "conflict 3: {md}");
        assert!(md.contains("keep user_id"), "the options presented are part of the audit trail: {md}");
    }

    #[test]
    fn splice_combines_both_sides_under_one_new_id() {
        // Two independent sessions (different sessionIds, so no shared raw prefix):
        // splice concatenates A's transcript with B's, normalizes both ids onto the
        // fresh new_id, and yields one resumable session carrying both sides' work.
        let a = concat!(
            r#"{"type":"user","sessionId":"AAA","uuid":"u1","cwd":"/proj","message":{"role":"user","content":"shared"}}"#, "\n",
            r#"{"type":"assistant","sessionId":"AAA","uuid":"u2","parentUuid":"u1","cwd":"/proj","message":{"role":"assistant","content":[{"type":"text","text":"A did X"}]}}"#, "\n",
        );
        let b = concat!(
            r#"{"type":"user","sessionId":"BBB","uuid":"u1","cwd":"/proj","message":{"role":"user","content":"shared"}}"#, "\n",
            r#"{"type":"assistant","sessionId":"BBB","uuid":"u3","parentUuid":"u1","cwd":"/proj","message":{"role":"assistant","content":[{"type":"text","text":"B did Y"}]}}"#, "\n",
        );
        let out = splice_sessions("claude-code", a, b, "NEWID", std::path::Path::new("/proj")).unwrap();
        let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "both sides' events land in the combined session: {out}");
        assert!(out.contains("A did X") && out.contains("B did Y"), "both sides' work is present: {out}");
        assert!(out.contains("NEWID"), "combined session carries the fresh id: {out}");
        assert!(!out.contains("AAA") && !out.contains("BBB"), "both source ids are normalized away: {out}");
        // each line stays valid JSON — the result is a resumable transcript, not a text blob
        for l in &lines {
            serde_json::from_str::<serde_json::Value>(l).unwrap_or_else(|_| panic!("line is valid json: {l}"));
        }
    }
}
