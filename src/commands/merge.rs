//! `agit merge` — a merge carried out by a merge agent; no text merging.
//!
//! Four steps, from the PRD's "merge" section, following the storage layout underneath:
//!
//! 1. **Start**: `--into` defaults to `@`; outside a session context and with no `--into` this is
//!    a usage error (reconciliation is asymmetric — the direction must be explicit). The
//!    preflight settles the target branch's unsettled turns, finds the fork point (merge-base
//!    within one repo, the turn hash chain as the cross-repo fallback), and locks the target
//!    branch.
//! 2. **resume merge agent**: resume a merge session from the target head **and send the
//!    instruction in as its opening message** (only the directions and B's branch ref; no
//!    transcript is quoted) — without it the agent comes up not knowing it is the merge agent
//!    and only waits. `--manual` puts a human at the wheel on the same plumbing; this command
//!    must not go down when no model is available.
//! 3. **Work**: `agit view B --json` to scout, `agit show B#n.k` to drill in,
//!    `agit merge pick/drop/summary` to select and write the summary, and direct edits to the
//!    shared files.
//! 4. **Land**: once `--continue` validates, a two-parent merge commit is created (expected-head
//!    CAS): all of B's objects fold into A's log; the VIEW gains `__merge_start__`, the selected
//!    B-side events (original envelopes; the origin marking is the `_session_id`),
//!    `merge_summary`, `__merge_end__`; the merge agent's own exploration turns never enter the
//!    VIEW; the reconciliation of shared files in the worktree goes into the tree with them.
//!    Failing validation makes it a proposal only, and the target ref never moved.
//!
//! B is left untouched.

use super::CmdResult;
use crate::domain::mergetx::{self, Tx};
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::domain::transcript;
use crate::domain::transcript::Envelope;
use crate::{ExitCode, ui};
use anyhow::Context;
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Source ref (branch / tag / `owner/repo@ref`).
    pub source: Option<String>,
    /// Landing branch (default: `@`, the current session’s branch).
    #[arg(long, value_name = "owner/repo@branch")]
    pub into: Option<String>,
    /// Run the merge agent under another runtime.
    #[arg(long = "as", value_name = "runtime")]
    pub as_runtime: Option<String>,
    /// Extra human instruction (e.g. “only the conclusion, not the process”).
    #[arg(short = 'm', long, value_name = "instruction")]
    pub message: Option<String>,
    /// Human-driven without a model: no agent is started; prints the fork point, new turns on both sides, and plumbing instructions.
    #[arg(long)]
    pub manual: bool,
    /// Recon only: fork point + new turns both sides; no lock, no agent.
    #[arg(long)]
    pub dry_run: bool,

    /// Show the open merge transaction.
    #[arg(long, conflicts_with_all = ["continue_", "abort"])]
    pub status: bool,
    /// Validate and commit.
    #[arg(long)]
    pub continue_: bool,
    /// Abandon this merge (the target ref was never touched).
    #[arg(long)]
    pub abort: bool,

    /// Pick: `agit merge pick <ref>...`
    #[command(subcommand)]
    pub cmd: Option<PickCmd>,
}

#[derive(clap::Subcommand)]
pub enum PickCmd {
    /// Mark turns/events from the source as picked.
    Pick { refs: Vec<String> },
    /// Remove from the pick list.
    Drop { refs: Vec<String> },
    /// Write the merge summary (required to land).
    Summary {
        #[arg(short = 'm', long)]
        message: Option<String>,
        #[arg(short = 'F', long)]
        file: Option<std::path::PathBuf>,
    },
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    // The transaction subcommands run first (they need no source).
    if args.status {
        return status(&cwd, args.into.as_deref());
    }
    if args.continue_ {
        return continue_tx(&cwd, args.into.as_deref());
    }
    if args.abort {
        return abort(&cwd, args.into.as_deref());
    }
    if let Some(cmd) = args.cmd {
        return pick_drop_summary(&cwd, args.into.as_deref(), cmd);
    }

    let Some(source) = args.source.clone() else {
        ui::error("missing the source ref.");
        ui::hint("use `agit merge <src-ref>`, or `agit merge --status|--continue|--abort`");
        return Ok(ExitCode::Usage);
    };
    start(&cwd, &source, &args)
}

/// Resolve the merge's target repository and target branch.
fn target_of(
    cwd: &std::path::Path,
    into: Option<&str>,
) -> crate::Result<Option<(Repo, String, String, String)>> {
    if let Some(raw) = into
        && raw.contains('@')
        && raw != "@"
    {
        let parsed = match crate::commands::target::branch_only(raw) {
            Ok(v) => v,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(None);
            }
        };
        let slug = parsed
            .repo
            .ok_or_else(|| anyhow::anyhow!("merge target has no repository"))?;
        let branch = parsed
            .base
            .ok_or_else(|| anyhow::anyhow!("merge target has no branch"))?;
        let (o, n) = super::parse_slug(&slug)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{slug} doesn’t exist locally."));
            ui::hint(&format!("fetch it first: `agit clone {slug}`"));
            return Ok(None);
        };
        return Ok(Some((
            repo,
            slug,
            branch,
            "explicit owner/repo@branch".into(),
        )));
    }
    let ctx = match super::context::resolve(cwd) {
        Ok(c) => c,
        Err(e) => {
            if into.is_none() {
                ui::error(&format!("not inside a session context: {e:#}"));
                ui::hint(
                    "reconciliation is asymmetric — the direction must be explicit: `agit merge <source> --into <branch>`",
                );
                return Ok(None);
            }
            // --into gives the direction, but the repo still comes from the context
            ui::error(&format!("{e:#}"));
            return Ok(None);
        }
    };
    let branch = into.unwrap_or(&ctx.branch).to_string();
    let (o, n) = super::parse_slug(&ctx.repo)?;
    let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
        ui::error(&format!("{} doesn’t exist locally.", ctx.repo));
        return Ok(None);
    };
    Ok(Some((repo, ctx.repo, branch, ctx.via.to_string())))
}

