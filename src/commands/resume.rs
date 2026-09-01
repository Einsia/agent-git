//! `agit resume` — the strict entry point for continuing a session.
//!
//! PRD section "Forking and continuing": the lenient arbitration lives in `run`; resume does not
//! guess.
//!
//! # Two load paths
//!
//! * **Fast path — reuse the local native session**. The branch head was settled out of this
//!   machine's native session (the link's branch matches, there is no materialized baseline, the
//!   content is the committed prefix), and neither `--as` nor a different `--cwd` is given: call
//!   the harness's own resume command (`claude --resume <original uuid>` /
//!   `codex resume <original uuid>`), zero copy and zero loss.
//! * **Slow path — materialize and mint a new id**: the transcript comes from another machine,
//!   merge / view surgery changed the VIEW (committed is no longer the prefix of live), another
//!   instance ran ahead, a cross-harness move, a different directory — any one of these, and a
//!   new session is materialized into the runtime from the head VIEW (not the full log); the new
//!   instance's baseline byte count and hash are recorded into the store link on the spot,
//!   `AGIT_SESSION` is injected, and settlement afterwards reads only what was appended past the
//!   baseline.
//!
//! # Preconditions (any one failing refuses, with a copy-pasteable fix)
//!
//! * the target must be a session-line branch head (historic points and tags go through `fork`,
//!   the file line through `new`);
//! * unsealed (`agit branch seal` leaves only forking and viewing);
//! * an omitted branch is not guessed: a tty enters the picker (here first, same-repo after), a
//!   non-tty lists the candidates and exits 8.

use super::CmdResult;
use crate::domain::link::{self, Link};
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::infra::config;
use crate::{ExitCode, adapter, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs, Default)]
pub struct Args {
    /// Branch (or `@`). Tags / historic commits / #n are refused — those go through `agit fork`.
    #[arg(value_name = "branch | @")]
    pub target: Option<String>,

    /// Continue under another runtime (cross-harness goes through the IR; the loss list is shown up front).
    #[arg(long = "as", value_name = "runtime")]
    pub as_runtime: Option<String>,

    /// Restore in this directory.
    #[arg(long, value_name = "dir")]
    pub cwd: Option<PathBuf>,

    /// Prepare and print the command without launching.
    #[arg(long)]
    pub no_launch: bool,

    /// Proceed despite an active instance on the same branch (final arbitration is still content continuity / CAS at commit time).
    #[arg(long)]
    pub force: bool,
}

/// What one resume produces, for fork/run to compose with.
pub struct Resumed {
    /// The command sent to the runtime (with `AGIT_SESSION` injected).
    pub cmd: Option<String>,
    /// Whether the cross-runtime conversion was lossy.
    pub lossy: bool,
}

