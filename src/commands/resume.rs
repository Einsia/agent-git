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
use crate::ui::quote_posix_argument as shell_quote;
#[cfg(windows)]
use crate::ui::quote_powershell_argument as powershell_recovery_arg;
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
    /// Merge publication and process spawning share admission with transaction cancellation.
    pub(crate) merge_launch_guard: Option<crate::domain::mergetx::ControlGuard>,
    launch_messages: Vec<LaunchMessage>,
}

enum LaunchMessage {
    Line(String),
    Warning(String),
}

impl Resumed {
    pub(crate) fn emit_launch_messages(&mut self) {
        for message in self.launch_messages.drain(..) {
            match message {
                LaunchMessage::Line(line) => println!("{line}"),
                LaunchMessage::Warning(warning) => ui::warning(&warning),
            }
        }
    }
}

fn launch_line(messages: &mut Vec<LaunchMessage>, defer: bool, line: String) {
    if defer {
        messages.push(LaunchMessage::Line(line));
    } else {
        println!("{line}");
    }
}

fn launch_warning(messages: &mut Vec<LaunchMessage>, defer: bool, warning: &str) {
    if defer {
        messages.push(LaunchMessage::Warning(warning.to_owned()));
    } else {
        ui::warning(warning);
    }
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

    let source = args
        .target
        .as_deref()
        .map(refs::parse)
        .transpose()?
        .as_ref()
        .map(|spec| match (&spec.repo, &spec.base) {
            (refs::RepoSel::Slug(_, _) | refs::RepoSel::Local(_), _) => {
                super::echo::Source::Explicit
            }
            (_, refs::Base::At | refs::Base::SessionBranch(_)) => super::echo::Source::Environment,
            _ => super::echo::Source::Mixed,
        })
        .unwrap_or(super::echo::Source::Interactive);
    match resume_branch_for(
        &repo,
        &slug,
        &branch,
        &args,
        None,
        ResumeRequest {
            purpose: ResumePurpose::Continue,
            echo_source: Some(source),
        },
    )? {
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
        let spec = super::target::resolve_local_repo(refs::parse(t)?)?;
        let (slug, base_name) = match &spec.repo {
            refs::RepoSel::Slug(o, n) => {
                let name = match &spec.base {
                    refs::Base::Name(b) | refs::Base::SessionBranch(b) => b.clone(),
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
            refs::RepoSel::Local(_) => unreachable!("local repository qualifiers are resolved"),
            refs::RepoSel::Context => {
                // A bare branch still needs the repository supplied by AGIT_SESSION.
                match &spec.base {
                    refs::Base::Name(b) | refs::Base::SessionBranch(b) => {
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
                                "use `agit resume <owner>/<repo>@<branch>`, or `agit resume @` with AGIT_SESSION",
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
            ui::hint_with_fix(
                &format!("fetch it first: `agit clone --no-bind {slug}`"),
                || super::fix::FixCommand::current(&["clone", "--no-bind", "--", &slug], false),
            );
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
            ui::hint("be explicit: `agit resume <owner>/<repo>@<branch>`");
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
            if l.is_active()
                && l.cwd.as_deref() == Some(cwd_s.as_str())
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

/// Candidate badges compare complete repository identities. Prefix matches would associate
/// unrelated projects, and transport spelling must not hide the same remote repository.
pub(crate) fn same_repo_as(code: &str, origin: &str) -> bool {
    let Some((recorded, sha)) = code.rsplit_once('@') else {
        return false;
    };
    if sha.len() < 4 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let origin = origin.trim();
    if !origin.is_empty() && recorded == origin {
        return true;
    }
    match (normalize_origin(recorded), normalize_origin(origin)) {
        (Some(recorded), Some(current)) => recorded == current,
        _ => false,
    }
}

fn normalize_origin(origin: &str) -> Option<String> {
    let origin = origin.trim();
    if origin.is_empty() {
        return None;
    }
    if origin.split_once("::").is_some_and(|(transport, _)| {
        !transport.is_empty()
            && transport
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+._-".contains(&byte))
    }) {
        return Some(format!("literal:{origin}"));
    }
    let scp_host = match origin.split_once('@') {
        Some((user, host)) if !user.contains([':', '/', '\\']) => host,
        _ => origin,
    };
    let (authority, path, scheme, scp) = if let Some((scheme, rest)) = origin.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return Some(format!("literal:{origin}"));
        }
        let (authority, path) = rest.split_once('/')?;
        (authority, path, scheme, false)
    } else if scp_host.starts_with('[') {
        let end = origin.find("]:")?;
        (&origin[..end + 1], &origin[end + 2..], "ssh", true)
    } else if let Some((authority, path)) = origin.split_once(':') {
        if authority.len() <= 1 || authority.contains(['/', '\\', '[']) {
            return Some(format!("literal:{origin}"));
        }
        (authority, path, "ssh", true)
    } else {
        return Some(format!("literal:{origin}"));
    };
    let (user, authority) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(user, host)| (Some(user), host));
    let authority = match authority.rsplit_once(':') {
        Some((host, port))
            if matches!(
                (scheme, port),
                ("https", "443") | ("http", "80") | ("ssh", "22") | ("git", "9418")
            ) =>
        {
            host
        }
        _ => authority,
    };
    let (path, home_relative) = if scp {
        path.strip_prefix('/')
            .map_or((path, true), |path| (path, path.starts_with('~')))
    } else {
        (path, scheme == "ssh" && path.starts_with('~'))
    };
    let path = path.trim_end_matches('/');
    let named_home = home_relative && path.starts_with('~') && !path.starts_with("~/");
    let path = if home_relative {
        path.strip_prefix("~/").unwrap_or(path)
    } else {
        path
    };
    if authority.is_empty() || path.is_empty() {
        return None;
    }
    // Named SSH homes retain their literal paths. Only the git login's own namespace can
    // match an HTTP forge path; other login homes must not collapse into each other.
    if home_relative && (user != Some("git") || named_home) {
        let namespace = if named_home {
            "ssh-named-home"
        } else {
            "ssh-home"
        };
        return Some(format!(
            "{namespace}:{user:?}@{}/{path}",
            canonical_origin_authority(authority)
        ));
    }
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    // Repository paths can be case-sensitive; custom ports can name different services.
    Some(format!(
        "remote:{}/{path}",
        canonical_origin_authority(authority)
    ))
}

fn canonical_origin_authority(authority: &str) -> String {
    let zone_start = authority
        .strip_prefix('[')
        .and_then(|host| host.split_once(']'))
        .and_then(|(address, _)| address.find('%'))
        .map(|offset| offset + 1);
    match zone_start {
        // Zone identifiers name interfaces whose spelling can be case-sensitive.
        Some(zone_start) => format!(
            "{}{}",
            authority[..zone_start].to_ascii_lowercase(),
            &authority[zone_start..]
        ),
        None => authority.to_ascii_lowercase(),
    }
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
    resume_branch_for(
        repo,
        slug,
        branch,
        args,
        prompt,
        ResumeRequest {
            purpose: ResumePurpose::Continue,
            echo_source: None,
        },
    )
}

/// Prepare the agent for an open merge whose source and target identities are frozen.
pub(crate) fn resume_merge_agent(
    repo: &Repo,
    slug: &str,
    tx: &crate::domain::mergetx::Tx,
    args: &Args,
    prompt: &str,
) -> crate::Result<Option<Resumed>> {
    resume_branch_for(
        repo,
        slug,
        &tx.target,
        args,
        Some(prompt),
        ResumeRequest {
            purpose: ResumePurpose::Merge(tx),
            echo_source: None,
        },
    )
}

#[derive(Clone, Copy)]
enum ResumePurpose<'a> {
    Continue,
    Merge(&'a crate::domain::mergetx::Tx),
}

struct ResumeRequest<'a> {
    purpose: ResumePurpose<'a>,
    echo_source: Option<super::echo::Source>,
}

impl ResumePurpose<'_> {
    fn require_transaction(self, repo: &Repo, branch: &str, head: &str) -> crate::Result<()> {
        if let Self::Merge(expected) = self {
            let active = crate::domain::mergetx::read(repo.root())?
                .ok_or_else(|| anyhow::anyhow!("the merge transaction is no longer open"))?;
            anyhow::ensure!(
                expected.generation.is_some()
                    && expected.target == branch
                    && expected.target_head == head
                    && active.same_instance(expected),
                "the merge transaction changed while preparing its agent; inspect `agit merge --status`"
            );
        }
        Ok(())
    }
}

fn resume_branch_for(
    repo: &Repo,
    slug: &str,
    branch: &str,
    args: &Args,
    prompt: Option<&str>,
    request: ResumeRequest<'_>,
) -> crate::Result<Option<Resumed>> {
    let ResumeRequest {
        purpose,
        echo_source,
    } = request;
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
    let store = Store::open_or_init()?;
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

    if let Some(source) = echo_source {
        super::echo::emit(
            "resume",
            &[super::echo::Selection::new(
                format!("{slug}@{branch}"),
                source,
            )],
        );
    }

    purpose.require_transaction(repo, branch, &head)?;
    let tracking = ResumeTracking::read(repo, branch)?;
    if matches!(purpose, ResumePurpose::Continue)
        && !tracking.is_integrated(repo, slug, branch, &head)?
    {
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
    let cwd = std::path::absolute(cwd)?;

    // `cwd_state` is an observation, not a checkout instruction. Compare it before either
    // native reuse or VIEW materialization so both resume paths make the same decision.
    let system_prompt = match cwd_resume_decision(&snap, &cwd)? {
        CwdResumeDecision::Continue => None,
        CwdResumeDecision::Inject(prompt) => Some(prompt),
        CwdResumeDecision::Cancel => return Ok(None),
    };

    // Prompts cannot hold the branch lock: a runtime waiting to settle must remain able to
    // finish while the operator decides. The selected snapshot must still be current afterward.
    let branch_guard = link::lock_branch(&store, slug, branch)?;
    require_resume_state(repo, branch, &head, &tracking)?;
    purpose.require_transaction(repo, branch, &head)?;
    if matches!(purpose, ResumePurpose::Merge(_)) {
        require_merge_claims(repo, &store, slug, branch, &head, false)?;
    }

    // ── Memory: the branch's memory merges into the target runtime's memory dir (both paths) ──
    let from = snap.runtime.as_str();
    let requested_runtime = args
        .as_runtime
        .as_deref()
        .map(adapter::normalize)
        .transpose()?;
    let to_runtime = requested_runtime
        .or_else(|| {
            config::get_global("runtime.default")
                .ok()
                .flatten()
                .as_deref()
                .and_then(|runtime| adapter::normalize(runtime).ok())
        })
        .unwrap_or(from);
    // ── The fast-path test: continue the local native session ──
    let switches_rt = requested_runtime.is_some_and(|runtime| runtime != from);
    // VIEW is only a projection of the committed LOG. Validate the evidence carrier even when the
    // slow path will install only VIEW, so a missing/tampered event cannot be bypassed by changing
    // runtimes or by lacking a native-session link.
    let committed_log = committed_log(repo, &head, &snap)?;
    let (owner, agent) = slug.split_once('/').unwrap_or(("", slug));
    let active = link::active_for_branch(&store, owner, agent, branch);
    if active.len() > 1 && !args.force {
        report_multiple_active(slug, branch, &active);
        return Ok(None);
    }

    // A repeated prepare of the same branch tip is idempotent. Appended content is safe to
    // resume in place too: replacement is forbidden, but continuing the same writer loses
    // nothing. A rewritten or unreadable baseline is not reused implicitly.
    let reuse_prepared = |active: &[Link]| {
        if matches!(purpose, ResumePurpose::Continue)
            && !args.force
            && let [existing] = active
            && existing.materialized_from.as_deref() == Some(head.as_str())
            && requested_runtime.is_none_or(|runtime| existing.source == runtime)
            && existing.cwd.as_deref() == Some(cwd.to_string_lossy().as_ref())
            && matches!(
                link::materialization_activity(existing),
                link::MaterializationActivity::Untouched | link::MaterializationActivity::Appended
            )
            && let Some(resumed) = prepared_resume(
                &existing.source,
                &existing.session_id,
                &cwd,
                slug,
                branch,
                prompt,
                system_prompt.as_deref(),
            )
        {
            materialize_memory(repo, branch, slug, &existing.source, &cwd);
            println!(
                "{}",
                ui::dim(&format!(
                    "  reusing the prepared runtime session: {} {}",
                    existing.source,
                    link::short(&existing.session_id)
                ))
            );
            return Some(resumed);
        }
        None
    };
    if let Some(resumed) = reuse_prepared(&active) {
        return Ok(Some(resumed));
    }

    if matches!(purpose, ResumePurpose::Continue)
        && !switches_rt
        && args.cwd.is_none()
        && !args.force
        && !history_requires_view_materialization(repo, &head)?
        && let [lk] = active.as_slice()
        && lk.baseline_bytes.is_none()
        && head_view_matches_log(repo, &head, &snap)?
    {
        // Native reuse requires both saved VIEW equality and a native LOG prefix. History
        // overlays cannot authorize replaying evidence excluded from the current snapshot.
        if let Ok(live) = lk.read() {
            let projected = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?
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
                materialize_memory(repo, branch, slug, from, &cwd);
                println!(
                    "{}",
                    ui::dim(&format!(
                        "  reusing the local native session (zero-copy): {slug} @ {branch}"
                    ))
                );
                return Ok(Some(Resumed {
                    cmd: Some(cmd),
                    lossy: false,
                    merge_launch_guard: None,
                    launch_messages: vec![],
                }));
            }
        }
    }

    let supersede = if active.is_empty() {
        Vec::new()
    } else if args.force {
        ui::warning(&format!(
            "replacing {} active runtime claim(s) because --force was given",
            active.len()
        ));
        active
    } else {
        let existing = &active[0];
        match claim_activity(repo, &committed_log, existing)? {
            ClaimActivity::Untouched => active,
            ClaimActivity::Appended => {
                ui::error(&format!(
                    "{slug}@{branch} already has unsettled content in {} {}.",
                    existing.source,
                    link::short(&existing.session_id)
                ));
                ui::hint(&format!(
                    "settle it first: `agit commit {}`",
                    existing.session_id
                ));
                ui::hint(&format!(
                    "or preserve a separate line: `agit fork {slug}@{branch} -b <new-branch> --resume`"
                ));
                return Ok(None);
            }
            ClaimActivity::Rewritten => {
                ui::error(&format!(
                    "the active runtime session {} was rewritten inside its recorded baseline.",
                    link::short(&existing.session_id)
                ));
                ui::hint(
                    "inspect the runtime transcript and `agit status`; automatic replacement fails closed",
                );
                ui::hint(&format!(
                    "to replace it deliberately: `agit resume {slug}@{branch} --force --no-launch`"
                ));
                return Ok(None);
            }
            ClaimActivity::Unverifiable => {
                ui::error(&format!(
                    "the active runtime session {} cannot be proven untouched.",
                    link::short(&existing.session_id)
                ));
                ui::hint("inspect it with `agit status`; automatic replacement fails closed");
                ui::hint(&format!(
                    "to replace it deliberately: `agit resume {slug}@{branch} --force --no-launch`"
                ));
                return Ok(None);
            }
        }
    };

    drop(branch_guard);
    confirm_conversion(from, to_runtime)?;
    let _branch_guard = link::lock_branch(&store, slug, branch)?;
    require_resume_state(repo, branch, &head, &tracking)?;
    purpose.require_transaction(repo, branch, &head)?;
    let current = link::active_for_branch(&store, owner, agent, branch);
    if current.iter().map(Link::instance).collect::<Vec<_>>()
        != supersede.iter().map(Link::instance).collect::<Vec<_>>()
    {
        if let Some(resumed) = reuse_prepared(&current) {
            return Ok(Some(resumed));
        }
        anyhow::bail!(
            "the active runtime claim changed while preparing to resume; retry the command"
        );
    }
    if matches!(purpose, ResumePurpose::Merge(_)) {
        require_merge_claims(repo, &store, slug, branch, &head, false)?;
    }
    materialize_memory(repo, branch, slug, to_runtime, &cwd);

    // ── Slow path: materialize from the head VIEW, mint a new id ──
    materialize_and_resume(
        repo,
        slug,
        branch,
        &head,
        &snap,
        from,
        to_runtime,
        args,
        &cwd,
        prompt,
        system_prompt.as_deref(),
        &committed_log,
        &store,
        supersede,
        purpose,
    )
    .map(Some)
}

#[derive(Debug, PartialEq, Eq)]
struct ResumeTracking {
    reference: Option<String>,
    oid: Option<String>,
}

impl ResumeTracking {
    fn read(repo: &Repo, branch: &str) -> crate::Result<Self> {
        let selected = format!("refs/heads/{branch}");
        let (status, output, error) = repo.git_status_local(&[
            "for-each-ref",
            "--count=1",
            "--sort=refname",
            "--format=%(refname)%09%(upstream)%00",
            &selected,
        ])?;
        anyhow::ensure!(
            status == Some(0) && error.is_empty(),
            "cannot inspect tracking for {selected}: {error}"
        );
        let output = output
            .strip_suffix('\0')
            .ok_or_else(|| anyhow::anyhow!("incomplete tracking record for {selected}"))?;
        let (found, upstream) = output.split_once('\t').unwrap_or((output, ""));
        anyhow::ensure!(
            found == selected,
            "the selected branch changed while reading tracking"
        );
        if upstream.is_empty() {
            for field in ["remote", "merge"] {
                let (status, _, error) = repo.git_status_local(&[
                    "config",
                    "--get",
                    &format!("branch.{branch}.{field}"),
                ])?;
                anyhow::ensure!(
                    status == Some(1) && error.is_empty(),
                    "cannot resolve the configured tracking branch for {selected}; inspect its Git tracking configuration: {error}"
                );
            }
            return Ok(Self {
                reference: None,
                oid: None,
            });
        }
        let (status, output, error) = repo.git_status_local(&[
            "for-each-ref",
            "--count=1",
            "--sort=refname",
            "--format=%(refname)%09%(objectname)",
            upstream,
        ])?;
        anyhow::ensure!(
            status == Some(0) && error.is_empty(),
            "cannot inspect known tracking ref {upstream}: {error}"
        );
        let oid = if let Some((found, oid)) = output.split_once('\t')
            && found == upstream
        {
            let (status, kind, error) = repo.git_status_local(&["cat-file", "-t", oid])?;
            anyhow::ensure!(
                status == Some(0) && error.is_empty() && kind == "commit",
                "known tracking ref {upstream} does not name a locally available commit: {error}"
            );
            Some(oid.to_owned())
        } else {
            let (status, _, error) =
                repo.git_status_local(&["symbolic-ref", "--quiet", upstream])?;
            anyhow::ensure!(
                status == Some(1) && error.is_empty(),
                "known tracking ref {upstream} cannot resolve to a commit: {error}"
            );
            None
        };
        Ok(Self {
            reference: Some(upstream.to_owned()),
            oid,
        })
    }

    fn is_integrated(
        &self,
        repo: &Repo,
        slug: &str,
        branch: &str,
        head: &str,
    ) -> crate::Result<bool> {
        let Some(oid) = &self.oid else {
            return Ok(true);
        };
        let (status, _, error) =
            repo.git_status_local(&["merge-base", "--is-ancestor", oid, head])?;
        match status {
            Some(0) => Ok(true),
            Some(1) if error.is_empty() => {
                ui::error(&format!(
                    "{slug}@{branch} has known tracking commits that are not integrated."
                ));
                let source = recovery_arg(&format!("{slug}@{}", meta::id_from_sha(oid)));
                let target = recovery_arg(&format!("{slug}@{branch}"));
                let shell = if cfg!(windows) { " in PowerShell" } else { "" };
                ui::hint(&format!(
                    "reconcile the known tracking history{shell}: `agit merge {source} --into {target} --manual`"
                ));
                Ok(false)
            }
            _ => {
                anyhow::bail!("cannot compare known tracking history for {slug}@{branch}: {error}")
            }
        }
    }
}

fn recovery_arg(value: &str) -> String {
    #[cfg(windows)]
    {
        powershell_recovery_arg(value)
    }
    #[cfg(not(windows))]
    {
        ui::session::shell_arg(value)
    }
}

fn require_resume_state(
    repo: &Repo,
    branch: &str,
    head: &str,
    tracking: &ResumeTracking,
) -> crate::Result<()> {
    require_resume_head(repo, branch, head)?;
    // A prompt can outlive either the configured tracking identity or its locally known tip.
    anyhow::ensure!(
        ResumeTracking::read(repo, branch)? == *tracking,
        "known tracking state changed while preparing to resume; retry the command"
    );
    Ok(())
}

fn require_resume_head(repo: &Repo, branch: &str, head: &str) -> crate::Result<()> {
    anyhow::ensure!(
        repo.git(&["rev-parse", &format!("refs/heads/{branch}")])?
            .trim()
            == head
            && !super::branch::is_sealed(repo, branch),
        "the branch changed while preparing to resume; retry the command"
    );
    Ok(())
}

fn confirm_conversion(from: &str, to: &str) -> crate::Result<()> {
    if adapter::is_lossy_conversion(from, to) {
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

    Ok(())
}

fn materialize_memory(repo: &Repo, branch: &str, slug: &str, runtime: &str, cwd: &Path) {
    match super::memory::materialize(repo, branch, slug, runtime, cwd) {
        Ok(Some(report)) => super::memory::report_materialize(&report),
        Ok(None) => {}
        Err(error) => ui::warning(&format!("memory was not materialized: {error:#}")),
    }
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

/// The validated LOG must name the same immutable bytes as VIEW before native reuse.
/// History can be shallow or grafted; absence of a visible rewrite cannot prove this equality.
fn head_view_matches_log(repo: &Repo, head: &str, snapshot: &meta::Meta) -> crate::Result<bool> {
    if snapshot.session.is_empty() {
        return Ok(true);
    }
    let paths = match snapshot.layout {
        meta::LayoutVersion::V0 => [meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE],
        meta::LayoutVersion::V1 => [meta::LOG_FILE, meta::VIEW_FILE],
    };
    let mut objects = Vec::new();
    repo.git_cat_file_batch_check(
        paths.iter().map(|path| format!("{head}:{path}")).collect(),
        |oid, kind, _| {
            objects.push((kind == "blob").then(|| oid.to_owned()));
            Ok(())
        },
    )?;
    Ok(matches!(objects.as_slice(), [Some(log), Some(view)] if log == view))
}

/// Whether session history can make the native transcript differ from the saved VIEW.
///
/// Native reuse proves LOG continuity, which does not prove VIEW equality. VIEW surgery can
/// remove context from VIEW, and archives can append LOG-only evidence. Once either operation
/// appears in session history, continuing from saved VIEW requires materialization.
/// File-line merges only reconcile shared files and do not impose this requirement.
fn history_requires_view_materialization(repo: &Repo, head: &str) -> crate::Result<bool> {
    Ok(first_parent_metas(repo, head)?.iter().any(|m| {
        m.is_session_line()
            && matches!(
                m.kind,
                meta::Kind::View | meta::Kind::Merge | meta::Kind::Archive
            )
    }))
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
    let mut hits = link::active_for_branch(&store, owner, name, branch);
    if hits.len() == 1 { hits.pop() } else { None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimActivity {
    Untouched,
    Appended,
    Rewritten,
    Unverifiable,
}

/// Merge replaces local writers only after their complete evidence is represented in history.
/// Hook success alone is insufficient because hooks deliberately suppress settlement failures.
pub(crate) fn require_merge_claims(
    repo: &Repo,
    store: &Store,
    slug: &str,
    branch: &str,
    head: &str,
    allow_settlement: bool,
) -> crate::Result<()> {
    let (owner, agent) = slug.split_once('/').unwrap_or(("", slug));
    let active = link::active_for_branch(store, owner, agent, branch);
    if active.is_empty() {
        return Ok(());
    }
    let snapshot = meta::read_at_ref_result(repo, head)?
        .ok_or_else(|| anyhow::anyhow!("the merge target has no session metadata"))?;
    let committed = committed_log(repo, head, &snapshot)?;
    for claim in active {
        require_merge_claim(repo, &committed, &claim, slug, branch, allow_settlement)?;
    }
    Ok(())
}

fn require_merge_claim(
    repo: &Repo,
    committed: &str,
    claim: &Link,
    slug: &str,
    branch: &str,
    allow_settlement: bool,
) -> crate::Result<()> {
    let bytes = claim.read_bytes().map_err(|_| anyhow::anyhow!(
        "cannot read the active runtime transcript; merge cannot prove {slug}@{branch} is settled"
    ))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!(
        "the active runtime transcript contains invalid text; inspect it before merging {slug}@{branch}"
    ))?;
    anyhow::ensure!(
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()),
        "the active runtime transcript contains an incomplete or malformed record; finish or recover it before merging {slug}@{branch}"
    );
    let activity = if claim.baseline_bytes.is_some() {
        match link::materialization_activity_with_bytes(claim, &bytes) {
            link::MaterializationActivity::Untouched => ClaimActivity::Untouched,
            link::MaterializationActivity::Appended => ClaimActivity::Appended,
            link::MaterializationActivity::Rewritten => ClaimActivity::Rewritten,
            link::MaterializationActivity::Unverifiable => ClaimActivity::Unverifiable,
        }
    } else {
        native_merge_claim_activity(repo, committed, text)?
    };
    if activity == ClaimActivity::Untouched {
        return Ok(());
    }
    if activity == ClaimActivity::Appended && allow_settlement {
        let region = match claim.baseline_bytes {
            Some(baseline) => usize::try_from(baseline)
                .ok()
                .and_then(|offset| text.get(offset..)),
            None => Some(text),
        };
        if let Some(region) = region
            && !super::commit::has_in_flight_turn(&claim.source, region)?
        {
            return Ok(());
        }
    }
    let target = recovery_arg(&format!("{slug}@{branch}"));
    anyhow::bail!(
        "the active {} session {} has unsettled or unverifiable content; finish the turn and run `agit commit {target}`, or preserve it on another branch before merging",
        claim.source,
        link::short(&claim.session_id)
    )
}

fn native_merge_claim_activity(
    repo: &Repo,
    committed: &str,
    live: &str,
) -> crate::Result<ClaimActivity> {
    if transcript::continuity(committed, live) == transcript::Continuity::Noop {
        return Ok(ClaimActivity::Untouched);
    }
    let (committed, live) = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?
        .hydrate_pair_readonly(committed, live)?;
    if committed.unresolved > 0 || live.unresolved > 0 {
        return Ok(ClaimActivity::Unverifiable);
    }
    Ok(
        match transcript::continuity_of_content(&committed.text, &live.text) {
            transcript::Continuity::Noop => ClaimActivity::Untouched,
            transcript::Continuity::Append => ClaimActivity::Appended,
            transcript::Continuity::Diverged => ClaimActivity::Rewritten,
        },
    )
}

/// Classify whether replacing one active branch claim can lose runtime content.
///
/// Materialized instances carry their own byte baseline. Native instances are compared against
/// the committed LOG after applying the repository's existing secret projection, the same
/// comparison used by the zero-copy path.
fn claim_activity(repo: &Repo, committed_log: &str, link: &Link) -> crate::Result<ClaimActivity> {
    if link.baseline_bytes.is_some() {
        return Ok(match link::materialization_activity(link) {
            link::MaterializationActivity::Untouched => ClaimActivity::Untouched,
            link::MaterializationActivity::Appended => ClaimActivity::Appended,
            link::MaterializationActivity::Rewritten => ClaimActivity::Rewritten,
            link::MaterializationActivity::Unverifiable => ClaimActivity::Unverifiable,
        });
    }
    let Ok(live) = link.read() else {
        return Ok(ClaimActivity::Unverifiable);
    };
    let projected = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?
        .protect_existing_jsonl(&live)?;
    Ok(
        match transcript::continuity(committed_log, &projected.text) {
            transcript::Continuity::Noop => ClaimActivity::Untouched,
            transcript::Continuity::Append => ClaimActivity::Appended,
            transcript::Continuity::Diverged => ClaimActivity::Rewritten,
        },
    )
}

fn report_multiple_active(slug: &str, branch: &str, links: &[Link]) {
    ui::error(&format!(
        "{slug}@{branch} has {} active session links — can’t pick for you:",
        links.len()
    ));
    for link in links.iter().take(8) {
        println!(
            "  {:12} {}  agit commit {}",
            link.source,
            link::short(&link.session_id),
            link.session_id
        );
    }
    ui::hint("choose one session id above; `agit status` lists every stored link");
    ui::hint(&format!(
        "to replace all active claims deliberately: `agit resume {slug}@{branch} --force --no-launch`"
    ));
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
        "codex" => adapter::codex::resume_command(sid, cwd),
        "opencode" if prompt.is_none() && system_prompt.is_none() => {
            format!("opencode --session {sid}")
        }
        _ => return None,
    };
    if runtime == "codex"
        && let Some(provider) = adapter::codex_provider::resume_override(sid, cwd)
    {
        let value = serde_json::to_string(&provider).ok()?;
        inner.push_str(" -c ");
        inner.push_str(&shell_quote(&format!("model_provider={value}")));
    }
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

/// Reconstruct the next step for an already materialized runtime instance.
///
/// CLI runtimes return a launch command. Claude Desktop's official handoff is intentionally only
/// printed, just as it is after the initial materialization; the guaranteed CLI fallback still
/// carries `AGIT_SESSION`.
fn prepared_resume(
    runtime: &str,
    sid: &str,
    cwd: &Path,
    slug: &str,
    branch: &str,
    prompt: Option<&str>,
    system_prompt: Option<&str>,
) -> Option<Resumed> {
    if let Some(cmd) = native_resume_cmd(runtime, sid, cwd, slug, branch, prompt, system_prompt) {
        return Some(Resumed {
            cmd: Some(cmd),
            lossy: false,
            merge_launch_guard: None,
            launch_messages: vec![],
        });
    }
    if runtime == "opencode" {
        if system_prompt.is_some() {
            ui::warning(
                "opencode cannot receive a system environment notice on resume; continuing without injection",
            );
        }
        if let Some(prompt) = prompt {
            ui::warning(
                "opencode can’t take an opening prompt on resume — paste this in as the first message:",
            );
            println!("{prompt}");
        }
        return Some(Resumed {
            cmd: Some(wrap_launch(
                &format!("opencode --session {sid}"),
                cwd,
                slug,
                branch,
            )),
            lossy: false,
            merge_launch_guard: None,
            launch_messages: vec![],
        });
    }
    if runtime != "claude-desktop" {
        return None;
    }
    if system_prompt.is_some() {
        ui::warning(
            "the desktop handoff deep link cannot carry the environment notice; use the CLI fallback below to resume with it",
        );
    }
    let trigger = format!("open 'claude://resume?session={sid}'");
    let plain_fallback = wrap_launch(&format!("claude --resume {sid}"), cwd, slug, branch);
    let fallback = handoff_fallback(
        &plain_fallback,
        sid,
        cwd,
        slug,
        branch,
        prompt,
        system_prompt,
    );
    println!("  {}", ui::accent(&trigger));
    println!(
        "  {}",
        ui::dim(&format!("the guaranteed way if handoff fails: {fallback}"))
    );
    Some(Resumed {
        cmd: None,
        lossy: false,
        merge_launch_guard: None,
        launch_messages: vec![],
    })
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

/// Hold the link lock only when this runtime still owns the branch claim being replaced.
///
/// The branch lock serializes operations on one branch, but importing this same runtime onto a
/// different branch takes a different branch lock. Re-reading under the per-link lock prevents a
/// supersession from writing an old routing snapshot over that newer destination.
fn lock_active_branch_claim(
    store: &Store,
    expected: &Link,
    slug: &str,
    branch: &str,
) -> crate::Result<Option<(std::fs::File, Link)>> {
    let guard = link::lock(store, &expected.source, &expected.session_id)?;
    let Some(current) = link::get(store, &expected.source, &expected.session_id) else {
        return Ok(None);
    };
    let (owner, agent) = slug.split_once('/').unwrap_or(("", slug));
    let still_claims_branch = link::claims_branch(&current, owner, agent, branch);
    Ok(still_claims_branch.then_some((guard, current)))
}

/// Materializes the VIEW into the runtime and prepares the resume command.
// The parameter list is long because loading needs exactly these facts; packing them into a
// struct only adds a layer of indirection.
#[allow(clippy::too_many_arguments)]
fn materialize_and_resume(
    repo: &Repo,
    slug: &str,
    branch: &str,
    head: &str,
    _snap: &meta::Meta,
    from: &str,
    to: &str,
    args: &Args,
    cwd: &Path,
    prompt: Option<&str>,
    system_prompt: Option<&str>,
    committed_log: &str,
    store: &Store,
    supersede: Vec<Link>,
    purpose: ResumePurpose<'_>,
) -> crate::Result<Resumed> {
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
    let view_env = crate::domain::storage::materialize_at(repo.root(), head, meta::VIEW_FILE)
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
        match crate::domain::storage::materialize_head_at(repo.root(), head, meta::LOG_FILE) {
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
    let hydrated = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?
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

    let lossy = adapter::is_lossy_conversion(from, to);

    let mut locked_supersede = Vec::with_capacity(supersede.len());
    for previous in &supersede {
        if let Some((guard, current)) = lock_active_branch_claim(store, previous, slug, branch)? {
            if matches!(purpose, ResumePurpose::Merge(_)) {
                require_merge_claim(repo, committed_log, &current, slug, branch, false)?;
            }
            locked_supersede.push((guard, current));
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
    lk.materialized_from = Some(head.to_string());
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
    let defer = matches!(purpose, ResumePurpose::Merge(_));
    let mut launch_messages = vec![];
    let mut merge_launch_guard = if defer {
        let control = crate::domain::mergetx::ControlGuard::acquire(repo.root())?;
        if let Err(error) = purpose
            .require_transaction(repo, branch, head)
            .and_then(|()| require_resume_head(repo, branch, head))
        {
            drop(control);
            ui::warning(&format!(
                "the prepared {} session {} was left unclaimed; `agit status --check-missing` can find it",
                lk.source,
                link::short(&lk.session_id)
            ));
            return Err(error);
        }
        Some(control)
    } else {
        None
    };
    for (_, current) in &locked_supersede {
        if matches!(purpose, ResumePurpose::Merge(_))
            && let Err(error) =
                require_merge_claim(repo, committed_log, current, slug, branch, false)
        {
            drop(merge_launch_guard.take());
            ui::warning(&format!(
                "the prepared {} session {} was left unclaimed; `agit status --check-missing` can find it",
                lk.source,
                link::short(&lk.session_id)
            ));
            return Err(error);
        }
        if !defer && !args.force {
            let activity = claim_activity(repo, committed_log, current)?;
            if activity != ClaimActivity::Untouched {
                ui::warning(&format!(
                    "the prepared {} session {} was left unclaimed; `agit status --check-missing` can find it",
                    lk.source,
                    link::short(&lk.session_id)
                ));
            }
            match activity {
                ClaimActivity::Untouched => {}
                ClaimActivity::Appended => {
                    ui::error(&format!(
                        "the active runtime session {} gained content while its replacement was being prepared.",
                        link::short(&current.session_id)
                    ));
                    ui::hint(&format!(
                        "its claim remains active; settle it with `agit commit {}` and retry",
                        current.session_id
                    ));
                    anyhow::bail!("active runtime changed during materialization");
                }
                ClaimActivity::Rewritten | ClaimActivity::Unverifiable => {
                    ui::error(&format!(
                        "the active runtime session {} can no longer be proven untouched.",
                        link::short(&current.session_id)
                    ));
                    ui::hint("its claim remains active; inspect the transcript and `agit status`");
                    ui::hint(&format!(
                        "to replace it deliberately: `agit resume {slug}@{branch} --force --no-launch`"
                    ));
                    anyhow::bail!("active runtime changed during materialization");
                }
            }
        }
    }
    let successor = lk.instance();
    // Publish the successor first. A crash during the following historical-link updates can then
    // leave an explicit multi-active refusal, but never zero active claims and an orphaned runtime
    // session. The branch lock keeps other prepare mutations out; a commit that resolved the old
    // link rechecks it under this same lock before writing.
    link::write(store, &lk)?;
    for (_guard, mut previous) in locked_supersede {
        previous.superseded_by = Some(successor.clone());
        link::write(store, &previous)?;
        launch_line(
            &mut launch_messages,
            defer,
            ui::dim(&format!(
                "  superseded {} {} on {slug} @ {branch}",
                previous.source,
                link::short(&previous.session_id)
            ))
            .to_string(),
        );
    }
    launch_line(
        &mut launch_messages,
        defer,
        format!(
            "  {} materialized VIEW → {} {}",
            ui::ok(ui::theme::symbols().check),
            ui::dim(&ui::tilde(&installed.path)),
            if lossy {
                ui::warn_text("(lossy)").to_string()
            } else {
                String::new()
            }
        ),
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
                        launch_warning(
                            &mut launch_messages,
                            defer,
                            &format!(
                                "{to} cannot receive a system environment notice on resume; continuing without injection"
                            ),
                        );
                    }
                    match prompt {
                        Some(p) => {
                            // This runtime has no "resume carrying a prompt" form. The
                            // instruction must not be dropped over that — print it for the
                            // person at the terminal to paste in, rather than leaving the agent
                            // idle once it starts.
                            launch_warning(
                                &mut launch_messages,
                                defer,
                                &format!(
                                    "{to} can’t take an opening prompt on resume — paste this in as the first message:"
                                ),
                            );
                            launch_line(&mut launch_messages, defer, p.to_owned());
                            Some(inject_session_env(c, slug, branch))
                        }
                        None => Some(inject_session_env(c, slug, branch)),
                    }
                }
            }
        }
        adapter::Next::HandOff { trigger, fallback } => {
            if system_prompt.is_some() {
                launch_warning(
                    &mut launch_messages,
                    defer,
                    "the desktop handoff deep link cannot carry the environment notice; use the CLI fallback below to resume with it",
                );
            }
            let fallback =
                handoff_fallback(fallback, &sid, cwd, slug, branch, prompt, system_prompt);
            launch_line(
                &mut launch_messages,
                defer,
                format!("  {}", ui::accent(trigger)),
            );
            launch_line(
                &mut launch_messages,
                defer,
                format!(
                    "  {}",
                    ui::dim(&format!("the guaranteed way if handoff fails: {fallback}"))
                ),
            );
            None
        }
    };
    Ok(Resumed {
        cmd,
        lossy,
        merge_launch_guard,
        launch_messages,
    })
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
    use crate::domain::link::{self, Link};
    use crate::domain::meta;
    use crate::domain::repo::Repo;
    use crate::domain::store::Store;
    use crate::domain::transcript;
    use std::path::Path;

    #[test]
    fn same_repo_matches_explicit_ssh_home_spellings() {
        let login_home = [
            "alice@example.invalid:repo.git",
            "alice@example.invalid:~/repo.git",
            "alice@example.invalid:/~/repo.git",
            "ssh://alice@example.invalid/~/repo.git",
        ];
        for recorded in login_home {
            for current in login_home {
                assert!(super::same_repo_as(&format!("{recorded}@1839e61"), current));
            }
        }
        for user in ["alice", "git"] {
            let named_home = [
                format!("{user}@example.invalid:~bob/repo.git"),
                format!("{user}@example.invalid:/~bob/repo.git"),
                format!("ssh://{user}@example.invalid/~bob/repo.git"),
            ];
            for recorded in &named_home {
                for current in &named_home {
                    assert!(super::same_repo_as(&format!("{recorded}@1839e61"), current));
                }
                for other in [
                    "https://example.invalid/~bob/repo.git".to_owned(),
                    format!("ssh://{user}@example.invalid/~bob/repo"),
                    format!("ssh://{user}@example.invalid/~Bob/repo.git"),
                    format!("ssh://{user}@example.invalid/~charlie/repo.git"),
                    format!("ssh://{user}@example.invalid//~bob/repo.git"),
                    format!("{user}@example.invalid://~bob/repo.git"),
                ] {
                    assert!(!super::same_repo_as(&format!("{recorded}@1839e61"), &other));
                }
            }
        }
    }

    #[test]
    fn same_repo_keeps_named_homes_separate_from_nested_login_paths() {
        for user in ["alice", "git"] {
            let home_root = [
                format!("{user}@example.invalid:~"),
                format!("{user}@example.invalid:~/"),
                format!("{user}@example.invalid:/~"),
                format!("{user}@example.invalid:/~/"),
                format!("ssh://{user}@example.invalid/~"),
                format!("ssh://{user}@example.invalid/~/"),
            ];
            for recorded in &home_root {
                for current in &home_root {
                    assert!(super::same_repo_as(&format!("{recorded}@1839e61"), current));
                }
            }
            let nested_login_home = [
                format!("{user}@example.invalid:~/~bob/repo.git"),
                format!("{user}@example.invalid:/~/~bob/repo.git"),
                format!("ssh://{user}@example.invalid/~/~bob/repo.git"),
            ];
            let named_home = [
                format!("{user}@example.invalid:~bob/repo.git"),
                format!("{user}@example.invalid:/~bob/repo.git"),
                format!("ssh://{user}@example.invalid/~bob/repo.git"),
            ];
            for recorded in &nested_login_home {
                for current in &nested_login_home {
                    assert!(super::same_repo_as(&format!("{recorded}@1839e61"), current));
                }
                for named in &named_home {
                    assert!(!super::same_repo_as(&format!("{recorded}@1839e61"), named));
                    assert!(!super::same_repo_as(&format!("{named}@1839e61"), recorded));
                }
            }
        }
    }

    #[test]
    fn same_repo_matches_transport_spellings_in_both_directions() {
        let spellings = [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app",
            "https://GitHub.com:443/acme/app.git/",
            "ssh://git@github.com:22/acme/app.git",
            "git://github.com:9418/acme/app.git",
        ];
        for recorded in spellings {
            for current in spellings {
                assert!(
                    super::same_repo_as(&format!("{recorded}@1839e61"), current),
                    "{recorded} and {current} name the same remote"
                );
            }
        }
        assert!(super::same_repo_as(
            "git@github.com:acme/app@next.git@1839e61",
            "https://github.com/acme/app@next.git"
        ));
        assert!(super::same_repo_as(
            "ssh://git@[2001:db8::1]/acme/app.git@1839e61",
            "https://[2001:db8::1]/acme/app"
        ));
        assert!(super::same_repo_as(
            "git@[2001:db8::1]:acme/app.git@1839e61",
            "ssh://git@[2001:db8::1]/acme/app.git"
        ));
        assert!(super::same_repo_as(
            "git@example.com:/srv/app.git@1839e61",
            "ssh://git@example.com/srv/app.git"
        ));
        assert!(super::same_repo_as(
            "git@example.com:/srv/repo[copy]:part.git@1839e61",
            "ssh://git@example.com/srv/repo[copy]:part.git"
        ));
        assert!(super::same_repo_as(
            "git@example.com:/srv/repo::part.git@1839e61",
            "ssh://git@example.com/srv/repo::part.git"
        ));
    }

    #[test]
    fn same_repo_preserves_exact_root_origins() {
        for origin in [
            "https://example.invalid",
            "https://example.invalid/",
            "http://example.invalid:8177",
        ] {
            assert!(super::same_repo_as(&format!("{origin}@1839e61"), origin));
        }
        for (code, origin) in [("@1839e61", ""), ("@1839e61", "  ")] {
            assert!(!super::same_repo_as(code, origin));
        }
    }

    #[test]
    fn same_repo_preserves_scoped_address_zones() {
        let spellings = [
            "git@[fe80::ABCD%EnA]:acme/app.git",
            "git@[FE80::abcd%EnA]:acme/app.git",
            "ssh://git@[fe80::ABCD%EnA]/acme/app.git",
            "ssh://git@[FE80::abcd%EnA]:22/acme/app.git",
            "http://[fe80::ABCD%EnA]/acme/app.git",
            "http://[FE80::abcd%EnA]:80/acme/app.git",
            "https://[FE80::abcd%EnA]:443/acme/app.git",
            "git://[FE80::abcd%EnA]:9418/acme/app.git",
        ];
        for recorded in spellings {
            for current in spellings {
                assert!(
                    super::same_repo_as(&format!("{recorded}@1839e61"), current),
                    "{recorded} and {current} name the same remote"
                );
                let other_zone = current.replace("%EnA", "%ena");
                assert!(
                    !super::same_repo_as(&format!("{recorded}@1839e61"), &other_zone),
                    "{recorded} and {other_zone} use different interfaces"
                );
            }
        }
        for recorded in [
            "ssh://git@[fe80::ABCD%EnA]:2222/acme/app.git",
            "ssh://git@[FE80::abcd%EnA]:2222/acme/app.git",
        ] {
            assert!(super::same_repo_as(
                &format!("{recorded}@1839e61"),
                "ssh://git@[fe80::abcd%EnA]:2222/acme/app"
            ));
            for other in [
                "ssh://git@[fe80::abcd%ena]:2222/acme/app.git",
                "ssh://git@[fe80::abcd%EnA]:2223/acme/app.git",
                "ssh://git@[fe80::abcd%EnA]:22/acme/app.git",
                "ssh://git@[fe80::abcd%EnA]:2222/acme/App.git",
                "ssh://git@[fe80::abcd%EnA]:2222/acme/app/child.git",
                "ssh://git@[fe80::abcd%EnA]:2222//acme/app.git",
            ] {
                assert!(!super::same_repo_as(&format!("{recorded}@1839e61"), other));
            }
        }
        assert!(super::same_repo_as(
            "https://[fe80::ABCD%25EnA]/acme/app.git@1839e61",
            "https://[FE80::abcd%25EnA]/acme/app.git"
        ));
        assert!(!super::same_repo_as(
            "https://[fe80::abcd%25EnA]/acme/app.git@1839e61",
            "https://[fe80::abcd%25ena]/acme/app.git"
        ));
    }

    #[test]
    fn same_repo_preserves_scoped_ssh_home_namespaces() {
        for user in ["alice", "git"] {
            for path in ["repo.git", "~bob/repo.git", "~/~bob/repo.git"] {
                let recorded = format!("{user}@[fe80::ABCD%EnA]:{path}");
                let url_path = if path.starts_with('~') {
                    path.to_owned()
                } else {
                    format!("~/{path}")
                };
                for current in [
                    format!("{user}@[FE80::abcd%EnA]:{path}"),
                    format!("ssh://{user}@[fe80::ABCD%EnA]/{url_path}"),
                    format!("ssh://{user}@[FE80::abcd%EnA]:22/{url_path}"),
                ] {
                    assert!(super::same_repo_as(
                        &format!("{recorded}@1839e61"),
                        &current
                    ));
                    let other_zone = current.replace("%EnA", "%ena");
                    assert!(!super::same_repo_as(
                        &format!("{recorded}@1839e61"),
                        &other_zone
                    ));
                }
            }
        }
        let recorded = "alice@[fe80::ABCD%EnA]:~bob/repo.git@1839e61";
        for other in [
            "git@[fe80::abcd%EnA]:~bob/repo.git",
            "alice@[fe80::abcd%EnA]:~Bob/repo.git",
            "alice@[fe80::abcd%EnA]:~/~bob/repo.git",
            "alice@[fe80::abcd%EnA]:~bob/repo",
            "https://[fe80::abcd%EnA]/~bob/repo.git",
        ] {
            assert!(!super::same_repo_as(recorded, other));
        }
    }

    #[test]
    fn same_repo_preserves_repository_and_service_boundaries() {
        let recorded = "git@github.com:acme/app.git@1839e61";
        for other in [
            "https://github.com/acme/application",
            "https://github.com/acme/app/child",
            "https://github.com/Acme/app.git",
            "https://github.com/acme/App.git",
            "https://gitlab.com/acme/app.git",
            "https://github.com:8443/acme/app.git",
            "ssh://git@github.com:2222/acme/app.git",
            "",
        ] {
            assert!(!super::same_repo_as(recorded, other), "{other}");
        }
        assert!(!super::same_repo_as(
            "github.com/acme/application@1839e61",
            "github.com/acme/app"
        ));
        assert!(super::same_repo_as(
            "https://github.com:8443/acme/app.git@1839e61",
            "https://github.com:8443/acme/app"
        ));
        for other in [
            "bob@example.com:app.git",
            "alice@example.com:app",
            "https://example.com/app.git",
            "ssh://alice@example.com/app.git",
            "alice@example.com:/app.git",
        ] {
            assert!(!super::same_repo_as(
                "alice@example.com:app.git@1839e61",
                other
            ));
        }
        assert!(super::same_repo_as(
            "alice@example.com:app.git@1839e61",
            "ssh://alice@example.com/~/app.git"
        ));
    }

    #[test]
    fn same_repo_does_not_reinterpret_local_paths_or_malformed_anchors() {
        for origin in ["/tmp/project.git", "../project.git", "C:\\project.git"] {
            assert!(super::same_repo_as(&format!("{origin}@1839e61"), origin));
            assert!(!super::same_repo_as(
                &format!("{origin}@1839e61"),
                origin.strip_suffix(".git").unwrap()
            ));
        }
        for code in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app.git@",
            "https://github.com/acme/app.git@HEAD",
            "https://github.com/acme/app.git@abc",
            "@1839e61",
        ] {
            assert!(!super::same_repo_as(
                code,
                "https://github.com/acme/app.git"
            ));
        }
        assert!(super::same_repo_as(
            "helper::opaque.git@1839e61",
            "helper::opaque.git"
        ));
        assert!(!super::same_repo_as(
            "helper::opaque.git@1839e61",
            "helper::opaque"
        ));
        for (code, other) in [
            (
                "helper::git@[::1]:/app.git@1839e61",
                "helper::git@[::1]:/app",
            ),
            (
                "helper::git@[ABCD::1]:/app.git@1839e61",
                "helper::git@[abcd::1]:/app.git",
            ),
        ] {
            assert!(!super::same_repo_as(code, other));
        }
    }

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

    #[test]
    fn resume_rejects_tracking_changes_across_confirmation() {
        for change in ["configured", "created", "advanced", "removed", "remapped"] {
            let (_dir, repo, head) = claimed_but_never_settled();
            let branch = repo.current_branch().unwrap();
            repo.git(&[
                "remote",
                "add",
                "mirror",
                "https://example.invalid/fixture.git",
            ])
            .unwrap();
            if change != "configured" {
                repo.git(&["config", &format!("branch.{branch}.remote"), "mirror"])
                    .unwrap();
                repo.git(&[
                    "config",
                    &format!("branch.{branch}.merge"),
                    "refs/heads/topic",
                ])
                .unwrap();
            }
            if !matches!(change, "configured" | "created") {
                repo.git(&["update-ref", "refs/remotes/mirror/topic", &head])
                    .unwrap();
            }
            let tracking = super::ResumeTracking::read(&repo, &branch).unwrap();
            super::require_resume_state(&repo, &branch, &head, &tracking).unwrap();
            match change {
                "configured" => {
                    repo.git(&["config", &format!("branch.{branch}.remote"), "mirror"])
                        .unwrap();
                    repo.git(&[
                        "config",
                        &format!("branch.{branch}.merge"),
                        "refs/heads/topic",
                    ])
                    .unwrap();
                }
                "created" => {
                    repo.git(&["update-ref", "refs/remotes/mirror/topic", &head])
                        .unwrap();
                }
                "advanced" => {
                    let tree = repo
                        .git(&["rev-parse", &format!("{head}^{{tree}}")])
                        .unwrap();
                    let advanced = repo
                        .git(&["commit-tree", &tree, "-p", &head, "-m", "tracking advance"])
                        .unwrap();
                    repo.git(&["update-ref", "refs/remotes/mirror/topic", &advanced])
                        .unwrap();
                }
                "removed" => {
                    repo.git(&["update-ref", "-d", "refs/remotes/mirror/topic"])
                        .unwrap();
                }
                "remapped" => {
                    repo.git(&["update-ref", "refs/remotes/mirror/other", &head])
                        .unwrap();
                    repo.git(&[
                        "config",
                        &format!("branch.{branch}.merge"),
                        "refs/heads/other",
                    ])
                    .unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(
                repo.git(&["rev-parse", &format!("refs/heads/{branch}")])
                    .unwrap(),
                head
            );
            assert!(
                super::require_resume_state(&repo, &branch, &head, &tracking).is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn merge_agent_preparation_requires_the_open_frozen_transaction() {
        use crate::domain::mergetx::{self, Tx};

        let (_dir, repo, head) = claimed_but_never_settled();
        let branch = repo.current_branch().unwrap();
        let expected = Tx {
            generation: Some("synthetic-generation".into()),
            target: branch.clone(),
            source: "source".into(),
            source_repo: Some("me/fixture".into()),
            source_branch: Some("source".into()),
            base: head.clone(),
            target_head: head.clone(),
            source_head: head.clone(),
            picked: vec![],
            summary: None,
        };
        let purpose = super::ResumePurpose::Merge(&expected);
        assert!(purpose.require_transaction(&repo, &branch, &head).is_err());
        mergetx::lock(repo.root(), &expected).unwrap();
        purpose.require_transaction(&repo, &branch, &head).unwrap();
        assert!(purpose.require_transaction(&repo, "other", &head).is_err());
        assert!(
            purpose
                .require_transaction(&repo, &branch, "other")
                .is_err()
        );
        for field in [
            "generation",
            "target",
            "target_head",
            "source",
            "source_head",
            "source_repo",
            "source_branch",
            "base",
        ] {
            let mut changed = expected.clone();
            match field {
                "generation" => changed.generation = Some("replacement-generation".into()),
                "target" => changed.target = "other".into(),
                "target_head" => changed.target_head = "other".into(),
                "source" => changed.source = "other".into(),
                "source_head" => changed.source_head = "other".into(),
                "source_repo" => changed.source_repo = None,
                "source_branch" => changed.source_branch = None,
                "base" => changed.base = "other".into(),
                _ => unreachable!(),
            }
            mergetx::lock(repo.root(), &changed).unwrap();
            assert!(
                purpose.require_transaction(&repo, &branch, &head).is_err(),
                "{field}"
            );
        }
        let mut progress = expected.clone();
        progress.picked.push("source#1".into());
        progress.summary = Some("reconciled intent".into());
        mergetx::lock(repo.root(), &progress).unwrap();
        purpose.require_transaction(&repo, &branch, &head).unwrap();
        mergetx::unlock(repo.root()).unwrap();
        assert!(purpose.require_transaction(&repo, &branch, &head).is_err());
    }

    #[test]
    fn resume_rejects_a_head_that_advanced_while_a_prompt_was_open() {
        let (_dir, repo, head) = claimed_but_never_settled();
        let branch = repo.git(&["branch", "--show-current"]).unwrap();
        let branch = branch.trim();
        super::require_resume_head(&repo, branch, &head).unwrap();
        stack(
            &repo,
            meta::Kind::Turn,
            meta::Line::Session,
            "concurrent settlement",
        );
        assert!(super::require_resume_head(&repo, branch, &head).is_err());
    }

    /// A per-link lock must validate the routing state read after acquisition. Otherwise a
    /// materialization holding the old branch lock can overwrite an import that already moved the
    /// same runtime claim to another branch.
    #[test]
    fn a_rerouted_link_is_not_locked_for_stale_supersession() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store"));
        let mut expected = Link::new("codex", "session-a", Some(Path::new("/repo")));
        expected.owner = Some("alice".into());
        expected.agent = Some("photo".into());
        expected.branch = Some("work".into());
        expected.baseline_bytes = Some(10);
        expected.baseline_hash = Some("old-baseline".into());
        link::write(&store, &expected).unwrap();

        let mut rerouted = expected.clone();
        rerouted.owner = Some("bob".into());
        rerouted.branch = Some("recovery".into());
        rerouted.baseline_bytes = None;
        rerouted.baseline_hash = None;
        link::write(&store, &rerouted).unwrap();

        let locked =
            super::lock_active_branch_claim(&store, &expected, "alice/photo", "work").unwrap();
        assert!(locked.is_none());
        let current = link::get(&store, "codex", "session-a").unwrap();
        assert_eq!(current.owner.as_deref(), Some("bob"));
        assert_eq!(current.branch.as_deref(), Some("recovery"));
        assert_eq!(current.baseline_bytes, None);
        assert!(current.superseded_by.is_none());
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
        assert!(!super::history_requires_view_materialization(&r, &head).unwrap());

        let after = stack(&r, meta::Kind::View, meta::Line::Session, "revert");
        assert!(super::history_requires_view_materialization(&r, &after).unwrap());

        // Settling another turn afterwards leaves the surgery in the history: the verdict does
        // not flip back because the head is a turn again.
        let later = stack(&r, meta::Kind::Turn, meta::Line::Session, "turn");
        assert!(super::history_requires_view_materialization(&r, &later).unwrap());
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
        assert!(!super::history_requires_view_materialization(&r, &merged).unwrap());
        let session = stack(
            &r,
            meta::Kind::Turn,
            meta::Line::Session,
            "turn on the new session",
        );
        assert!(!super::history_requires_view_materialization(&r, &session).unwrap());
        // The same kind counts only when it lands on the session line.
        let surgery = stack(&r, meta::Kind::Merge, meta::Line::Session, "session merge");
        assert!(super::history_requires_view_materialization(&r, &surgery).unwrap());
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
        assert!(!super::history_requires_view_materialization(&r, &head).unwrap());
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
        assert!(cx.contains("codex resume ABC --cd '/w' 'merge it'"), "{cx}");
        // The workspace remains explicit when no opening prompt is supplied.
        let bare =
            super::native_resume_cmd("codex", "ABC", Path::new("/w"), "me/r", "b", None, None)
                .unwrap();
        assert!(bare.ends_with("codex resume ABC --cd '/w')"), "{bare}");
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