fn start(cwd: &std::path::Path, src_ref: &str, args: &Args) -> CmdResult {
    let Some((repo, slug, target, via)) = target_of(cwd, args.into.as_deref())? else {
        return Ok(if args.into.is_none() {
            ExitCode::Usage
        } else {
            ExitCode::Ref
        });
    };
    println!(
        "{}",
        ui::dim(&format!("  target: {slug} @ {target} ({via})"))
    );

    if target == src_ref {
        ui::error("can’t merge a branch into itself.");
        return Ok(ExitCode::Usage);
    }
    // One repo runs one merge at a time. The message names **which** branch is locked — with
    // the transaction open on a different branch, "`{target}` is locked" says something false.
    if let Some(open) = mergetx::read(repo.root())? {
        ui::error(&format!(
            "this repo already has a merge open on `{}` ({} → {}).",
            open.target, open.source, open.target
        ));
        ui::hint("`agit merge --status` to inspect, `--abort` to drop");
        return Ok(ExitCode::Precondition);
    }

    // Resolve the source (it may live in another local repo).
    let Some(base) = super::fork::resolve_base(src_ref, cwd)? else {
        return Ok(ExitCode::Ref);
    };

    let target_head = repo.git(&["rev-parse", &format!("refs/heads/{target}")])?;
    let target_head = target_head.trim().to_string();

    // Fork point: merge-base within one repo; across repos (the source lives in another one)
    // the hash chain's common prefix is the fallback — a cross-repo common prefix means nothing
    // on the commit graph and is reported only.
    let same_repo = base.repo.root() == repo.root();
    let fork_point = if same_repo {
        repo.git_opt(&["merge-base", &target_head, &base.resolved.sha])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| target_head.clone())
    } else {
        target_head.clone() // cross-repo: the report starts from the target head
    };

    // Recon report: the turns each side added — counted off the turn table, not off commits (a
    // fork's identity commit and file commits take no turn ordinal).
    let new_on_target = turns_since(&repo, &target_head, &fork_point);
    let new_on_source = if same_repo {
        turns_since(&base.repo, &base.resolved.sha, &fork_point)
    } else {
        0
    };
    println!("fork point  {}", &fork_point[..9.min(fork_point.len())]);
    println!("this side  +{new_on_target} turns    source side  +{new_on_source} turns");

    if args.dry_run {
        println!(
            "{}",
            ui::dim("  --dry-run: no lock taken, no agent started")
        );
        return Ok(ExitCode::Ok);
    }

    // Preflight: settle the target branch's unsettled turns (settling is idempotent; with no
    // session link it silently skips).
    let exe = std::env::current_exe()?;
    let _ = std::process::Command::new(exe)
        .args(["commit", "--from-hook", &target])
        .current_dir(repo.root())
        .output();

    // The target branch's worktree comes first: the merge agent reconciles shared files there,
    // and `--continue` collects the edits from there. During the transaction its path comes from
    // `agit repo path <repo>@<target>`.
    super::worktree::checkout(&repo, &target)?;

    // Take the lock.
    mergetx::lock(
        repo.root(),
        &Tx {
            target: target.clone(),
            source: src_ref.to_string(),
            source_repo: Some(base.slug.clone()),
            source_branch: source_branch_for_tx(src_ref, &base)?,
            base: fork_point,
            target_head: target_head.clone(),
            source_head: base.resolved.sha.clone(),
            picked: vec![],
            summary: None,
        },
    )?;
    ui::success(&format!(
        "merge transaction open: {src_ref} → {target} (branch locked)"
    ));

    if args.manual {
        println!();
        print!(
            "{}",
            manual_commands(slug.as_str(), target.as_str(), src_ref)
        );
        return Ok(ExitCode::Ok);
    }

    // resume merge agent: materialized from the target head (its instance carries the
    // AGIT_MERGE_TX marker).
    let rargs = super::resume::Args {
        target: Some(target.clone()),
        as_runtime: args.as_runtime.clone(),
        cwd: None,
        no_launch: false,
        force: true, // the caller is usually the live session on the target branch itself
    };
    // The instruction goes in as the merge agent's **opening message**. Without it the agent
    // comes up with no idea that it is the merge agent — that is what leaves it sitting there
    // waiting after launch.
    let instruction = merge_instruction(&slug, &target, src_ref, args.message.as_deref());
    match super::resume::resume_branch_with_prompt(
        &repo,
        &slug,
        &target,
        &rargs,
        Some(&instruction),
    )? {
        Some(res) => {
            println!();
            println!(
                "merge-agent protocol (sent as its opening message; the skill is on the scene too):"
            );
            for l in instruction.lines() {
                println!("  {}", ui::dim(l));
            }
            match res.cmd {
                Some(cmd) => {
                    // AGIT_MERGE_TX goes to the child only: `Command::env` is enough, with no
                    // `export` spliced into the command string (that would require the string to
                    // have exactly the `(export ...` shape, which the codex resume command does
                    // not, and the string surgery then drops the marker with no symptom).
                    let status = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .env(mergetx::ENV, format!("{slug}@{target}"))
                        .status()?;
                    Ok(match status.code() {
                        Some(0) | None => ExitCode::Ok,
                        Some(_) => ExitCode::Precondition,
                    })
                }
                None => Ok(ExitCode::Ok),
            }
        }
        None => {
            // The agent cannot come up (runtime unavailable, and so on) — the transaction
            // stays open and the fallback to manual driving is stated.
            ui::warning(
                "the merge agent didn’t come up; the transaction stays open — drive it by hand:",
            );
            ui::hint("agit merge pick … → agit merge summary -m … → agit merge --continue");
            Ok(ExitCode::Precondition)
        }
    }
}

fn manual_commands(slug: &str, target: &str, src_ref: &str) -> String {
    let target_ref = format!("{slug}@{target}");
    format!(
        "manual driving (same plumbing):\n\
  agit view {src_ref} --json              what the source actually sees\n\
  agit show {src_ref}#n.k                drill into events outside the VIEW\n\
  agit merge --into {target_ref} pick {src_ref}#a..#b          select turns\n\
  agit merge --into {target_ref} summary -m \"…\"       write the merge summary\n\
  agit merge --into {target_ref} --continue            land it\n\
  agit merge --into {target_ref} --status              inspect it\n\
  agit merge --into {target_ref} --abort               drop it\n"
    )
}

/// The opening message sent to the merge agent.
///
/// Only the **directions** and B's branch ref — not one word of the transcript (a design red
/// line). Quoting the transcript forces the whole of B's context into A's window, while the merge
/// agent's first act is to scout with `agit view` itself: what it wants, and how far down it
/// drills, are its own call, on demand.
///
/// It lands through the harness's native "resume carrying a prompt" (see
/// [`super::resume::resume_branch_with_prompt`]), not by appending a forged user message to the
/// transcript — that transcript is evidence going into history.
fn merge_instruction(slug: &str, target: &str, src: &str, extra: Option<&str>) -> String {
    let mut s = format!(
        "You were resumed by `agit merge` as the merge agent (AGIT_MERGE_TX={slug}@{target}).\n\
         Reconcile branch `{src}` into the current branch `{target}`. Reconcile intent, don't stitch text.\n\
         \n\
         1. `agit view {src} --json` — what that session actually sees\n\
         2. `agit show {src}#n.k` — drill into events outside its VIEW\n\
         3. `agit merge pick {src}#a..#b` — select what belongs here (`drop` to unselect)\n\
         4. edit memory/ · skills/ · AGENTS.md under `$(agit repo path {slug}@{target})` — reconciling shared files is your job\n\
         5. `agit merge summary -m \"<what this reconciliation concluded>\"` — required to land\n\
         6. `agit merge --continue` — land it (`--abort` drops it; the target ref never moved)\n"
    );
    if let Some(m) = extra.map(str::trim).filter(|m| !m.is_empty()) {
        s.push_str(&format!("\nThe human who started this merge adds: {m}\n"));
    }
    s
}

// ─────────────────── Transaction state ──────────────────

/// Find the merge transaction in progress and its repository.
fn open_tx(cwd: &std::path::Path, into: Option<&str>) -> crate::Result<Option<(Repo, Tx)>> {
    let (repo, selected_branch) = if let Some(into) = into {
        let Some((repo, _slug, branch, _via)) = target_of(cwd, Some(into))? else {
            return Ok(None);
        };
        (repo, Some(branch))
    } else {
        let ctx = match super::context::resolve(cwd) {
            Ok(c) => c,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(None);
            }
        };
        let (o, n) = super::parse_slug(&ctx.repo)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{} doesn’t exist locally.", ctx.repo));
            return Ok(None);
        };
        (repo, Some(ctx.branch))
    };
    match mergetx::read(repo.root())? {
        Some(tx) => {
            if let Some(branch) = selected_branch
                && tx.target != branch
            {
                ui::error(&format!(
                    "this repo has a merge open on `{}`; the requested target is `{branch}`.",
                    tx.target
                ));
                ui::hint(&format!(
                    "use `agit merge --into <owner/repo>@{} --status|--abort`",
                    tx.target
                ));
                return Ok(None);
            }
            Ok(Some((repo, tx)))
        }
        None => {
            println!("no merge transaction is open.");
            Ok(None)
        }
    }
}

fn status(cwd: &std::path::Path, into: Option<&str>) -> CmdResult {
    let Some((_repo, tx)) = open_tx(cwd, into)? else {
        return Ok(ExitCode::Precondition);
    };
    println!("merge transaction  {} → {}", tx.source, tx.target);
    println!("  fork point    {}", &tx.base[..9.min(tx.base.len())]);
    println!("  picked        {} items", tx.picked_count());
    println!(
        "  summary   {}",
        if tx.has_summary() {
            "written"
        } else {
            "missing (--continue needs it)"
        }
    );
    Ok(ExitCode::Ok)
}