pub fn run(args: Args) -> CmdResult {
    let cwd_now = std::env::current_dir()?;

    // No argument + someone sitting at a terminal = hand over to the resident Sessions screen.
    // The verdict is taken once, and all four outcomes are handled.
    //
    // Matching only `Enter` lets the `NoTerminal` that `--tui` produces without a terminal fall
    // back silently to a plain resume — the script then believes the flag took effect. Putting
    // `Explain` outside this `if` recites "no interface for you" at an `agit resume <branch>`
    // that names a target too, where the interface was never a candidate.
    if args.target.is_none() && !args.no_launch {
        match crate::tui::should_enter() {
            // It **does not return a selection**: the Sessions screen stays alive for the whole
            // agent session (suspend the terminal → start the runtime → come back → refresh), so
            // the rest of this command never runs. `--no-launch` is the exception — that one
            // means "prepare only, do not launch".
            crate::tui::Verdict::Enter => return crate::tui::screens::sessions::run(&cwd_now),
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }

    // ── Resolve the target branch ──
    let (repo, slug, branch) = match resolve_branch(&args, &cwd_now)? {
        Resolved::Branch(repo, slug, branch) => (repo, slug, branch),
        Resolved::Refused(code) => return Ok(code),
    };

    match resume_branch(&repo, &slug, &branch, &args)? {
        Some(res) => finish(res, args.no_launch),
        None => Ok(ExitCode::Precondition),
    }
}

/// The result of [`resolve_branch`]: a resolved branch, or a refusal whose reason is already
/// printed and only the exit code is left.
enum Resolved {
    Branch(Repo, String, String),
    Refused(ExitCode),
}

/// Resolves (repo, slug, branch). Prints the reason and returns `Refused` on failure.
///
/// The exit code is decided here instead of uniformly as `Ref` at the call site: a reference that
/// does not resolve is 3, while "more than one candidate and no terminal to ask at" is 8 — a
/// script tells "you wrote it wrong" from "run it another way" by that code.
fn resolve_branch(args: &Args, cwd: &Path) -> crate::Result<Resolved> {
    // Explicit target: refs syntax (may be owner/repo@branch).
    if let Some(t) = &args.target {
        let spec = refs::parse(t)?;
        let (slug, base_name) = match &spec.repo {
            refs::RepoSel::Slug(o, n) => {
                let name = match &spec.base {
                    refs::Base::Name(b) => b.clone(),
                    refs::Base::At => {
                        ui::error("`@` takes no repo qualifier (it only ever means you).");
                        return Ok(Resolved::Refused(ExitCode::Ref));
                    }
                    refs::Base::Default => {
                        ui::error("owner/repo didn’t resolve to a branch.");
                        ui::hint(&format!("e.g. `agit resume {o}/{n}@<branch>`"));
                        return Ok(Resolved::Refused(ExitCode::Ref));
                    }
                };
                (format!("{o}/{n}"), name)
            }
            _ => {
                // With the branch name given explicitly only the repo has to resolve: the
                // directory binding is enough. Full `resolve` also demands that this directory
                // resolve a branch, so a freshly cloned directory (with no adopted session yet)
                // fails — and "clone, then resume a branch" is the main path of design W3.
                match &spec.base {
                    refs::Base::Name(b) => {
                        let repo = match super::context::repo_for(cwd) {
                            Ok(r) => r,
                            Err(e) => {
                                ui::error(&format!("{e:#}"));
                                return Ok(Resolved::Refused(ExitCode::Ref));
                            }
                        };
                        (super::context::qualify(&repo), b.clone())
                    }
                    // Both forms, `@` and an omitted branch, ask for "the current branch"
                    // to be resolved, and that is full resolution.
                    refs::Base::At | refs::Base::Default => {
                        let ctx = match super::context::resolve(cwd) {
                            Ok(c) => c,
                            Err(e) => {
                                ui::error(&format!("{e:#}"));
                                return Ok(Resolved::Refused(ExitCode::Ref));
                            }
                        };
                        if matches!(spec.base, refs::Base::Default) {
                            ui::error("no branch given.");
                            ui::hint(
                                "use `agit resume <branch>`, or `agit switch <branch>` then `agit resume`",
                            );
                            return Ok(Resolved::Refused(ExitCode::Ref));
                        }
                        (super::context::qualify(&ctx.repo), ctx.branch.clone())
                    }
                }
            }
        };
        if !matches!(spec.tail, refs::Tail::None) {
            ui::error(&format!(
                "`{t}` doesn’t point at a branch head. resume continues branch heads only."
            ));
            ui::hint(&format!(
                "fork off that point to keep going: `agit fork {t} -b <new> --resume`"
            ));
            return Ok(Resolved::Refused(ExitCode::Ref));
        }
        let (o, n) = super::parse_slug(&slug)?;
        let dir = config::repo_dir(&o, &n)?;
        let Some(repo) = Repo::open(&dir) else {
            ui::error(&format!("{slug} doesn’t exist locally."));
            ui::hint(&format!("fetch it first: `agit clone {slug}`"));
            return Ok(Resolved::Refused(ExitCode::Ref));
        };
        return Ok(Resolved::Branch(repo, slug, base_name));
    }

    // Omitted target: the picker (two signals).
    let cands = gather_candidates(cwd);
    let labels: Vec<String> = cands
        .iter()
        .map(|c| format!("{}  {} @ {}  {}", c.badge, c.slug, c.branch, c.detail))
        .collect();
    let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    match ui::prompt::select("which session to continue?", &refs)? {
        Some(i) => {
            let c = &cands[i];
            let (o, n) = super::parse_slug(&c.slug)?;
            let repo = Repo::open(config::repo_dir(&o, &n)?)
                .ok_or_else(|| anyhow::anyhow!("{} doesn’t exist locally", c.slug))?;
            Ok(Resolved::Branch(repo, c.slug.clone(), c.branch.clone()))
        }
        None if cands.is_empty() => {
            ui::error(
                "nothing to continue here: no adopted session in this directory and no branch anchored to this code repo.",
            );
            ui::hint(
                "be explicit: `agit resume <owner/repo>@<branch>`, or adopt one with `agit import`",
            );
            Ok(Resolved::Refused(ExitCode::Ref))
        }
        None => {
            ui::error("several candidate sessions — nothing interactive to pick with.");
            for c in &cands {
                eprintln!("  {}  {} @ {}", c.badge, c.slug, c.branch);
            }
            ui::hint("be explicit: `agit resume <branch>`");
            Ok(Resolved::Refused(super::context::NEED_INTERACTIVE))
        }
    }
}

struct Candidate {
    badge: &'static str, // "here" / "same-repo"
    slug: String,
    branch: String,
    detail: String,
}

/// Two signals: a link whose cwd matches this directory (here); a code anchor whose origin
/// matches this code repo (same-repo).
fn gather_candidates(cwd: &Path) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = vec![];
    let cwd_s = cwd.to_string_lossy().to_string();
    if let Ok(store) = Store::open_or_init() {
        for l in link::list(&store) {
            if l.cwd.as_deref() == Some(cwd_s.as_str())
                && let Some(branch) = &l.branch
                && let Some(slug) = super::context::slug_of_link(&l)
            {
                out.push(Candidate {
                    badge: "here",
                    slug,
                    branch: branch.clone(),
                    detail: format!("{} {}", l.source, link::short(&l.session_id)),
                });
            }
        }
    }
    // same-repo: when the current directory is a code repo, match `origin` against the code
    // anchor of every branch snapshot.
    if let Some(origin) = config::repo_origin()
        && let Ok(all) = super::clone::list_local()
    {
        for (owner, name, path) in all {
            let Some(repo) = Repo::open(&path) else {
                continue;
            };
            for b in repo.branches() {
                if let Some(snap) = meta::read_at_ref(&repo, &format!("refs/heads/{b}"))
                    && let Some(code) = &snap.code
                    && same_repo_as(code, &origin)
                {
                    let slug = format!("{owner}/{name}");
                    if out.iter().any(|c| c.slug == slug && c.branch == b) {
                        continue;
                    }
                    out.push(Candidate {
                        badge: "same-repo",
                        slug,
                        branch: b.clone(),
                        detail: snap.milestone.clone().unwrap_or_default(),
                    });
                }
            }
        }
    }
    out
}

/// Whether this branch's code anchor points at the `origin` code repo.
///
/// **This is the only implementation**: the TUI's Sessions screen lists the same candidates
/// (`tui::screens::sessions`), and two copies of "the same repo" will drift apart sooner or
/// later — the symptom of that drift is rows appearing in or vanishing from the list out of
/// nowhere, which nobody immediately connects to two criteria disagreeing.
pub(crate) fn same_repo_as(code: &str, origin: &str) -> bool {
    code.starts_with(&format!("{origin}@")) || code.starts_with(&normalize_origin(origin))
}

/// The ssh and the https spelling are the same repo (PRD, the interactive picker section).
fn normalize_origin(o: &str) -> String {
    o.replace("git@", "https://")
        .replace(':', "/")
        .replace("https://", "")
}