fn abort(cwd: &std::path::Path, into: Option<&str>) -> CmdResult {
    let Some((repo, tx)) = open_tx(cwd, into)? else {
        return Ok(ExitCode::Precondition);
    };
    mergetx::unlock(repo.root())?;
    ui::success(&format!(
        "dropped the {} → {} merge. The target ref never moved.",
        tx.source, tx.target
    ));
    Ok(ExitCode::Ok)
}

fn pick_drop_summary(cwd: &std::path::Path, into: Option<&str>, cmd: PickCmd) -> CmdResult {
    let Some((repo, mut tx)) = open_tx(cwd, into)? else {
        ui::error(
            "no merge transaction is open. Start one with `agit merge <source> --into <branch>`.",
        );
        return Ok(ExitCode::Precondition);
    };
    match cmd {
        PickCmd::Pick { refs: picks } => {
            tx.pick_more(&picks);
            mergetx::lock(repo.root(), &tx)?;
            ui::success(&format!("picked {} items", picks.len()));
        }
        PickCmd::Drop { refs: drops } => {
            let n = tx.drop(&drops);
            mergetx::lock(repo.root(), &tx)?;
            ui::success(&format!("removed {n} items"));
        }
        PickCmd::Summary { message, file } => {
            let text = match (message, file) {
                (Some(m), _) => m,
                (None, Some(f)) => std::fs::read_to_string(&f)?,
                (None, None) => {
                    ui::error("summary needs -m <text> or -F <file>.");
                    return Ok(ExitCode::Usage);
                }
            };
            tx.set_summary(text);
            mergetx::lock(repo.root(), &tx)?;
            ui::success("merge summary written");
        }
    }
    Ok(ExitCode::Ok)
}

fn continue_tx(cwd: &std::path::Path, into: Option<&str>) -> CmdResult {
    let Some((repo, tx)) = open_tx(cwd, into)? else {
        return Ok(ExitCode::Precondition);
    };

    // Validation: with no summary this is a proposal only.
    if !tx.has_summary() {
        ui::error(
            "the merge summary is missing. A merge without “what this reconciliation concluded” is an undocumented merge.",
        );
        ui::hint("agit merge summary -m \"…\"");
        return Ok(ExitCode::Precondition);
    }

    // Target-branch CAS: a head other than the one seen at start means somebody else moved it.
    let now_head = repo.git(&["rev-parse", &format!("refs/heads/{}", tx.target)])?;
    if now_head.trim() != tx.target_head {
        ui::error(&format!(
            "{} was pushed during the transaction (the lock was bypassed).",
            tx.target
        ));
        ui::hint(
            "see what actually happened, then `--abort` and redo — history never gets stitched quietly",
        );
        return Ok(ExitCode::Precondition);
    }

    // File-line special case: the merge reconciles files and puts the summary in the message,
    // with no VIEW change. The test is `meta.line` — "meta cannot be read" is not a file line,
    // and such a checkout errors below in its own words.
    let target_is_file_line = meta::is_file_line_at(&repo, &tx.target_head);
    let target_layout = super::plumbing::storage_layout_at(&repo, &tx.target_head)?;
    // Every successful merge writes a v1 snapshot. A v0 target may still use the new root names
    // as ordinary files, including ignored worktree data, so prove that namespace is free before
    // any object import or ref movement.
    super::plumbing::ensure_v1_upgrade_preflight(&repo, &tx.target_head)?;

    // Resolve the source (possibly cross-repo). A transaction recovers the source repo and
    // branch from the lock, not from cwd.
    let Some(base) = resolve_transaction_source(&tx, cwd)? else {
        ui::error("the source ref no longer resolves (branch deleted?)");
        ui::hint("`agit merge --abort` drops this transaction");
        return Ok(ExitCode::Precondition);
    };
    let src_head = &base.resolved.sha;
    if src_head != &tx.source_head {
        ui::error(&format!(
            "{} moved during the transaction (expected {}, now {}).",
            tx.source,
            &tx.source_head[..9.min(tx.source_head.len())],
            &src_head[..9.min(src_head.len())]
        ));
        ui::hint("`agit merge --abort` and start over against the source as it is now");
        return Ok(ExitCode::Precondition);
    }
    if target_is_file_line {
        // File-line merge-tree incorporates both input trees. A v0 source's root LOG/VIEW/events
        // are user files, not storage, and must never be removed as part of the v1 result cleanup.
        super::plumbing::ensure_v1_namespace_available_at(&base.repo, src_head)?;
    }

    // Cross-repo source commits live in a different object database.  Import their complete graph
    // before either `merge-tree` or `commit-tree -p` needs to resolve the source parent.  This only
    // installs Git objects: it creates no ref and leaves FETCH_HEAD, the index, and both worktrees
    // untouched.  Same-repo merges are a no-op here.
    super::plumbing::import_commit_graph(&repo, &base.repo, src_head)?;

    // The reconciliation of shared files lives in **the target branch's worktree**: reconciling
    // memory/ · skills/ · AGENTS.md is the merge agent's job, and it edits them directly under
    // `agit repo path <repo>@<target>`, with no add and no commit. Without collecting from there
    // at landing time, not one byte of those edits reaches the merge commit.
    //
    // Only the worktree that has the target branch checked out counts: in any other checkout the
    // "worktree ↔ target_head" difference is a difference between two branches, not the merge
    // agent's work, and taking it in credits it to the wrong party.
    let checkout = super::worktree::existing(&repo, &tx.target)?;
    let shared: Vec<String> = match &checkout {
        Some(checkout) => super::plumbing::worktree_changes(
            checkout,
            &tx.target_head,
            storage_exclusions(target_layout),
        )?,
        None => vec![],
    };
    let on_target = checkout.is_some();
    // Reading the worktree and refreshing the checkout below both target that worktree; with no
    // worktree only the ref moves.
    let repo = checkout.unwrap_or(repo);

    let (tree, msg) = match merge_tree(
        &repo,
        &base.repo,
        &tx,
        src_head,
        &shared,
        target_is_file_line,
    )? {
        Some(v) => v,
        None => return Ok(ExitCode::Precondition),
    };
    if !shared.is_empty() {
        println!(
            "{}",
            ui::dim(&format!(
                "  shared files reconciled into this merge: {}",
                shared.join(", ")
            ))
        );
    }

    let commit = super::plumbing::commit_tree(&repo, &tree, &[&tx.target_head, src_head], &msg)?;
    super::plumbing::update_branch_cas_and_refresh(
        &repo,
        &tx.target,
        &commit,
        &tx.target_head,
        on_target,
    )?;
    mergetx::unlock(repo.root())?;

    ui::success(&format!(
        "merge commit landed: {} (parents: {}, {})",
        &commit[..9.min(commit.len())],
        &tx.target_head[..9.min(tx.target_head.len())],
        &src_head[..9.min(src_head.len())]
    ));
    println!(
        "{}",
        ui::dim(
            "  after the next resume, the agent on the target sees itself as the main body, plus the marked merge block"
        )
    );
    Ok(ExitCode::Ok)
}

fn source_branch_for_tx(
    source: &str,
    base: &super::fork::ForkBase,
) -> crate::Result<Option<String>> {
    let spec = super::context::substitute_at(refs::parse(source)?)?;
    Ok(match spec.base {
        refs::Base::Name(branch) => Some(branch),
        // `substitute_at` has already replaced `@` with the branch name.
        refs::Base::At => unreachable!("`@` is substituted before it reaches here"),
        refs::Base::Default => base.resolved.branch.clone(),
    })
}

fn transaction_source_spec(tx: &Tx) -> crate::Result<refs::RefSpec> {
    let slug = tx
        .source_repo
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("merge transaction has no source repository identity"))?;
    let (owner, name) = super::parse_slug(slug)?;
    let mut spec = refs::parse(&tx.source)?;
    spec.repo = refs::RepoSel::Slug(owner, name);
    if matches!(spec.base, refs::Base::At | refs::Base::Default) {
        let branch = tx
            .source_branch
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("merge transaction has no source branch identity"))?;
        spec.base = refs::Base::Name(branch.to_string());
    }
    Ok(spec)
}

fn resolve_transaction_source(
    tx: &Tx,
    cwd: &std::path::Path,
) -> crate::Result<Option<super::fork::ForkBase>> {
    let Some(slug) = tx.source_repo.as_deref() else {
        // Locks written before source identity was persisted retain their old
        // behavior; new locks never take this path.
        return super::fork::resolve_base(&tx.source, cwd);
    };
    let spec = transaction_source_spec(tx)?;
    let (owner, name) = super::parse_slug(slug)?;
    let dir = crate::infra::config::repo_dir(&owner, &name)?;
    let Some(repo) = Repo::open(&dir) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        ui::hint(&format!("fetch it first: `agit fetch {slug}`"));
        return Ok(None);
    };
    let resolved = match refs::resolve(&repo, &spec) {
        Ok(resolved) => resolved,
        Err(e) => {
            ui::error(&format!("failed to resolve `{}`: {e:#}", tx.source));
            return Ok(None);
        }
    };
    Ok(Some(super::fork::ForkBase {
        repo,
        slug: slug.to_string(),
        resolved,
    }))
}

fn storage_exclusions(layout: meta::LayoutVersion) -> &'static [&'static str] {
    match layout {
        meta::LayoutVersion::V0 => &[meta::FILE, meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE],
        meta::LayoutVersion::V1 => &[
            meta::FILE,
            meta::LOG_FILE,
            meta::VIEW_FILE,
            meta::LEGACY_LOG_FILE,
            meta::LEGACY_VIEW_FILE,
            meta::EVENTS_DIR,
        ],
    }
}

/// The merge commit's **tree and message** at landing time: what the merge result looks like is
/// decided entirely here.
///
/// Split off from the rest of [`continue_tx`] (CAS, unlocking, worktree alignment) because this
/// half is purely functional — given two heads, one selection, and a set of shared files, it
/// produces a determined tree. Tests feed [`Tx`] directly, with no forged session context.
///
/// `Ok(None)` = a refusal whose reason is already printed.
fn merge_tree(
    repo: &Repo,
    source_repo: &Repo,
    tx: &Tx,
    src_head: &str,
    shared: &[String],
    target_is_file_line: bool,
) -> crate::Result<Option<(String, String)>> {
    if !target_is_file_line {
        // Session line: merge the log and operate on the VIEW, over a base tree that already
        // carries the shared files.
        let base_tree = super::plumbing::tree_overlay_worktree(repo, &tx.target_head, shared)?;
        return merge_session_view(repo, source_repo, tx, src_head, &base_tree);
    }
    // File reconciliation: `merge-tree` does the mechanical three-way merge, and the session's
    // own storage files are stripped out.
    let tree = match repo.git(&["merge-tree", "--write-tree", &tx.target_head, src_head]) {
        Ok(t) => t.trim().to_string(),
        Err(e) => {
            ui::error(&format!(
                "the shared files have conflicts the merge agent didn’t resolve: {e:#}"
            ));
            ui::hint(
                "edit the shared files in the target’s worktree (`cd $(agit repo path <owner/repo>@<target>)`), then `--continue` again",
            );
            return Ok(None);
        }
    };
    // A file line carries no log/VIEW, but it **must** carry meta — the form is a declaration
    // written into the tree, and a reconciling merge must not erase it (erasing it puts
    // downstream back to guessing).
    let file_meta = {
        let mut m = meta::Meta::new_file_line();
        m.kind = meta::Kind::Merge;
        m.milestone = Some(format!("merge {}", tx.source));
        meta::to_text(&m)?
    };
    // The mechanical three-way merge is only a draft; the merge agent's hand reconciliation in
    // the worktree goes over it — on how a conflict is actually resolved, its conclusion beats
    // `merge-tree`'s guess.
    let tree = super::plumbing::tree_overlay_worktree(repo, &tree, shared)?;
    let existing_attributes = super::plumbing::regular_blob_text_at(repo, &tree, meta::ATTRS_FILE)?;
    let mut edits: std::collections::BTreeMap<String, Option<Vec<u8>>> = repo
        .ls_tree(&tree)
        .into_iter()
        .filter(|path| meta::is_storage_path(path))
        .map(|path| (path, None))
        .collect();
    edits.insert(meta::FILE.to_string(), Some(file_meta.into_bytes()));
    edits.insert(
        meta::ATTRS_FILE.to_string(),
        Some(storage::attributes_text_strict(existing_attributes.as_deref())?.into_bytes()),
    );
    let tree = super::plumbing::tree_apply_owned(repo, &tree, edits.into_iter().collect())?;
    let msg = format!(
        "agit: merge {} → {} (file reconciliation)\n\n{}",
        tx.source,
        tx.target,
        tx.summary_text()
    );
    Ok(Some((tree, msg)))
}