enum CwdResumeDecision {
    Continue,
    Inject(String),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CwdStateComparison {
    Equal,
    Different,
    Unknown,
}

fn compare_cwd_state(recorded: &meta::CwdState, current: &meta::CwdState) -> CwdStateComparison {
    if recorded.origin != current.origin
        || recorded.head != current.head
        || recorded.branch != current.branch
    {
        return CwdStateComparison::Different;
    }
    if recorded.worktree == meta::WorktreeStatus::Unknown
        || current.worktree == meta::WorktreeStatus::Unknown
    {
        return CwdStateComparison::Unknown;
    }
    if recorded == current {
        CwdStateComparison::Equal
    } else {
        CwdStateComparison::Different
    }
}

/// Compare the state recorded by the last settled turn with the selected resume cwd.
///
/// A missing snapshot is expected for sessions created before cwd-state persistence. A cwd
/// outside Git is also not an error: there is no trustworthy state to compare, so resume warns
/// and continues. A known, unequal pair and an uncomparable pair both need a user decision.
fn cwd_resume_decision(snapshot: &meta::Meta, cwd: &Path) -> crate::Result<CwdResumeDecision> {
    let Some(recorded) = snapshot.cwd_state.as_ref() else {
        return Ok(CwdResumeDecision::Continue);
    };
    let Some(current) = meta::cwd_state_of(cwd) else {
        ui::warning(&format!(
            "resume cwd `{}` is not a Git repository; the recorded cwd state cannot be compared, so continuing without an environment notice",
            cwd.display()
        ));
        ui::hint("use `agit resume --cwd <git-checkout>` to compare the recorded repository state");
        return Ok(CwdResumeDecision::Continue);
    };
    let comparison = compare_cwd_state(recorded, &current);
    if comparison == CwdStateComparison::Equal {
        return Ok(CwdResumeDecision::Continue);
    }

    match comparison {
        CwdStateComparison::Different => {
            ui::warning("the resume cwd differs from the state recorded at the last turn");
        }
        CwdStateComparison::Unknown => {
            ui::warning(
                "the resume cwd worktree state cannot be compared reliably; uncommitted changes may be missing",
            );
            ui::hint(
                "the repository identity matches, but an unknown state does not prove that the worktrees are equal",
            );
        }
        CwdStateComparison::Equal => unreachable!(),
    }
    println!("  recorded cwd:  {}", snapshot.cwd);
    println!("  recorded state: {}", display_cwd_state(recorded));
    println!("  current cwd:    {}", cwd.display());
    println!("  current state:  {}", display_cwd_state(&current));

    if std::env::var_os("AGIT_YES").is_some() {
        return Ok(CwdResumeDecision::Continue);
    }

    let options = [
        "continue anyway",
        "continue and inject an environment notice",
        "cancel",
    ];
    let prompt = match comparison {
        CwdStateComparison::Different => "the cwd state changed — choose how to resume",
        CwdStateComparison::Unknown => "the cwd state cannot be compared — choose how to resume",
        CwdStateComparison::Equal => unreachable!(),
    };
    match ui::prompt::select(prompt, &options)? {
        Some(0) => Ok(CwdResumeDecision::Continue),
        Some(1) => Ok(CwdResumeDecision::Inject(cwd_state_notice(
            snapshot, cwd, recorded, &current, comparison,
        ))),
        Some(_) | None => {
            println!("cancelled.");
            Ok(CwdResumeDecision::Cancel)
        }
    }
}

fn display_cwd_state(state: &meta::CwdState) -> String {
    serde_json::to_string(state).unwrap_or_else(|_| "{\"unserializable\":true}".into())
}

fn cwd_state_notice(
    snapshot: &meta::Meta,
    cwd: &Path,
    recorded: &meta::CwdState,
    current: &meta::CwdState,
    comparison: CwdStateComparison,
) -> String {
    let reason = match comparison {
        CwdStateComparison::Different => {
            "the working directory environment differs from the state recorded at the last settled turn"
        }
        CwdStateComparison::Unknown => {
            "the recorded or current worktree status is unknown, so the two working trees cannot be compared reliably"
        }
        CwdStateComparison::Equal => "the working directory environment was compared successfully",
    };
    format!(
        "AgentGit environment notice: {reason}.\nRecorded cwd: {}\nRecorded cwd state: {}\nCurrent cwd: {}\nCurrent cwd state: {}\nTreat the current cwd and its Git state as authoritative. Re-check files and Git status before acting; do not assume that uncommitted changes from the recorded state still exist.",
        snapshot.cwd,
        display_cwd_state(recorded),
        cwd.display(),
        display_cwd_state(current)
    )
}

/// Brings a branch up: preconditions → fast/slow path load → link registration.
///
/// The fork/run/new compositions all go through this function — "the resume load rules" exist
/// once. `Ok(None)` = a precondition refused (the reason is already printed).
pub fn resume_branch(
    repo: &Repo,
    slug: &str,
    branch: &str,
    args: &Args,
) -> crate::Result<Option<Resumed>> {
    resume_branch_with_prompt(repo, slug, branch, args, None)
}

/// A resume carrying an **opening prompt**: the session comes back and receives its first user
/// message at the same moment.
///
/// `agit merge` uses it to hand the merge instruction to the merge agent. Both harnesses support
/// this natively (`claude --resume <uuid> "<prompt>"` / `codex resume <uuid> "<prompt>"`), so the
/// instruction rides **argv** — never a forged user message written into the transcript: that
/// transcript is evidence that gets committed into history, and slipping agit's own words into it
/// fabricates evidence.
pub fn resume_branch_with_prompt(
    repo: &Repo,
    slug: &str,
    branch: &str,
    args: &Args,
    prompt: Option<&str>,
) -> crate::Result<Option<Resumed>> {
    // Preconditions: it exists, it is not the file line, it is unsealed, and it is a branch head
    // (`resolve_branch` already guarantees the last).
    if !repo.has_ref(&format!("refs/heads/{branch}")) {
        ui::error(&format!("{slug} has no branch `{branch}`."));
        return Ok(None);
    }
    if super::branch::is_sealed(repo, branch) {
        ui::error(&format!("`{branch}` is sealed — not resumable."));
        ui::hint(&format!(
            "fork off it instead: `agit fork {branch} -b <new> --resume`"
        ));
        return Ok(None);
    }
    let head = repo.git(&["rev-parse", &format!("refs/heads/{branch}")])?;
    let head = head.trim().to_string();
    // The form comes from `meta.line`, never a guess. A missing meta and "this is the file line"
    // are two different things and must not read the same: the first is a broken checkout, the
    // second a branch that never carries a session.
    let Some(snap) = meta::read_at_ref(repo, &head) else {
        ui::error(&format!(
            "`{branch}` carries no {} — this checkout is incomplete.",
            meta::FILE
        ));
        ui::hint("re-fetch it: `agit fetch` (or `agit clone` again)");
        return Ok(None);
    };
    if snap.is_file_line() {
        ui::error(&format!(
            "`{branch}` is the file line — it never carries a session, so there is nothing to resume."
        ));
        ui::hint(&format!(
            "start a fresh session off it: `agit new -b <name> --from {branch}`"
        ));
        return Ok(None);
    }

    // The fallback is the current directory — the same answer as launching the runtime directly.
    // Falling back to "the top level of the git repo containing the current directory" instead
    // installs a session resumed from ~/Code into ~ on a machine whose home directory is itself a
    // git repo.
    let cwd = args
        .cwd
        .clone()
        .or_else(|| {
            lk_for(repo, slug, branch)
                .and_then(|l| l.cwd.clone())
                .map(PathBuf::from)
        })
        .unwrap_or(std::env::current_dir()?);

    // `cwd_state` is an observation, not a checkout instruction. Compare it before either
    // native reuse or VIEW materialization so both resume paths make the same decision.
    let system_prompt = match cwd_resume_decision(&snap, &cwd)? {
        CwdResumeDecision::Continue => None,
        CwdResumeDecision::Inject(prompt) => Some(prompt),
        CwdResumeDecision::Cancel => return Ok(None),
    };

    // ── Memory: the branch's memory merges into the target runtime's memory dir (both paths) ──
    let from = snap.runtime.as_str();
    let to_runtime = args
        .as_runtime
        .as_deref()
        .and_then(|r| adapter::normalize(r).ok())
        .unwrap_or(from);
    match super::memory::materialize(repo, branch, slug, to_runtime, &cwd) {
        Ok(Some(report)) => super::memory::report_materialize(&report),
        Ok(None) => {}
        Err(error) => ui::warning(&format!("memory was not materialized: {error:#}")),
    }

    // ── The fast-path test: continue the local native session ──
    let switches_rt = args
        .as_runtime
        .as_deref()
        .map(|r| adapter::normalize(r).map(|n| n != from).unwrap_or(true))
        .unwrap_or(false);
    // VIEW is only a projection of the committed LOG. Validate the evidence carrier even when the
    // slow path will install only VIEW, so a missing/tampered event cannot be bypassed by changing
    // runtimes or by lacking a native-session link.
    let committed_log = committed_log(repo, &head, &snap)?;
    if !switches_rt
        && args.cwd.is_none()
        && !args.force
        && !history_rewrote_view(repo, &head)?
        && let Some(lk) = lk_for(repo, slug, branch)
        && lk.baseline_bytes.is_none()
    {
        // The branch head must have been settled out of this native session: committed is the
        // prefix of live, and nobody touched the VIEW after that commit (continuity covers this
        // test — objects brought in by a merge stop committed from being the prefix of live).
        if let Ok(live) = lk.read() {
            let projected = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())
                .protect_existing_jsonl(&live)?;
            if matches!(
                transcript::continuity(&committed_log, &projected.text),
                transcript::Continuity::Append | transcript::Continuity::Noop
            ) && let Some(cmd) = native_resume_cmd(
                from,
                &lk.session_id,
                &cwd,
                slug,
                branch,
                prompt,
                system_prompt.as_deref(),
            ) {
                println!(
                    "{}",
                    ui::dim(&format!(
                        "  reusing the local native session (zero-copy): {slug} @ {branch}"
                    ))
                );
                return Ok(Some(Resumed {
                    cmd: Some(cmd),
                    lossy: false,
                }));
            }
        }
    }