/// The session line's log/VIEW surgery: all plumbing, no worktree action.
///
/// * LOG (once materialized): all of A + the `__merge_start__` marker + all of B's
///   envelopes (objects folded in) + `merge_summary` + `__merge_end__`.
/// * VIEW: A's view + the marker + the **selected** B-side events (original envelopes, the
///   `_session_id` is the origin marking) + the summary + the end marker.
fn merge_session_view(
    target_repo: &Repo,
    source_repo: &Repo,
    tx: &Tx,
    src_head: &str,
    base_tree: &str,
) -> crate::Result<Option<(String, String)>> {
    let snap = meta::read_at_ref(target_repo, &tx.target_head)
        .ok_or_else(|| anyhow::anyhow!("the target head is missing {}", meta::FILE))?;
    let a_log = storage::materialize_at(target_repo.root(), &tx.target_head, meta::LOG_FILE)?;
    let a_view = storage::materialize_at(target_repo.root(), &tx.target_head, meta::VIEW_FILE)?;
    let b_log = match storage::materialize_at(source_repo.root(), src_head, meta::LOG_FILE) {
        Ok(log) => log,
        Err(error) => {
            // A source branch whose log cannot be read offers nothing to select from. Falling
            // back to an empty log makes "every pick fell through" and "the source was empty all
            // along" look identical, and the merge lands anyway.
            ui::error(&format!(
                "`{}` carries no readable {} at {} — there is nothing to merge from: {error:#}",
                tx.source,
                meta::LOG_FILE,
                &src_head[..9.min(src_head.len())]
            ));
            ui::hint("`agit merge --abort` drops this transaction");
            return Ok(None);
        }
    };

    // The selected events: resolve the picked refs into envelope lines of the source transcript.
    // A ref that does not resolve fails validation = a proposal; the target ref does not move,
    // and the transaction stays open for the selection to be fixed.
    let picked = match expand_picked(source_repo, tx) {
        Ok(p) => p,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ui::hint(
                "`agit merge drop <ref>` to fix the selection, or `--abort` to drop the whole merge",
            );
            return Ok(None);
        }
    };
    let b_lines: Vec<&str> = b_log.lines().collect();
    let mut picked_lines: Vec<&str> = Vec::with_capacity(picked.len());
    for gi in &picked {
        match b_lines.get(*gi) {
            Some(l) => picked_lines.push(l),
            // A selection pointing at a line the source log does not have = the refs recorded
            // in the transaction no longer match the source (the source was rewritten). Dropping
            // it silently is the shape where `--status` says "picked 1 items" while the VIEW
            // gains nothing.
            None => {
                ui::error(&format!(
                    "this merge picks event #{gi} of `{}`, but its log only has {} lines — the source was rewritten.",
                    tx.source,
                    b_lines.len()
                ));
                ui::hint("`agit merge --abort` and start over against the source as it is now");
                return Ok(None);
            }
        }
    }

    let claim = &snap.session;
    let runtime = &snap.runtime;

    let mark_start = marker_envelope("__merge_start__", runtime, claim, &tx.source);
    let mark_end = marker_envelope("__merge_end__", runtime, claim, &tx.source);
    let summary_line = summary_envelope(&tx.summary_text(), runtime, claim);

    let mut new_log = a_log.clone();
    if !new_log.ends_with('\n') && !new_log.is_empty() {
        new_log.push('\n');
    }
    new_log.push_str(&mark_start);
    new_log.push_str(&b_log);
    if !b_log.is_empty() && !b_log.ends_with('\n') {
        new_log.push('\n');
    }
    new_log.push_str(&summary_line);
    new_log.push_str(&mark_end);

    let mut new_view = a_view.clone();
    if !new_view.ends_with('\n') && !new_view.is_empty() {
        new_view.push('\n');
    }
    new_view.push_str(&mark_start);
    for l in &picked_lines {
        new_view.push_str(l);
        new_view.push('\n');
    }
    new_view.push_str(&summary_line);
    new_view.push_str(&mark_end);

    // meta: kind=Merge; turn and form are inherited from A's head (a merge does not change the
    // branch's form).
    let mut s = snap.clone();
    s.kind = meta::Kind::Merge;
    s.milestone = Some(format!("merge {}", tx.source));
    s.layout = meta::LayoutVersion::CURRENT;
    let snap_text = meta::to_text(&s)?;

    let existing_attributes =
        super::plumbing::regular_blob_text_at(target_repo, base_tree, meta::ATTRS_FILE)?;
    let mut edits: std::collections::BTreeMap<String, Option<Vec<u8>>> =
        storage::snapshot_files(&new_log, &new_view)?
            .into_iter()
            .map(|(path, bytes)| (path, Some(bytes)))
            .collect();
    // A migrated v1 tip should already have no v0 files. Keeping these explicit makes the
    // merge operation safe when invoked directly against an old local checkout.
    edits.insert(meta::LEGACY_LOG_FILE.to_string(), None);
    edits.insert(meta::LEGACY_VIEW_FILE.to_string(), None);
    edits.insert(meta::FILE.to_string(), Some(snap_text.into_bytes()));
    edits.insert(
        meta::ATTRS_FILE.to_string(),
        Some(storage::attributes_text_strict(existing_attributes.as_deref())?.into_bytes()),
    );
    // `commit-tree -p B` does not merge B's tree. Re-encoding all materialized B envelopes
    // here is what actually imports source-only event blobs, including cross-repo merges.
    let tree =
        super::plumbing::tree_apply_owned(target_repo, base_tree, edits.into_iter().collect())?;
    let msg = format!(
        "agit: merge {} → {}\n\n{}",
        tx.source,
        tx.target,
        tx.summary_text()
    );
    Ok(Some((tree, msg)))
}

/// Markers and summaries both land as envelope lines (the envelope discipline has no exception:
/// everything that enters the log is an Envelope).
pub fn marker_envelope(kind: &str, runtime: &str, claim: &str, source: &str) -> String {
    let content = serde_json::json!({
        "type": "system",
        "subtype": format!("agit:{kind}"),
        "source": source,
    });
    synthetic_envelope(content, runtime, claim)
}

pub fn summary_envelope(text: &str, runtime: &str, claim: &str) -> String {
    let content = serde_json::json!({
        "type": "user",
        "agit": "merge_summary",
        "message": {"role": "user", "content": text},
    });
    synthetic_envelope(content, runtime, claim)
}

fn synthetic_envelope(content: serde_json::Value, runtime: &str, claim: &str) -> String {
    let envelope = Envelope {
        source: runtime.to_string(),
        session_id: claim.to_string(),
        object_hash: transcript::object_hash(&content),
        content,
    };
    storage::envelope_line(&envelope)
}

/// The raw-JSON entry point for the cherry-pick / revert test fixtures; live serialization still
/// goes through [`storage::envelope_line`] alone, so no caller assembles its own Envelope bytes
/// that could drift.
pub fn envelope_line(raw: &str, runtime: &str, claim: &str) -> String {
    let content: serde_json::Value =
        serde_json::from_str(raw).expect("synthetic event content must be valid JSON");
    synthetic_envelope(content, runtime, claim)
}

/// Expand picked refs of the `B#3..#5` / `B#8.2` form into a list of **global line numbers in
/// the source transcript**.
///
/// Picked refs resolve against the source branch head (the `source_head` recorded in the
/// transaction): turn n = the lines that turn's commit adds to the transcript relative to its
/// parent (in log order; the log is append-only, so the numbering is stable).
fn expand_picked(source_repo: &Repo, tx: &Tx) -> crate::Result<Vec<usize>> {
    let mut out: Vec<usize> = vec![];
    for r in tx.picked_refs() {
        // The full `try-ratelimit#3..#5` is allowed, as is a bare `#3` (relative to the
        // transaction's source branch head).
        let spec = refs::parse(r)?;
        let tail = match &spec.tail {
            t @ (refs::Tail::Turn(_) | refs::Tail::Event { .. } | refs::Tail::Range { .. }) => {
                t.clone()
            }
            _ => {
                anyhow::bail!("`{r}` is not a turn-level ref (pick takes `#n` / `#n.k` / `#a..#b`)")
            }
        };
        let head = &tx.source_head;
        let before = out.len();
        match tail {
            refs::Tail::Turn(n) => out.extend(turn_lines(
                source_repo,
                head,
                refs::real_turn(source_repo, head, n)?,
            )?),
            refs::Tail::Range { a, b } => {
                for n in
                    refs::real_turn(source_repo, head, a)?..=refs::real_turn(source_repo, head, b)?
                {
                    out.extend(turn_lines(source_repo, head, n)?);
                }
            }
            refs::Tail::Event { turn, index } => {
                let n = refs::real_turn(source_repo, head, turn)?;
                let lines = turn_lines(source_repo, head, n)?;
                let k = (index as usize).saturating_sub(1);
                match lines.get(k) {
                    Some(l) => out.push(*l),
                    None => anyhow::bail!("`{r}`: turn {n} has only {} events", lines.len()),
                }
            }
            _ => unreachable!(),
        }
        // A ref that resolves to no event at all = the selection fell through. Left unchecked,
        // the merge lands with a VIEW holding only the marker block while `--status` keeps
        // reporting "picked N items".
        if out.len() == before {
            anyhow::bail!(
                "`{r}` selects no events — that turn added nothing to `{}`’s log. Check `agit view {} --json`",
                tx.source,
                tx.source
            );
        }
    }
    // Selections may overlap (`#3..#5` and then `#4.2` on its own), but one event appears in the
    // VIEW once, and in log order, not in the order the picks were typed.
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// How many turns this side has settled since the fork point.
///
/// Counts only the turn commits outside `fork_point`'s first-parent chain: an `agit merge` recon
/// report says "+N turns", and N must be turns that are visible in `agit log`.
fn turns_since(repo: &Repo, head: &str, fork_point: &str) -> u32 {
    let Ok(chain) = refs::Chain::read(repo, head) else {
        return 0;
    };
    let base: std::collections::HashSet<String> = refs::first_parent_chain(repo, fork_point)
        .unwrap_or_default()
        .into_iter()
        .collect();
    chain
        .turns()
        .iter()
        .filter(|(_, sha)| !base.contains(*sha))
        .count() as u32
}

/// The line numbers turn n contributes to the source transcript (the lines its commit adds
/// relative to its **first parent**).
///
/// The log is append-only, so "the lines this turn added" = the stretch between the two line
/// counts, and the numbering stays stable through the branch's later history.
pub fn turn_lines(repo: &Repo, head: &str, n: u32) -> crate::Result<Vec<usize>> {
    let (_, sha) = refs::turn_commit(repo, head, n)?;
    let this = storage::materialize_at(repo.root(), &sha, meta::LOG_FILE)
        .with_context(|| format!("turn {n} at {sha} has no valid LOG"))?;
    let parents = repo.git(&["rev-list", "--parents", "-n", "1", &sha])?;
    let mut fields = parents.split_whitespace();
    anyhow::ensure!(
        fields.next() == Some(sha.as_str()),
        "git returned the wrong turn commit"
    );
    let prev = match fields.next() {
        None => String::new(),
        Some(parent) => {
            let snapshot = meta::read_at_ref(repo, parent).with_context(|| {
                format!("turn {n}'s first parent has no readable {}", meta::FILE)
            })?;
            if snapshot.is_file_line() || snapshot.session.is_empty() {
                String::new()
            } else {
                storage::materialize_at(repo.root(), parent, meta::LOG_FILE).with_context(|| {
                    format!("turn {n}'s first parent at {parent} has no valid LOG")
                })?
            }
        }
    };
    anyhow::ensure!(
        this.starts_with(&prev),
        "turn {n} rewrites its first parent's LOG instead of appending"
    );
    if head != sha {
        let latest = storage::materialize_at(repo.root(), head, meta::LOG_FILE)
            .with_context(|| format!("source head {head} has no valid LOG"))?;
        anyhow::ensure!(
            latest.starts_with(&this),
            "source head {head} rewrites LOG after turn {n}; its event coordinates are no longer stable"
        );
    }
    Ok((prev.lines().count()..this.lines().count()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::Meta;

    #[test]
    fn markers_are_envelopes_too() {
        let m = super::marker_envelope("__merge_start__", "codex", &fake_claim('a'), "b#1");
        assert!(m.contains("_session_id"));
        assert!(m.ends_with('\n'));
        assert!(storage::parse_envelope_line(&m).is_ok());
    }

    // ───────────── The opening instruction ─────────────

    /// The instruction carries identity, direction and the whole protocol on its own — an agent
    /// that has just come up never has to guess who it is.
    #[test]
    fn the_opening_instruction_carries_identity_direction_and_the_whole_protocol() {
        let s = merge_instruction("me/repo", "main", "try-ratelimit", None);
        assert!(s.contains("AGIT_MERGE_TX=me/repo@main"), "{s}");
        assert!(s.contains("`try-ratelimit`") && s.contains("`main`"), "{s}");
        for verb in [
            "agit view try-ratelimit --json",
            "agit show try-ratelimit#n.k",
            "agit merge pick",
            "agit merge summary",
            "agit merge --continue",
        ] {
            assert!(s.contains(verb), "missing `{verb}`: {s}");
        }
        assert!(
            s.contains("memory/"),
            "shared-file reconciliation is its job and must be stated: {s}"
        );
    }

    #[test]
    fn manual_commands_keep_an_explicit_target_for_every_transaction_step() {
        let s = manual_commands("alice/payments", "main", "alice/payments@feature");
        for verb in [
            "agit merge --into alice/payments@main pick",
            "agit merge --into alice/payments@main summary",
            "agit merge --into alice/payments@main --continue",
            "agit merge --into alice/payments@main --status",
            "agit merge --into alice/payments@main --abort",
        ] {
            assert!(s.contains(verb), "missing `{verb}`: {s}");
        }
    }

    #[test]
    fn transaction_source_spec_rebinds_at_to_the_persisted_source_identity() {
        let tx = Tx {
            target: "main".into(),
            source: "@#1".into(),
            source_repo: Some("bob/notes".into()),
            source_branch: Some("main".into()),
            base: "base".into(),
            target_head: "target".into(),
            source_head: "source".into(),
            picked: vec![],
            summary: None,
        };
        let spec = transaction_source_spec(&tx).unwrap();
        assert_eq!(spec.repo, refs::RepoSel::Slug("bob".into(), "notes".into()));
        assert_eq!(spec.base, refs::Base::Name("main".into()));
        assert_eq!(spec.tail, refs::Tail::Turn(1));
    }

    /// Design red line: the instruction holds only directions and the branch ref, **quoting no
    /// transcript**.
    #[test]
    fn the_opening_instruction_quotes_no_transcript() {
        let s = merge_instruction(
            "me/repo",
            "main",
            "try-ratelimit",
            Some("only the conclusion"),
        );
        assert!(
            s.contains("The human who started this merge adds: only the conclusion"),
            "{s}"
        );
        // The instruction has a fixed size: its content is decided by its four parameters alone
        // and does not grow with the source branch.
        assert!(s.len() < 900, "instruction stays short: {} bytes", s.len());
    }

    #[test]
    fn an_empty_extra_instruction_adds_nothing() {
        let bare = merge_instruction("me/r", "main", "b", None);
        assert_eq!(merge_instruction("me/r", "main", "b", Some("   ")), bare);
    }

    // ───── Selections land in the VIEW ─────

    fn fake_claim(c: char) -> String {
        format!(
            "{}{}",
            crate::domain::meta::ID_PREFIX,
            c.to_string().repeat(40)
        )
    }

    fn env_line(text: &str, claim: &str) -> String {
        synthetic_envelope(
            serde_json::json!({"type": "user", "message": {"role": "user", "content": text}}),
            "codex",
            claim,
        )
    }

    /// Land one turn commit: the log appends `events`, the VIEW tracks the log, and meta records
    /// the turn ordinal.
    fn settle_turn(repo: &Repo, claim: &str, turn: u32, events: &[&str]) {
        let mut log = repo.show("HEAD", meta::LOG_FILE).unwrap_or_default();
        if !log.is_empty() && !log.ends_with('\n') {
            log.push('\n');
        }
        for e in events {
            log.push_str(&env_line(e, claim));
        }
        storage::write_snapshot(repo.root(), &log, &log).unwrap();
        let mut m = Meta::new(claim.to_string(), "codex".into(), "/w".into());
        m.turn = Some(turn);
        meta::write(repo.root(), &m).unwrap();
        repo.add_all().unwrap();
        repo.commit(&format!("agit: turn #{turn}")).unwrap();
    }

    /// The birth commit: it declares the form, **takes no turn ordinal**, and does not grow the
    /// log.
    ///
    /// Its existence is exactly why "count turn ordinals by commit position" comes out
    /// misaligned.
    fn birth(repo: &Repo) {
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "# shared\n").unwrap();
        meta::write(
            repo.root(),
            &Meta::new_session_line("codex".into(), "/w".into()),
        )
        .unwrap();
        storage::write_snapshot(repo.root(), "", "").unwrap();
        repo.add_all().unwrap();
        repo.commit("agit: claim session line").unwrap();
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repo,
        tx: Tx,
        src_head: String,
    }

    /// main (the target, one turn) and b (the source, two turns) fork from the same birth commit.
    fn two_branches() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("r")).unwrap();
        birth(&repo);
        let fork = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        settle_turn(&repo, &fake_claim('a'), 1, &["A's first turn"]);
        let target_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

        repo.git(&["checkout", "-q", "-b", "b", &fork]).unwrap();
        settle_turn(
            &repo,
            &fake_claim('b'),
            1,
            &["B's probe", "B's probe result"],
        );
        settle_turn(&repo, &fake_claim('b'), 2, &["B's conclusion"]);
        let src_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        repo.git(&["checkout", "-q", "main"]).unwrap();

        let tx = Tx {
            target: "main".into(),
            source: "b".into(),
            source_repo: None,
            source_branch: None,
            base: fork,
            target_head,
            source_head: src_head.clone(),
            picked: vec![],
            summary: Some("B's conclusion comes in; the probing stays on b".into()),
        };
        Fixture {
            _dir: dir,
            repo,
            tx,
            src_head,
        }
    }

    fn view_of(f: &Fixture, tree: &str) -> String {
        let c = crate::commands::plumbing::commit_tree(&f.repo, tree, &[&f.tx.target_head], "t")
            .unwrap();
        f.repo.show(&c, meta::VIEW_FILE).unwrap()
    }

    /// The turn ordinal comes from meta's `turn`, not from the commit's position.
    ///
    /// b's history is [birth, turn 1, turn 2]: counted by position, `b#2` points at turn 1. That
    /// misalignment is what makes `--status` say "picked 1 items" while the VIEW gains nothing.
    #[test]
    fn turn_numbers_follow_the_meta_not_the_commit_position() {
        let f = two_branches();
        assert_eq!(turn_lines(&f.repo, &f.src_head, 1).unwrap(), vec![0, 1]);
        assert_eq!(turn_lines(&f.repo, &f.src_head, 2).unwrap(), vec![2]);
        // `#-1` also resolves by the declared turn ordinal, not by the commit count (which is
        // three here).
        assert_eq!(
            refs::real_turn(&f.repo, &f.src_head, refs::LAST_TURN).unwrap(),
            2
        );
    }

    #[test]
    fn turn_coordinates_fail_when_a_later_commit_rewrites_log() {
        let f = two_branches();
        let replacement = env_line("rewritten history", &fake_claim('b'));
        let mut snapshot = meta::read_at_ref(&f.repo, &f.src_head).unwrap();
        snapshot.kind = meta::Kind::View;
        let snapshot_text = meta::to_text(&snapshot).unwrap();
        let tree = super::super::plumbing::session_snapshot_tree(
            &f.repo,
            &f.src_head,
            &replacement,
            &replacement,
            &snapshot_text,
        )
        .unwrap();
        let rewritten =
            super::super::plumbing::commit_tree(&f.repo, &tree, &[&f.src_head], "rewrite LOG")
                .unwrap();

        let error = turn_lines(&f.repo, &rewritten, 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("coordinates are no longer stable")
        );
    }

    #[test]
    fn turn_coordinates_fail_closed_on_invalid_intermediate_meta() {
        let f = two_branches();
        let invalid = b"{\"layout\":\"v1\",\"line\":\"session\",\"session\":\"not-an-agit-id\",\"runtime\":\"codex\",\"cwd\":\"/w\",\"kind\":\"turn\",\"turn\":3}\n";
        let tree = super::super::plumbing::tree_apply_owned(
            &f.repo,
            &f.src_head,
            vec![(meta::FILE.to_string(), Some(invalid.to_vec()))],
        )
        .unwrap();
        let corrupt =
            super::super::plumbing::commit_tree(&f.repo, &tree, &[&f.src_head], "corrupt meta")
                .unwrap();

        let error = turn_lines(&f.repo, &corrupt, 1).unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid session/meta.json"),
            "{error:#}"
        );
    }

    /// Picked events land in the VIEW for real, carrying their origin marking, between the two
    /// markers.
    #[test]
    fn picked_events_land_in_the_view_with_their_origin() {
        let mut f = two_branches();
        f.tx.picked = vec!["b#2".into()];
        let (tree, _) = merge_tree(&f.repo, &f.repo, &f.tx, &f.src_head.clone(), &[], false)
            .unwrap()
            .unwrap();
        let view = view_of(&f, &tree);

        assert!(
            view.contains("B's conclusion"),
            "a selected event lands in the VIEW: {view}"
        );
        assert!(
            !view.contains("B's probe"),
            "an unselected turn must not slip in: {view}"
        );
        assert!(
            view.contains("A's first turn"),
            "this branch's VIEW stays unchanged"
        );
        // Start marker, summary, end marker, plus the one selected event.
        assert_eq!(view.lines().count(), 5, "{view}");

        // Order: start marker → selection → summary → end marker.
        let idx = |needle: &str| view.lines().position(|l| l.contains(needle)).unwrap();
        assert!(idx("__merge_start__") < idx("B's conclusion"));
        assert!(idx("B's conclusion") < idx("merge_summary"));
        assert!(idx("merge_summary") < idx("__merge_end__"));

        // Origin marking: the envelope carries the source session id **unchanged**, and
        // `agit view` compares it against this branch's claim to print `merged-from:` — the
        // rendering side needs no change.
        let picked_line = view.lines().find(|l| l.contains("B's conclusion")).unwrap();
        let env: serde_json::Value = serde_json::from_str(picked_line).unwrap();
        assert_eq!(env["_session_id"], serde_json::json!(fake_claim('b')));
        assert_ne!(env["_session_id"], serde_json::json!(fake_claim('a')));
    }

    /// Multiple selections: a range plus a single pick, overlaps deduped, ordered by the log.
    #[test]
    fn picks_merge_dedupe_and_keep_log_order() {
        let mut f = two_branches();
        f.tx.picked = vec!["b#1..#2".into(), "b#1.1".into()];
        let (tree, _) = merge_tree(&f.repo, &f.repo, &f.tx, &f.src_head.clone(), &[], false)
            .unwrap()
            .unwrap();
        let view = view_of(&f, &tree);
        assert_eq!(
            view.matches("B's probe\"").count(),
            1,
            "an event selected twice enters once: {view}"
        );
        // This branch's line, the three marker lines, and all three of b's events.
        assert_eq!(view.lines().count(), 7, "{view}");
        let idx = |needle: &str| view.lines().position(|l| l.contains(needle)).unwrap();
        assert!(
            idx("B's probe result") < idx("B's conclusion"),
            "ordered by the log, not by the pick order"
        );
    }

    /// A selection that comes up empty is refused on the spot — silently landing a merge that
    /// holds only the marker lines is the worst outcome: the transaction closes while "the merge
    /// happened" is false.
    #[test]
    fn a_pick_that_selects_nothing_is_refused() {
        let mut f = two_branches();
        f.tx.picked = vec!["b#9".into()];
        let e = expand_picked(&f.repo, &f.tx).unwrap_err().to_string();
        assert!(e.contains("no turn 9"), "{e}");
        // Landing side: failing validation makes it a proposal only, producing neither tree nor
        // message.
        assert!(
            merge_tree(&f.repo, &f.repo, &f.tx, &f.src_head.clone(), &[], false)
                .unwrap()
                .is_none()
        );
    }

    /// A commit that inherits a turn ordinal (file / merge / view) must not impersonate that turn.
    ///
    /// A `-m` file commit and a merge commit both copy the head's `turn` unchanged (neither
    /// settles a new turn). Counting them into the turn table makes `b#2` point at the commit
    /// that copied ordinal 2 — and what enters the VIEW is that commit's whole addition relative
    /// to its parent, not turn 2.
    #[test]
    fn a_commit_that_only_inherits_a_turn_number_never_shadows_that_turn() {
        let mut f = two_branches();
        f.repo.git(&["checkout", "-q", "b"]).unwrap();
        std::fs::write(f.repo.root().join("AGENTS.md"), "# shared\nnote\n").unwrap();
        let mut m = meta::read(f.repo.root()).unwrap();
        m.kind = meta::Kind::File; // turn stays 2, inherited from the head
        assert_eq!(m.turn, Some(2));
        meta::write(f.repo.root(), &m).unwrap();
        f.repo.add_all().unwrap();
        f.repo.commit("agit: file commit").unwrap();
        let head = f
            .repo
            .git(&["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        f.repo.git(&["checkout", "-q", "main"]).unwrap();
        f.tx.source_head = head.clone();

        assert_eq!(
            turn_lines(&f.repo, &head, 2).unwrap(),
            vec![2],
            "still the real turn 2, not the file commit"
        );
        assert_eq!(refs::real_turn(&f.repo, &head, refs::LAST_TURN).unwrap(), 2);
    }

    /// Selecting no event is itself a conclusion ("nothing from B belongs here"); with the summary
    /// present it lands.
    #[test]
    fn a_merge_that_picks_nothing_still_lands_its_summary() {
        let f = two_branches();
        let (tree, msg) = merge_tree(&f.repo, &f.repo, &f.tx, &f.src_head.clone(), &[], false)
            .unwrap()
            .unwrap();
        assert!(msg.contains("B's conclusion comes in"));
        let view = view_of(&f, &tree);
        assert_eq!(
            view.lines().count(),
            4,
            "this branch's line plus the marker block: {view}"
        );
    }

    /// All of B's objects fold into A's log (the selection decides the VIEW, not the log).
    #[test]
    fn the_log_takes_all_of_b_even_when_the_view_takes_one() {
        let mut f = two_branches();
        f.tx.picked = vec!["b#2".into()];
        let (tree, _) = merge_tree(&f.repo, &f.repo, &f.tx, &f.src_head.clone(), &[], false)
            .unwrap()
            .unwrap();
        let c = crate::commands::plumbing::commit_tree(&f.repo, &tree, &[&f.tx.target_head], "t")
            .unwrap();
        let log = f.repo.show(&c, meta::LOG_FILE).unwrap();
        assert!(
            log.contains("B's probe"),
            "an unselected event still enters the log (all objects fold in): {log}"
        );
        assert!(
            log.contains("__merge_start__"),
            "the start marker must stay reachable in the log: {log}"
        );
        assert!(
            log.contains("__merge_end__"),
            "the end marker must stay reachable in the log: {log}"
        );
    }

    /// Two parents record ancestry only; the other parent's tree is not folded into the result
    /// automatically. Across repos the source event has to be written into the result tree
    /// explicitly, and the complete source parent DAG has to be imported into the target object
    /// database first.
    #[test]
    fn cross_repo_merge_copies_source_only_event_and_materializes_it() {
        let target_dir = tempfile::tempdir().unwrap();
        let target = Repo::init(&target_dir.path().join("target")).unwrap();
        birth(&target);
        settle_turn(&target, &fake_claim('a'), 1, &["target event"]);
        let target_head = target
            .git(&["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let source_dir = tempfile::tempdir().unwrap();
        let source = Repo::init(&source_dir.path().join("source")).unwrap();
        birth(&source);
        settle_turn(&source, &fake_claim('b'), 1, &["source-only event"]);
        let source_head = source
            .git(&["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let source_log =
            storage::materialize_at(source.root(), &source_head, meta::LOG_FILE).unwrap();
        let source_envelope = storage::parse_envelopes(&source_log)
            .unwrap()
            .pop()
            .unwrap();
        let source_line = storage::envelope_line(&source_envelope);
        let source_event_id = storage::event_id(&source_line).unwrap();
        let source_event_path = meta::event_path(&source_event_id).unwrap();
        assert!(
            target.show_raw(&target_head, &source_event_path).is_none(),
            "precondition: target must not already own the source event"
        );
        assert!(
            target
                .git_opt(&[
                    "rev-parse",
                    "--verify",
                    &format!("{source_head}^{{commit}}"),
                ])
                .is_none(),
            "precondition: the independent target must not own the source commit"
        );
        let refs_before = target
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .unwrap();
        let status_before = target.git(&["status", "--porcelain=v1"]).unwrap();

        let tx = Tx {
            target: "main".into(),
            source: "elsewhere/source@main".into(),
            source_repo: Some("elsewhere/source".into()),
            source_branch: Some("main".into()),
            base: target_head.clone(),
            target_head: target_head.clone(),
            source_head: source_head.clone(),
            picked: vec!["source#1".into()],
            summary: Some("take the source conclusion".into()),
        };
        let (tree, _) = merge_tree(&target, &source, &tx, &source_head, &[], false)
            .unwrap()
            .unwrap();
        crate::commands::plumbing::import_commit_graph(&target, &source, &source_head).unwrap();
        let commit = super::super::plumbing::commit_tree(
            &target,
            &tree,
            &[&target_head, &source_head],
            "test cross-repo merge tree",
        )
        .unwrap();

        assert_eq!(
            target
                .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
                .unwrap(),
            refs_before,
            "object import must not create even a private temporary ref"
        );
        assert_eq!(
            target.git(&["status", "--porcelain=v1"]).unwrap(),
            status_before,
            "object import and commit-tree must not touch index/worktree"
        );
        let parents = target
            .git(&["rev-list", "--parents", "-n", "1", &commit])
            .unwrap();
        assert_eq!(
            parents.split_whitespace().collect::<Vec<_>>(),
            vec![commit.as_str(), target_head.as_str(), source_head.as_str()]
        );

        assert_eq!(
            target.show_raw(&commit, &source_event_path).as_deref(),
            Some(source_line.as_str())
        );
        assert!(
            target.ls_tree(&commit).contains(&source_event_path),
            "git tree must explicitly contain the source-only event path"
        );
        let materialized =
            storage::materialize_at(target.root(), &commit, meta::VIEW_FILE).unwrap();
        assert!(materialized.contains("source-only event"), "{materialized}");
        let source_parent_log =
            storage::materialize_at(target.root(), &source_head, meta::LOG_FILE).unwrap();
        assert!(source_parent_log.contains("source-only event"));
        target
            .git(&["fsck", "--connectivity-only", "--no-dangling", &commit])
            .unwrap();
    }

    // ─────── Shared files into the tree ───────

    /// The merge agent edits the worktree's shared files directly — those bytes must land in the
    /// merge commit.
    #[test]
    fn shared_file_reconciliation_rides_along_into_the_merge_commit() {
        let mut f = two_branches();
        f.tx.picked = vec!["b#2".into()];
        // What the merge agent does: edit the shared files, with no add and no commit.
        std::fs::write(
            f.repo.root().join("AGENTS.md"),
            "# shared\nrate limits key on uid\n",
        )
        .unwrap();
        std::fs::create_dir_all(f.repo.root().join("memory")).unwrap();
        std::fs::write(f.repo.root().join("memory/team.md"), "uid, not user_id\n").unwrap();
        // `.gitattributes` is shared outside AgentGit's marked blocks. The merge agent may
        // reconcile user rules directly; final tree construction re-installs canonical blocks.
        std::fs::write(f.repo.root().join(meta::ATTRS_FILE), "*.bin binary\n").unwrap();

        let shared = crate::commands::plumbing::worktree_changes(
            &f.repo,
            &f.tx.target_head,
            storage_exclusions(meta::LayoutVersion::V1),
        )
        .unwrap();
        assert_eq!(
            shared,
            vec![meta::ATTRS_FILE, "AGENTS.md", "memory/team.md"]
        );

        let (tree, _) = merge_tree(&f.repo, &f.repo, &f.tx, &f.src_head.clone(), &shared, false)
            .unwrap()
            .unwrap();
        let c = crate::commands::plumbing::commit_tree(&f.repo, &tree, &[&f.tx.target_head], "t")
            .unwrap();
        assert!(f.repo.show(&c, "AGENTS.md").unwrap().contains("uid"));
        assert_eq!(
            f.repo.show(&c, "memory/team.md").unwrap().trim_end(),
            "uid, not user_id"
        );
        let attributes = f.repo.show_raw(&c, meta::ATTRS_FILE).unwrap();
        assert!(attributes.contains("*.bin binary"));
        assert_eq!(
            attributes,
            storage::attributes_text_strict(Some("*.bin binary\n")).unwrap()
        );
        // The session's LOG/VIEW/meta come from the merge surgery, not from the worktree's
        // stale bytes.
        assert!(
            f.repo
                .show(&c, meta::VIEW_FILE)
                .unwrap()
                .contains("__merge_start__")
        );
    }
}