    // ── Slow path: materialize from the head VIEW, mint a new id ──
    materialize_and_resume(
        repo,
        slug,
        branch,
        &snap,
        from,
        args,
        &cwd,
        prompt,
        system_prompt.as_deref(),
    )
    .map(Some)
}

/// Brings the runtime up from `owner/repo` + branch, down the same path as `agit resume <branch>`.
///
/// For the TUI: the screen picks which one, and the load-and-launch rules live **here only**
/// ([`resume_branch`]). A set assembled inside the TUI duplicates them, and the symptom of two
/// load rules drifting is "the same branch resumes with different content from the interface than
/// from the command line".
pub fn launch_branch(slug: &str, branch: &str) -> CmdResult {
    let (o, n) = super::parse_slug(slug)?;
    let Some(repo) = Repo::open(config::repo_dir(&o, &n)?) else {
        ui::error(&format!("{slug} doesn’t exist locally."));
        return Ok(ExitCode::Precondition);
    };
    match resume_branch(&repo, slug, branch, &Args::default())? {
        Some(res) => finish_pub(res, false),
        None => Ok(ExitCode::Precondition),
    }
}

/// Whether this branch's history ever carried VIEW surgery (`revert` / `cherry-pick` / `merge`).
///
/// The fast path reuses the native session on the test "the committed LOG is the prefix of the
/// live transcript". VIEW surgery does not touch the LOG: what a revert takes out and what a
/// cherry-pick brings in change only the VIEW, and the LOG prefix relation still holds — so the
/// fast path sends the agent back to the **native transcript**, where it still sees the reverted
/// content and not one word of what the cherry-pick brought in. Once VIEW and LOG part ways,
/// materialization from the head VIEW is mandatory (the slow path). A merge already fails the LOG
/// continuity test; it is listed here only so the rule reads complete.
///
/// Only surgery on the **session line** counts: a reconciling merge on the file line also carries
/// `kind: merge`, and the VIEW of a session line growing out of it was never rewritten — counting
/// that merge in would permanently close the fast path for every descendant session over a single
/// operation that merges nothing but shared files.
fn history_rewrote_view(repo: &Repo, head: &str) -> crate::Result<bool> {
    Ok(first_parent_metas(repo, head)?
        .iter()
        .any(|m| m.is_session_line() && matches!(m.kind, meta::Kind::View | meta::Kind::Merge)))
}

/// The meta of every commit on the first-parent chain that carries `session/meta.json`, from the
/// head towards the root.
///
/// Every ordinary `resume` walks this path and reads as many metas as the history is long, so
/// `read_at_ref_result` one commit at a time is **out** — that call starts three git processes
/// each time, and the process count then grows linearly with the turns. This is fixed at three
/// processes: one `rev-list` to list the chain, one `cat-file --batch-check` to drop the commits
/// with no meta (legacy history pushed in from outside, the root commit), and one
/// `cat-file --batch` to read the remaining bodies in a single pass. What is read is still
/// validated strictly by [`meta::validate`]: a corrupt declaration is an error here just as on
/// the one-at-a-time path, not "absent".
fn first_parent_metas(repo: &Repo, head: &str) -> crate::Result<Vec<meta::Meta>> {
    use crate::domain::repo::ObjectBody;
    use anyhow::Context as _;
    let list = repo
        .git_opt(&["rev-list", "--first-parent", head])
        .ok_or_else(|| anyhow::anyhow!("can’t read the history of `{head}`"))?;
    let specs: Vec<String> = list
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|sha| format!("{sha}:{}", meta::FILE))
        .collect();
    let mut present = Vec::with_capacity(specs.len());
    let mut i = 0usize;
    repo.git_cat_file_batch_check(specs.clone(), |_, kind, _| {
        if kind != "missing" {
            present.push(specs[i].clone());
        }
        i += 1;
        Ok(())
    })?;
    let mut out = Vec::with_capacity(present.len());
    repo.git_cat_file_batch(present, usize::MAX, |oid, _, body| {
        let ObjectBody::Read(bytes) = body else {
            anyhow::bail!("git refused to hand over {oid}");
        };
        let m: meta::Meta = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid {} JSON at {oid}", meta::FILE))?;
        meta::validate(&m).with_context(|| format!("invalid {} metadata at {oid}", meta::FILE))?;
        out.push(m);
        Ok(())
    })?;
    Ok(out)
}

/// The LOG committed at the branch head — the evidence carrier of the continuity test.
///
/// **A missing LOG is not corruption.** A session line fresh out of `new` / `import`, carrying
/// only that `agit: claim session line` commit, has not claimed an identity yet (`session` is
/// empty) and its tree is not supposed to hold a LOG — that is exactly what `declare_session_line`
/// writes, and the startup migration `migrate_tip` changes only the meta on such a tip without
/// adding a LOG/VIEW. Treating it as corrupt makes that branch unresumable forever.
///
/// The test matches every sibling reader (`settle_bytes` in commit, `materialize_optional` in
/// revert / cherry-pick, `turn_lines` in merge): the file line and a session line with no claimed
/// identity are handled as an empty LOG; a missing LOG on a branch that **has claimed an
/// identity** is real corruption and still fails hard — that fail-closed discipline is not
/// loosened by one word.
fn committed_log(repo: &Repo, head: &str, snap: &meta::Meta) -> crate::Result<String> {
    if snap.is_file_line() || snap.session.is_empty() {
        return Ok(String::new());
    }
    crate::domain::storage::materialize_at(repo.root(), head, meta::LOG_FILE).map_err(|error| {
        anyhow::anyhow!("cannot resume from a corrupt committed LOG at {head}: {error:#}")
    })
}

/// Finds the store link serving this branch.
fn lk_for(_repo: &Repo, slug: &str, branch: &str) -> Option<Link> {
    let store = Store::open_or_init().ok()?;
    let (owner, name) = slug.split_once('/').unwrap_or(("", slug));
    let mut hits: Vec<Link> = link::list(&store)
        .into_iter()
        .filter(|l| {
            l.agent.as_deref() == Some(name)
                && l.branch.as_deref() == Some(branch)
                // A link that records a namespace belongs to that namespace only; one that
                // records none is a legacy link, kept compatible by name.
                && l.owner.as_deref().is_none_or(|o| o == owner)
        })
        .collect();
    if hits.len() == 1 { hits.pop() } else { None }
}

/// The native resume command (each harness's own resume verb).
///
/// A non-empty `prompt` is appended in each harness's native form as a **separate argv**
/// (`claude --resume <id> "<prompt>"` / `codex resume <id> [PROMPT]`), with quotes and newlines
/// caught by [`shell_quote`] — this command ends up at `sh -c`.
fn native_resume_cmd(
    runtime: &str,
    sid: &str,
    cwd: &Path,
    slug: &str,
    branch: &str,
    prompt: Option<&str>,
    system_prompt: Option<&str>,
) -> Option<String> {
    let mut inner = match runtime {
        "claude-code" => format!("claude --resume {sid}"),
        "codex" => format!("codex resume {sid}"),
        _ => return None,
    };
    if let Some(system) = system_prompt {
        match runtime {
            "claude-code" => {
                inner.push_str(" --append-system-prompt ");
                inner.push_str(&shell_quote(system));
            }
            "codex" => {
                let value = serde_json::to_string(system).ok()?;
                inner.push_str(" -c ");
                inner.push_str(&shell_quote(&format!("developer_instructions={value}")));
            }
            _ => return None,
        }
    }
    if let Some(p) = prompt {
        inner.push(' ');
        inner.push_str(&shell_quote(p));
    }
    Some(wrap_launch(&inner, cwd, slug, branch))
}

fn handoff_fallback(
    fallback: &str,
    sid: &str,
    cwd: &Path,
    slug: &str,
    branch: &str,
    prompt: Option<&str>,
    system_prompt: Option<&str>,
) -> String {
    if prompt.is_none() && system_prompt.is_none() {
        return fallback.to_owned();
    }
    native_resume_cmd("claude-code", sid, cwd, slug, branch, prompt, system_prompt)
        .unwrap_or_else(|| fallback.to_owned())
}

/// Materializes the VIEW into the runtime and prepares the resume command.
// The parameter list is long because loading needs exactly these facts; packing them into a
// struct only adds a layer of indirection.
#[allow(clippy::too_many_arguments)]
fn materialize_and_resume(
    repo: &Repo,
    slug: &str,
    branch: &str,
    _snap: &meta::Meta,
    from: &str,
    args: &Args,
    cwd: &Path,
    prompt: Option<&str>,
    system_prompt: Option<&str>,
) -> crate::Result<Resumed> {
    let to = match &args.as_runtime {
        Some(r) => adapter::normalize(r)?,
        None => config::get_global("runtime.default")?
            .as_deref()
            .and_then(|r| adapter::normalize(r).ok())
            .unwrap_or(from),
    };

    // Cursor is import-only: refused before any work starts (PRD).
    let dst_ad = adapter::get(to)?;
    if !matches!(
        dst_ad.capability(),
        adapter::Capability::Resumable | adapter::Capability::ExportOnly
    ) {
        ui::error(&format!("{to} is import-only — can’t install into it."));
        ui::hint("cross-harness targets: claude-code / codex / opencode");
        anyhow::bail!("import-only target");
    }
    if !args.no_launch
        && matches!(dst_ad.capability(), adapter::Capability::Resumable)
        && !dst_ad.available()
    {
        ui::error(&format!(
            "the {to} executable `{}` isn’t on PATH.",
            dst_ad.cli()
        ));
        ui::hint("--no-launch prepares the session without starting it");
        anyhow::bail!("runtime not on PATH");
    }

    // The materialized content = the original lines unwrapped from the branch head's VIEW (not
    // the full log).
    let view_env = crate::domain::storage::materialize_at(
        repo.root(),
        &format!("refs/heads/{branch}"),
        meta::VIEW_FILE,
    )
        .map_err(|_| {
            anyhow::anyhow!(
                "{branch} has no {} yet — this session line hasn’t settled a turn (`agit commit` first), or the checkout is incomplete",
                meta::VIEW_FILE
            )
        })?;
    let (text, skipped) = transcript::unwrap_lossy(&view_env);
    // A compact-anchored VIEW carries no leading `session_meta` line, and that line is more than
    // bootstrap: `history_mode` / `model_provider` / `base_instructions` all sit in it, and a
    // synthesized fallback cannot supply them. The original lies on the LOG's first line — ask
    // whether it is needed first, then materialize the first event only, rather than reading the
    // whole LOG for one bootstrap line; the identity keys are rewritten uniformly by the load
    // afterwards.
    let text = if transcript::needs_bootstrap(&text, from) {
        match crate::domain::storage::materialize_head_at(
            repo.root(),
            &format!("refs/heads/{branch}"),
            meta::LOG_FILE,
        ) {
            Ok(Some(head)) => transcript::restore_bootstrap(&text, &head, from),
            _ => text,
        }
    } else {
        text
    };
    if skipped > 0 {
        ui::warning(&format!(
            "{} had {skipped} corrupt lines — skipped.",
            meta::VIEW_FILE
        ));
    }
    if text.trim().is_empty() {
        ui::error(&format!(
            "the VIEW of `{branch}` is empty — nothing to resume from."
        ));
        ui::hint("a fresh session goes through `agit new`");
        anyhow::bail!("empty VIEW");
    }
    let hydrated = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())
        .hydrate_jsonl(&text)?;
    if hydrated.unresolved > 0 {
        ui::warning(&format!(
            "{} repository secret placeholder(s) have no local dictionary entry and were left unchanged.",
            hydrated.unresolved
        ));
        ui::hint(
            "repository secret dictionaries are device-local and are never fetched from the hub",
        );
    }
    let text = hydrated.text;

    // The loss list: printed before any work starts, confirmed on a tty (PRD, "print the loss
    // list before a cross-harness conversion").
    let lossy = adapter::is_lossy_conversion(from, to);
    if lossy {
        println!("  cross-runtime ({from} → {to}) via IR:");
        println!(
            "  kept: messages, tool calls; arguments and paired outputs when recoverable from the source transcript, thinking (best effort)"
        );
        println!("  lost: encrypted reasoning, vendor encodings, compact boundaries");
        if ui::is_tty() && std::env::var("AGIT_YES").is_err() {
            match ui::prompt::confirm("proceed?", false)? {
                Some(true) => {}
                _ => {
                    println!("cancelled.");
                    anyhow::bail!("cancelled by user");
                }
            }
        }
    }

    let (installed, _) = crate::domain::install::install(&text, from, to, cwd)?;

    // Registration: the new instance's baseline = the byte count and hash of the materialized
    // file at this moment; the identity is injected as `AGIT_SESSION`.
    let sid = installed
        .path
        .file_stem()
        .map(|s| adapter::session_id_from_stem(&s.to_string_lossy()))
        .unwrap_or_default();
    use sha2::Digest as _;
    let mut lk = Link::new(to, &sid, Some(cwd));
    match slug.split_once('/') {
        Some((owner, agent)) => {
            lk.owner = Some(owner.to_string());
            lk.agent = Some(agent.to_string());
        }
        None => lk.agent = Some(slug.to_string()),
    }
    lk.branch = Some(branch.to_string());
    // The baseline must be taken down **the same path that later reads the live transcript**
    // (`lk.read_bytes` → resolve), never by reading the file `install` dropped: for a file-backed
    // runtime the two are the same bytes, for a library-backed one they are not — OpenCode is
    // installed with the export payload, while the live transcript reads the canonical line set
    // materialized out of the library. Take the former as the baseline and the very first
    // comparison misreads a healthy session as "truncated". A read that fails propagates: with no
    // authoritative live bytes, better to record no link than to substitute another
    // representation — the same holds for an empty-baseline fallback, which would treat the whole
    // history as the appended range. Only an ExportOnly target, which has no read side (the
    // installed file is itself the only carrier of truth), uses the installed file.
    use anyhow::Context as _;
    let materialized = if baseline_reads_live(dst_ad.capability()) {
        lk.read_bytes().context(
            "cannot read the just-installed session back through its runtime — refusing to record a baseline from the install receipt",
        )?
    } else {
        // A target with no read side does not swallow the error either: an unreadable installed
        // file means there is no baseline to record.
        std::fs::read(&installed.path)
            .with_context(|| format!("cannot read {}", installed.path.display()))?
    };
    lk.baseline_bytes = Some(materialized.len() as u64);
    lk.baseline_hash = Some(hex::encode(sha2::Sha256::digest(&materialized)));
    if let Ok(store) = Store::open_or_init()
        && let Err(e) = link::write(&store, &lk)
    {
        ui::warning(&format!("failed to record the link: {e:#}"));
    }

    println!(
        "  {} materialized VIEW → {} {}",
        ui::ok(ui::theme::symbols().check),
        ui::dim(&ui::tilde(&installed.path)),
        if lossy {
            ui::warn_text("(lossy)").to_string()
        } else {
            String::new()
        }
    );

    let cmd = match &installed.next {
        // With an opening prompt the command is rebuilt in the harness's native form: the string
        // the adapter hands back has nowhere to put the prompt (the paren of
        // `(cd ... && claude --resume ID)` closes at the end), and appending to it assembles a
        // command that cannot run.
        adapter::Next::Resume(c) => {
            match native_resume_cmd(to, &sid, cwd, slug, branch, prompt, system_prompt) {
                Some(c2) => Some(c2),
                None => {
                    if system_prompt.is_some() {
                        ui::warning(&format!(
                            "{to} cannot receive a system environment notice on resume; continuing without injection"
                        ));
                    }
                    match prompt {
                        Some(p) => {
                            // This runtime has no "resume carrying a prompt" form. The
                            // instruction must not be dropped over that — print it for the
                            // person at the terminal to paste in, rather than leaving the agent
                            // idle once it starts.
                            ui::warning(&format!(
                                "{to} can’t take an opening prompt on resume — paste this in as the first message:"
                            ));
                            println!("{p}");
                            Some(inject_session_env(c, slug, branch))
                        }
                        None => Some(inject_session_env(c, slug, branch)),
                    }
                }
            }
        }
        adapter::Next::HandOff { trigger, fallback } => {
            if system_prompt.is_some() {
                ui::warning(
                    "the desktop handoff deep link cannot carry the environment notice; use the CLI fallback below to resume with it",
                );
            }
            let fallback =
                handoff_fallback(fallback, &sid, cwd, slug, branch, prompt, system_prompt);
            println!("  {}", ui::accent(trigger));
            println!(
                "  {}",
                ui::dim(&format!("the guaranteed way if handoff fails: {fallback}"))
            );
            None
        }
    };
    Ok(Resumed { cmd, lossy })
}

/// Injects `AGIT_SESSION` into the launch command.
fn inject_session_env(cmd: &str, slug: &str, branch: &str) -> String {
    cmd.replacen(
        "(cd ",
        &format!(
            "(export AGIT_SESSION={}; cd ",
            shell_quote(&super::context::encode_session_env(slug, branch))
        ),
        1,
    )
}

/// Assembles the launch command uniformly: `(export AGIT_SESSION=...; cd <dir> && <runtime
/// command>)`.
fn wrap_launch(inner: &str, cwd: &Path, slug: &str, branch: &str) -> String {
    format!(
        "(export AGIT_SESSION={}; cd {} && {})",
        shell_quote(&super::context::encode_session_env(slug, branch)),
        shell_quote(&cwd.to_string_lossy()),
        inner
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The finish: print or launch.
fn finish(res: Resumed, no_launch: bool) -> CmdResult {
    finish_pub(res, no_launch)
}

/// The finish that fork / run reuse.
pub fn finish_pub(res: Resumed, no_launch: bool) -> CmdResult {
    match res.cmd {
        Some(cmd) => {
            if no_launch {
                println!("\n  {}", ui::accent(&cmd));
                return Ok(ExitCode::Ok);
            }
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status()
                .map_err(|e| anyhow::anyhow!("couldn’t launch: {e}"))?;
            Ok(match status.code() {
                Some(0) | None => ExitCode::Ok,
                Some(_) => ExitCode::Precondition,
            })
        }
        None => Ok(ExitCode::Ok),
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::meta;
    use crate::domain::repo::Repo;
    use crate::domain::transcript;
    use std::path::Path;

    /// A branch carrying only that one `agit: claim session line` commit: it declares itself a
    /// session line, has not claimed an identity, and holds no LOG / VIEW in its tree.
    fn claimed_but_never_settled() -> (tempfile::TempDir, Repo, String) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::ensure_session_dir(r.root()).unwrap();
        meta::write(
            r.root(),
            &meta::Meta::new_session_line("claude-code".into(), "/r".into()),
        )
        .unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: claim session line").unwrap());
        let head = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        (d, r, head)
    }

    /// A branch that has claimed the form but has not settled a turn must still resume.
    ///
    /// # What this pins
    ///
    /// Such a tip has no LOG in its tree — that is exactly what `declare_session_line` writes,
    /// and the startup migration `migrate_tip` changes only its meta without adding a LOG/VIEW.
    /// Treat "no LOG to read" as corruption and `cannot resume from a corrupt committed LOG`
    /// stands in front of everything, so the fast path is never even tried — a branch fresh out
    /// of `new` / `import` that has not settled is then unreachable for good.
    ///
    /// The assertions land on the observable consequence: the committed LOG is empty, and an
    /// empty committed LOG is `Append` against any live transcript — precisely the condition the
    /// fast path needs.
    #[test]
    fn a_claimed_but_unsettled_line_still_resumes() {
        let (_d, r, head) = claimed_but_never_settled();
        let snap = meta::read_at_ref(&r, &head).unwrap();
        assert!(
            snap.is_session_line() && snap.session.is_empty(),
            "precondition: the form is claimed, the identity is not"
        );
        assert!(
            r.show_result(&head, meta::LOG_FILE).unwrap().is_none(),
            "precondition: such a tip has no LOG in its tree"
        );

        let committed = super::committed_log(&r, &head, &snap)
            .unwrap_or_else(|e| panic!("an unsettled session line is not corrupt: {e:#}"));
        assert!(committed.is_empty(), "{committed}");
        assert!(
            matches!(
                transcript::continuity(&committed, "{\"a\":1}\n"),
                transcript::Continuity::Append
            ),
            "this is precisely the verdict the fast path needs"
        );
    }

    /// Lands one more commit on top of the current HEAD: changes the meta's kind (and line
    /// form) and drops a marker file in the tree so the commit always takes. Returns the new
    /// HEAD.
    fn stack(r: &Repo, kind: meta::Kind, line: meta::Line, marker: &str) -> String {
        let head = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let mut snap = meta::read_at_ref(r, &head).unwrap();
        snap.kind = kind;
        snap.line = line;
        meta::write(r.root(), &snap).unwrap();
        std::fs::write(r.root().join("marker"), marker).unwrap();
        r.add_all().unwrap();
        assert!(r.commit(marker).unwrap());
        r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string()
    }

    /// The fast path is forbidden after VIEW surgery: the LOG is unchanged, but the context the
    /// agent is meant to see is not.
    #[test]
    fn a_view_surgery_in_the_history_forces_materialization() {
        let (_d, r, head) = claimed_but_never_settled();
        assert!(!super::history_rewrote_view(&r, &head).unwrap());

        let after = stack(&r, meta::Kind::View, meta::Line::Session, "revert");
        assert!(super::history_rewrote_view(&r, &after).unwrap());

        // Settling another turn afterwards leaves the surgery in the history: the verdict does
        // not flip back because the head is a turn again.
        let later = stack(&r, meta::Kind::Turn, meta::Line::Session, "turn");
        assert!(super::history_rewrote_view(&r, &later).unwrap());
    }

    /// A reconciling merge on the file line is not VIEW surgery: a session line growing out of
    /// it still takes the fast path.
    #[test]
    fn a_file_line_merge_below_a_session_keeps_the_fast_path() {
        let (_d, r, _head) = claimed_but_never_settled();
        let merged = stack(
            &r,
            meta::Kind::Merge,
            meta::Line::File,
            "reconcile shared files",
        );
        assert!(!super::history_rewrote_view(&r, &merged).unwrap());
        let session = stack(
            &r,
            meta::Kind::Turn,
            meta::Line::Session,
            "turn on the new session",
        );
        assert!(!super::history_rewrote_view(&r, &session).unwrap());
        // The same kind counts only when it lands on the session line.
        let surgery = stack(&r, meta::Kind::Merge, meta::Line::Session, "session merge");
        assert!(super::history_rewrote_view(&r, &surgery).unwrap());
    }

    /// A long history is read in one pass: every commit carrying a meta is in the list, and a
    /// root commit without a meta is not an error, merely absent from it.
    #[test]
    fn first_parent_metas_reads_a_long_history_in_one_pass() {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        // The root commit carries no meta (this is what legacy history pushed in from outside
        // looks like).
        std::fs::write(r.root().join("README"), "root").unwrap();
        r.add_all().unwrap();
        assert!(r.commit("root without meta").unwrap());
        meta::ensure_session_dir(r.root()).unwrap();
        meta::write(
            r.root(),
            &meta::Meta::new_session_line("claude-code".into(), "/r".into()),
        )
        .unwrap();
        r.add_all().unwrap();
        assert!(r.commit("agit: claim session line").unwrap());
        let turns = 40;
        for i in 0..turns {
            stack(
                &r,
                meta::Kind::Turn,
                meta::Line::Session,
                &format!("turn {i}"),
            );
        }
        let head = r.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let metas = super::first_parent_metas(&r, &head).unwrap();
        assert_eq!(
            metas.len(),
            turns + 1,
            "claim + every turn; the meta-less root is skipped"
        );
        assert_eq!(
            metas.iter().filter(|m| m.kind == meta::Kind::Turn).count(),
            turns
        );
        assert!(!super::history_rewrote_view(&r, &head).unwrap());
    }

    /// The exemption covers only the "identity not claimed yet" stretch: a missing LOG on a
    /// branch that has claimed one is still corruption.
    #[test]
    fn a_claimed_session_line_missing_its_log_is_still_corrupt() {
        let (_d, r, head) = claimed_but_never_settled();
        let mut snap = meta::read_at_ref(&r, &head).unwrap();
        snap.session = format!("agit-{}", "c".repeat(40));
        let e = super::committed_log(&r, &head, &snap)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("cannot resume from a corrupt committed LOG"),
            "{e}"
        );
    }

    #[test]
    fn a_non_git_resume_cwd_does_not_block_when_state_comparison_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut snapshot = meta::Meta::new(
            "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "codex".into(),
            "/previous/checkout".into(),
        );
        snapshot.cwd_state = Some(meta::CwdState {
            origin: Some("https://example.invalid/team/app.git".into()),
            head: Some("b".repeat(40)),
            branch: Some("main".into()),
            worktree: meta::WorktreeStatus::Clean,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
            status_digest: Some("c".repeat(64)),
        });

        assert!(matches!(
            super::cwd_resume_decision(&snapshot, dir.path()).unwrap(),
            super::CwdResumeDecision::Continue
        ));
    }

    #[test]
    fn env_injection_is_shell_safe() {
        let c = super::wrap_launch(
            "claude --resume ABC",
            std::path::Path::new("/m/my dir"),
            "me/repo",
            "b",
        );
        assert!(c.contains("AGIT_SESSION='me/repo@b'"));
        assert!(c.contains("cd '/m/my dir'"));
        assert!(c.starts_with("(export"));
    }

    /// An opening prompt rides each harness's native form, appended to the resume command as a
    /// separate argv; the cwd environment notice is injected through each harness's
    /// system/developer instructions entry point.
    #[test]
    fn an_opening_prompt_rides_the_native_resume_verb() {
        let cc = super::native_resume_cmd(
            "claude-code",
            "ABC",
            Path::new("/w"),
            "me/r",
            "b",
            Some("merge it"),
            None,
        )
        .unwrap();
        assert!(cc.contains("claude --resume ABC 'merge it'"), "{cc}");
        let cx = super::native_resume_cmd(
            "codex",
            "ABC",
            Path::new("/w"),
            "me/r",
            "b",
            Some("merge it"),
            None,
        )
        .unwrap();
        assert!(cx.contains("codex resume ABC 'merge it'"), "{cx}");
        // With no prompt not one word is added.
        let bare =
            super::native_resume_cmd("codex", "ABC", Path::new("/w"), "me/r", "b", None, None)
                .unwrap();
        assert!(bare.ends_with("codex resume ABC)"), "{bare}");
    }

    /// A prompt always carries quotes (`agit merge summary -m "..."`) and newlines — this
    /// command goes to `sh -c`, and without escaping it blows up on the spot.
    #[test]
    fn an_opening_prompt_survives_quotes_and_newlines() {
        let p = "run: agit merge summary -m \"it's done\"\nthen --continue";
        let c = super::native_resume_cmd(
            "claude-code",
            "ID",
            Path::new("/w"),
            "me/r",
            "b",
            Some(p),
            None,
        )
        .unwrap();
        assert!(
            c.contains(r"'\''"),
            "a single quote must be closed before it is escaped: {c}"
        );
        assert!(c.contains("then --continue"));
        // The whole string is still a valid sh command: hand it to sh to parse once.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "set -- {}; printf '%s' \"$1\"",
                super::shell_quote(p)
            ))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), p);
    }

    #[test]
    fn cwd_notice_uses_native_system_instruction_options() {
        let notice = "recorded state differs\ncheck the current checkout";
        let claude = super::native_resume_cmd(
            "claude-code",
            "ID",
            Path::new("/w"),
            "me/r",
            "b",
            None,
            Some(notice),
        )
        .unwrap();
        assert!(claude.contains("--append-system-prompt"), "{claude}");
        assert!(claude.contains("recorded state differs"), "{claude}");

        let codex = super::native_resume_cmd(
            "codex",
            "ID",
            Path::new("/w"),
            "me/r",
            "b",
            None,
            Some(notice),
        )
        .unwrap();
        assert!(codex.contains("developer_instructions="), "{codex}");
        assert!(codex.contains("recorded state differs"), "{codex}");
    }

    #[test]
    fn unknown_cwd_state_is_not_treated_as_equal() {
        let base = meta::CwdState {
            origin: Some("https://example.invalid/team/app.git".into()),
            head: Some("a".repeat(40)),
            branch: Some("main".into()),
            worktree: meta::WorktreeStatus::Unknown,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
            status_digest: None,
        };
        assert_eq!(
            super::compare_cwd_state(&base, &base),
            super::CwdStateComparison::Unknown
        );

        let mut changed_identity = base.clone();
        changed_identity.branch = Some("topic".into());
        assert_eq!(
            super::compare_cwd_state(&base, &changed_identity),
            super::CwdStateComparison::Different
        );
    }

    #[test]
    fn handoff_fallback_carries_resume_overrides() {
        let fallback = super::handoff_fallback(
            "(cd /w && claude --resume ID)",
            "ID",
            Path::new("/w"),
            "me/r",
            "main",
            Some("continue the work"),
            Some("the checkout changed"),
        );
        assert!(fallback.contains("AGIT_SESSION='me/r@main'"), "{fallback}");
        assert!(fallback.contains("--append-system-prompt"), "{fallback}");
        assert!(fallback.contains("the checkout changed"), "{fallback}");
        assert!(fallback.contains("continue the work"), "{fallback}");
    }
}

/// Where the baseline bytes come from: whichever path settlement later reads the live transcript
/// down, the baseline is taken from that same path. An ExportOnly target has no read side, and
/// the installed file is itself the only carrier of truth.
fn baseline_reads_live(capability: adapter::Capability) -> bool {
    !matches!(capability, adapter::Capability::ExportOnly)
}

#[cfg(test)]
mod baseline_tests {
    use super::*;

    /// Every resumable target takes its baseline from the live-read path (on a library-backed
    /// runtime like OpenCode the install receipt and the live transcript are not the same
    /// bytes); only a target with no read side uses the installed file.
    #[test]
    fn baseline_source_follows_the_settlement_read_path() {
        for rt in ["claude-code", "codex", "opencode"] {
            let cap = crate::adapter::get(rt).unwrap().capability();
            assert!(
                baseline_reads_live(cap),
                "the baseline of {rt} must come from the live read"
            );
        }
        let desktop = crate::adapter::get("claude-desktop").unwrap().capability();
        assert!(
            !baseline_reads_live(desktop),
            "a target with no read side has only the installed file"
        );
    }
}
