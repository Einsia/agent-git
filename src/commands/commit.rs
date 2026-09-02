//! `agit commit` — settle: the content the live transcript appended since the last settlement,
//! cut at user turns into turn commits (one per turn).
//!
//! # Semantics (item by item against the PRD's "Recording" section)
//!
//! * The message is the turn's **first line of user prompt** (truncated to 72 characters).
//! * Every commit updates the VIEW on its own (recognizing a compact the harness did itself).
//! * Boundaries fall only at real user speech, leaving no dangling `tool_use`.
//! * **An in-flight trailing turn does not settle** — a turn is atomic, there is no half-turn
//!   commit. "In flight" has two forms: only a user prompt, with no reply of any kind yet; or a
//!   tool call the agent started whose output has not arrived (when the agent runs `agit commit`
//!   from inside a turn, that call is the open one). Both stay in the runtime until the next
//!   settlement, see [`in_flight_tail`].
//! * When the content is unchanged, print "nothing new since <version>" and exit 0.
//! * `--milestone` annotates the last commit with a phase summary, `--tag` also tags it,
//!   `--code` additionally makes one commit in the cwd's code repo and records the new sha as
//!   the code anchor.
//! * The `-m` form is legal only when there are no new turns and only shared files
//!   (memory/skills/AGENTS.md...) changed; it lands one file commit, and the message is required.
//! * `--from-hook` belongs to hooks: quiet, non-blocking on failure, skipped automatically during
//!   a merge transaction.
//!
//! # Two settlement modes
//!
//! * **Native continuation** (after `import` adopts it, work continues in the original harness):
//!   the live transcript must carry the committed content as its prefix (the content continuity
//!   check), with the increment at the tail. The baseline is the committed content itself.
//! * **Materialized baseline** (`run`/`resume`/`fork` installs the VIEW into a runtime, which
//!   mints a new id): the link records the byte baseline and the hash at the moment of
//!   materialization; settlement reads only the bytes appended **after** that baseline. Checking
//!   the baseline region (still verbatim, never written non-append) is doctor's job; settlement
//!   refuses outright on a hash mismatch — the provenance of those bytes is branch history, and
//!   they cannot pretend to be something else.
//!
//! # Requires sign-in, but sends no network request itself
//!
//! A commit's author fields (git user.name/email) come from the credentials and cannot be
//! backfilled, so the check runs up front instead of failing halfway through.

use super::CmdResult;
use crate::adapter::{EventKind, OpenCall, Session};
use crate::domain::link::{self, Link};
use crate::domain::mergetx;
use crate::domain::meta::{self, Completeness, Kind, Meta};
use crate::domain::repo::{self, Repo};
use crate::domain::storage;
use crate::domain::store::Store;
use crate::domain::transcript::{self, Continuity};
use crate::infra::credentials;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::Path;

/// Private handoff from the strict RC commit entry point to its supervisor.
/// The file is written only after a real transcript settlement has published
/// its branch ref; a zero exit without this result is therefore a true no-op.
pub(crate) const SUPERVISOR_RESULT_ENV: &str = "AGIT_RC_SUPERVISOR_COMMIT_RESULT";

#[cfg(test)]
type SettlementInterleaveHook = Box<dyn FnOnce(&Repo, &str)>;

#[cfg(test)]
thread_local! {
    static SETTLEMENT_INTERLEAVE_HOOK: std::cell::RefCell<Option<SettlementInterleaveHook>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn interleave_next_settlement(hook: impl FnOnce(&Repo, &str) + 'static) {
    SETTLEMENT_INTERLEAVE_HOOK.with(|slot| {
        let replaced = slot.borrow_mut().replace(Box::new(hook));
        assert!(
            replaced.is_none(),
            "a settlement interleave hook is already installed"
        );
    });
}

#[cfg(test)]
fn maybe_interleave_settlement(repo: &Repo, branch: &str) {
    let hook = SETTLEMENT_INTERLEAVE_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(repo, branch);
    }
}

#[cfg(not(test))]
fn maybe_interleave_settlement(_repo: &Repo, _branch: &str) {}

#[derive(ClapArgs)]
pub struct Args {
    /// Target `owner/repo@branch` (or legacy branch / `@`; default: context resolution).
    /// Legacy forms accepted: agent name / session id prefix.
    #[arg(value_name = "owner/repo@branch | branch | @")]
    pub target: Option<String>,

    /// Phase summary, attached to the last turn commit (with --tag it’s a milestone).
    #[arg(long, value_name = "summary")]
    pub milestone: Option<String>,

    /// Tag the last commit after settling.
    #[arg(long, value_name = "tag")]
    pub tag: Option<String>,

    /// Also run one git commit in the cwd’s code repo and record an exact-level code anchor.
    #[arg(long)]
    pub code: bool,

    /// A pure file commit (shared-file changes); message required, legal only when there are no new turns.
    #[arg(short = 'm', long = "message", value_name = "msg")]
    pub message: Option<String>,

    /// Path scope for the file commit.
    #[arg(last = true, value_name = "path")]
    pub paths: Vec<String>,

    /// For runtime hooks: quiet, non-blocking on failure, auto-skipped during merge transactions.
    #[arg(long, hide = true)]
    pub from_hook: bool,

    /// Strict automatic settlement owned by the RC supervisor. Unlike a Stop
    /// hook, failures and a disabled `commit.auto` setting remain non-zero so
    /// the supervisor cannot mistake an old HEAD for a newly settled turn.
    #[arg(long, hide = true, conflicts_with = "from_hook")]
    pub from_supervisor: bool,

    /// Compat: settle under a different repo (default: this session’s own home).
    #[arg(short = 'n', long = "name", value_name = "owner/name or name")]
    pub name: Option<String>,
}

/// What a settlement targets.
enum Target {
    /// Branch semantics: exactly which branch to advance is known.
    Branch {
        repo_dir: std::path::PathBuf,
        slug: String,
        branch: String,
        link: Link,
        via: &'static str,
    },
    /// The legacy form: a link resolved from an agent name / session id (its branch is the
    /// registered value, or the current branch).
    Legacy { link: Link, agent: String },
    /// A `-m` pure file commit on the file line. There is no session link, and there must not be
    /// one: the file line never claims a session, so "no link" is its normal state, not a defect.
    FileLine {
        repo_dir: std::path::PathBuf,
        slug: String,
        branch: String,
        via: &'static str,
    },
}

pub fn run(args: Args) -> CmdResult {
    let from_hook = args.from_hook;
    match run_inner(args) {
        Ok(c) => Ok(c),
        // A --from-hook failure never blocks the runtime: exit 0 quietly. Error text on stderr
        // is only noise — nobody reads it in a hook.
        Err(_) if from_hook => Ok(ExitCode::Ok),
        // A hand-typed `agit commit` runs the same run_inner, so this arm must test from_hook:
        // swallowing every internal error into a wordless exit 0 shows whoever is diagnosing
        // "the command succeeded and nothing happened". Quiet belongs to hooks alone.
        Err(e) => {
            ui::error(&format!("{e:#}"));
            Ok(ExitCode::Failure)
        }
    }
}

fn run_inner(args: Args) -> CmdResult {
    let quiet = args.from_hook;
    if let Some(code) = delegated_settlement(args.from_hook)? {
        return Ok(code);
    }
    if (args.from_hook || args.from_supervisor)
        && super::config::get("commit.auto").as_deref() == Some("false")
    {
        if args.from_hook {
            return Ok(ExitCode::Ok); // hook settlement is explicitly off: stay quiet
        }
        ui::error("automatic settlement is disabled by `commit.auto = false`");
        return Ok(ExitCode::Precondition);
    }

    // The sign-in check comes first: a commit's author fields come from the credentials and
    // cannot be backfilled.
    let Some(owner) = owner_for_recording(quiet)? else {
        return Ok(if quiet { ExitCode::Ok } else { ExitCode::Auth });
    };

    // The store indexes "which sessions this machine tracks", and empty is a legal state: `-m`
    // on the file line depends on no session, and a machine that never ran import/new must still
    // be able to commit shared files to main. A missing store means there is no link, not a
    // reason to refuse.
    let store = Store::open_or_init()?;

    // When the Stop hook names the transcript it fired for, settle that one only — its stdin is
    // the only source that knows "which conversation this Stop belongs to". See [`hook_target`].
    let hook_input = if args.from_hook {
        HookInput::from_stdin()
    } else {
        None
    };
    let target = match hook_input.as_ref().and_then(|h| h.session_id.as_deref()) {
        Some(sid) => hook_target(
            &store,
            sid,
            hook_input.as_ref().and_then(|h| h.cwd.as_deref()),
        ),
        None => resolve_target(&store, &args, quiet)?,
    };
    let Some(target) = target else {
        return Ok(if quiet { ExitCode::Ok } else { ExitCode::Ref });
    };

    let opts = SettleOpts {
        milestone: args.milestone,
        tag: args.tag,
        code: args.code,
        message: args.message,
        paths: args.paths,
        quiet,
    };

    match target {
        Target::Branch {
            repo_dir,
            slug,
            branch,
            link,
            via,
        } => {
            if std::env::var_os(crate::hub::identity::EXPECTED_AGENT_ID_ENV).is_some() {
                let repo = Repo::open(&repo_dir).ok_or_else(|| {
                    anyhow::anyhow!(
                        "the identity-fenced settlement checkout {} does not exist",
                        repo_dir.display()
                    )
                })?;
                crate::hub::identity::require_current_expected(
                    &repo,
                    &crate::infra::config::hub_url(),
                )?;
            }
            if !quiet {
                println!(
                    "{}",
                    ui::dim(&format!("  target: {slug} @ {branch} ({via})"))
                );
            }
            settle(&store, &repo_dir, &slug, &branch, link, &owner, opts)
        }
        Target::Legacy { link, agent } => {
            if std::env::var_os(crate::hub::identity::EXPECTED_AGENT_ID_ENV).is_some() {
                anyhow::bail!(
                    "an identity-fenced RC settlement did not resolve to its exact branch checkout"
                );
            }
            // A link that records a namespace resolves the repo under it, and a missing one is
            // an error — never fall back to the signed-in account and guess a different repo of
            // the same name. A link that records none (a legacy link) is under the user's own.
            let namespace = link.owner.clone().unwrap_or_else(|| owner.clone());
            let repo_dir = if link.owner.is_some() {
                let dir = crate::infra::config::repo_dir(&namespace, &agent)?;
                if Repo::open(&dir).is_none() {
                    anyhow::bail!(
                        "this session is claimed on {namespace}/{agent}, but that checkout is missing ({}); clone it back with `agit clone {namespace}/{agent}`",
                        dir.display()
                    );
                }
                dir
            } else {
                super::clone::checkout_for_recording(&namespace, &agent)?
            };
            let repo = Repo::open_or_init(&repo_dir)?;
            let branch = link
                .branch
                .clone()
                .or_else(|| repo.current_branch())
                .unwrap_or_else(|| "main".into());
            let slug = format!("{namespace}/{agent}");
            settle(&store, &repo_dir, &slug, &branch, link, &owner, opts)
        }
        Target::FileLine {
            repo_dir,
            slug,
            branch,
            via,
        } => {
            if !quiet {
                println!(
                    "{}",
                    ui::dim(&format!("  target: {slug} @ {branch} ({via}, file line)"))
                );
            }
            settle_file_line(&repo_dir, &slug, &branch, &owner, opts)
        }
    }
}

/// The delegation gate for settlement: every settlement entry point passes it before touching the
/// store or the repo.
///
/// A harness the supervisor started holds only the environment snapshot it was launched with and
/// cannot see whether the connection is still alive; settling on its own would move the branch
/// outside the supervisor's ACK lease and race the strict watermark. So the gate lives in this one
/// place: no settlement path may go around it, and none may read local state before passing it. A
/// hook yields quietly; an explicit command errors, so a safe no-op is never taken for a settled
/// turn.
///
/// `Some(code)` means this process must not settle and exits with that code.
pub(crate) fn delegated_settlement(from_hook: bool) -> crate::Result<Option<ExitCode>> {
    if !crate::rc::harness::settlement_is_delegated() {
        return Ok(None);
    }
    if from_hook {
        return Ok(Some(ExitCode::Ok));
    }
    anyhow::bail!(
        "{}; finish the turn and let agitd commit it under the live identity lease",
        crate::rc::harness::SUPERVISED_SETTLEMENT_MESSAGE
    );
}

/// The account name a recorded version needs. Under `quiet` (from_hook) it returns None silently.
pub fn owner_for_recording(quiet: bool) -> crate::Result<Option<String>> {
    let c = crate::hub::Client::from_env();
    if !c.has_token() {
        if !quiet {
            ui::error(&format!(
                "sign in to {} first — versions record who wrote them.",
                c.base()
            ));
            ui::hint(
                "a version names its author and repo paths need the account name — neither can be backfilled",
            );
            ui::hint("`agit login`");
        }
        return Ok(None);
    }
    match credentials::current_user() {
        Some(o) => Ok(Some(o)),
        None => {
            if !quiet {
                ui::error("no account name in the stored credentials.");
                ui::hint("re-run `agit login`");
            }
            Ok(None)
        }
    }
}

/// Settle by one **link that already carries a claim** — the entry point of `agit hooks settle`.
///
/// # Why this does not go through [`resolve_target`]
///
/// That chain starts with context resolution, whose second step is `AGIT_SESSION`: a
/// process-level environment variable that goes stale the moment the user switches sessions with
/// `/resume` / `/clear` in the runtime's TUI (`/clear` mints a new id on the spot, `/resume`
/// switches to the id of the stretch it restored). Settling by it writes session B's content into
/// session A's history.
///
/// The Stop hook's payload already carries the answer to **which session the turn that just
/// ended happened in**, so this goes from the link straight to the branch and guesses nothing.
///
/// Quiet: every unmet precondition returns `Ok`, since nobody reads output in a hook.
pub(crate) fn settle_from_link(store: &Store, lk: Link) -> CmdResult {
    let Some(owner) = owner_for_recording(true)? else {
        return Ok(ExitCode::Ok);
    };
    // An unadopted session does not settle — hook convenience does not get to break the premise
    // that adoption is explicit.
    let (Some(agent), Some(branch)) = (lk.agent.clone(), lk.branch.clone()) else {
        return Ok(ExitCode::Ok);
    };
    let slug = link_slug(&lk, &agent, None);
    let (o, n) = super::parse_slug(&slug)?;
    let dir = crate::infra::config::repo_dir(&o, &n)?;
    // Settle only into a local repo that already exists: exit quietly when it is missing, and
    // never create one out of nowhere under any name — least of all for a link that records a
    // namespace, where a missing checkout is simply missing.
    if Repo::open(&dir).is_none() {
        return Ok(ExitCode::Ok);
    }
    settle(
        store,
        &dir,
        &slug,
        &branch,
        lk,
        &owner,
        SettleOpts {
            milestone: None,
            tag: None,
            code: false,
            message: None,
            paths: Vec::new(),
            quiet: true,
        },
    )
}

/// The part we use of the JSON the harness feeds the Stop hook.
///
/// Claude Code's form is `{"session_id", "transcript_path", "cwd", "hook_event_name", ...}`, the
/// same structure SessionStart hands to `agit hooks ingest`.
#[derive(Debug, Default, PartialEq, Eq)]
struct HookInput {
    session_id: Option<String>,
    cwd: Option<std::path::PathBuf>,
}

impl HookInput {
    /// Read only when stdin is a pipe — a person typing `agit commit --from-hook` in a terminal
    /// must not hang waiting for an EOF that never comes.
    fn from_stdin() -> Option<HookInput> {
        use std::io::{IsTerminal as _, Read as _};
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            return None;
        }
        let mut buf = String::new();
        stdin.lock().read_to_string(&mut buf).ok()?;
        Self::parse(&buf)
    }

    fn parse(text: &str) -> Option<HookInput> {
        let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
        Some(HookInput {
            session_id: v
                .get("session_id")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            cwd: v
                .get("cwd")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from),
        })
    }
}

/// The target of a hook settlement: the branch belonging to the session named on stdin, and
/// nothing at all when there is none.
///
/// The context-resolution chain is the wrong tool here. `AGIT_SESSION` is injected when the
/// runtime starts and is constant across the whole process tree; after `/new` or `/resume`
/// switches sessions in the TUI it still points at the line the runtime started on, and the
/// pinned branch and the cwd's only session equally do not know whose conversation is in front of
/// them. Settling by any of those either drops the new session's turns silently (it reads the old
/// transcript and reports "nothing new") or records another session's content on this line. The
/// link is the only place that records a claim by session id: settle a claimed one, and treat a
/// `hooks ingest` pre-registration, or no registration at all, as a conversation nobody has
/// adopted yet — exit quietly and let `agit import` decide where it goes.
fn hook_target(store: &Store, session_id: &str, hook_cwd: Option<&Path>) -> Option<Target> {
    let lk = link::list(store)
        .into_iter()
        .find(|l| l.session_id == session_id)?;
    let (agent, branch) = hook_claim(&lk)?;
    let slug = link_slug(&lk, &agent, hook_cwd);
    let (owner, name) = super::parse_slug(&slug).ok()?;
    let repo_dir = crate::infra::config::repo_dir(&owner, &name).ok()?;
    Repo::open(&repo_dir)?;
    Some(Target::Branch {
        repo_dir,
        slug,
        branch,
        link: lk,
        via: "hook session id",
    })
}

/// Whether a link carries a claim a hook can settle: both the agent and the branch are
/// registered.
fn hook_claim(lk: &Link) -> Option<(String, String)> {
    Some((lk.agent.clone()?, lk.branch.clone()?))
}

/// The full `owner/name` of a link.
///
/// When the link records a namespace itself (written the moment the claim recorded a version),
/// use it; for a legacy link that records only a bare agent name, the owner is recovered in turn
/// from the injected session identity, the workspace binding of the payload's or the link's
/// working directory, and the current user. Every hook settlement path takes its slug from here,
/// so a checkout under someone else's name is never reinterpreted, on any path, as the current
/// user's repo of the same name.
fn link_slug(lk: &Link, agent: &str, hook_cwd: Option<&Path>) -> String {
    let injected = std::env::var("AGIT_SESSION")
        .ok()
        .and_then(|v| super::context::decode_session_env(&v))
        .map(|(repo, _)| repo);
    let cwd = hook_cwd
        .map(Path::to_path_buf)
        .or_else(|| lk.cwd.as_deref().map(std::path::PathBuf::from))
        .or_else(|| std::env::current_dir().ok());
    let bound = cwd
        .and_then(|d| crate::domain::workspace::read(&d))
        .map(|w| w.repo);
    hook_slug(
        agent,
        lk.owner.as_deref(),
        injected.as_deref(),
        bound.as_deref(),
    )
}

/// The link records only a bare agent name, so the owner has to be recovered from elsewhere.
///
/// Filling it in as "signed-in user/agent" is right inside one's own repo and wrong inside
/// someone else's read-only checkout: the link for `alice/qa` also writes only `agent = "qa"`,
/// looking up `me/qa` under the current user finds nothing, and the hook stalls silently. Both
/// the `AGIT_SESSION` injected when the runtime starts and the directory binding record a full
/// `owner/name` — as long as the agent name matches, the owner they name is this link's owner.
///
/// When the link records a namespace itself (written the moment the claim recorded a version),
/// use it and keep the environment out of it: a missing checkout is simply missing and the hook
/// does not settle (the caller exits on the absent directory), rather than falling back to the
/// signed-in account to find a different repo of the same name.
fn hook_slug(
    agent: &str,
    claimed: Option<&str>,
    injected: Option<&str>,
    bound: Option<&str>,
) -> String {
    if let Some(owner) = claimed {
        return format!("{owner}/{agent}");
    }
    let names = |slug: &str| slug.rsplit('/').next().unwrap_or(slug) == agent;
    injected
        .filter(|s| names(s))
        .or_else(|| bound.filter(|s| names(s)))
        .map(str::to_string)
        .unwrap_or_else(|| super::context::qualify(agent))
}

/// Resolve `commit`'s target: branch semantics first, the legacy form (agent name / session id)
/// as the fallback.
fn resolve_target(store: &Store, args: &Args, quiet: bool) -> crate::Result<Option<Target>> {
    let cwd = std::env::current_dir()?;

    // Unified form: `owner/repo@branch`.  It is resolved independently of the
    // current directory; the legacy bare branch / session-id forms below keep
    // their old context behavior for compatibility.
    if let Some(raw) = args.target.as_deref()
        && raw.contains('@')
        && raw != "@"
    {
        let parsed = match crate::commands::target::branch_only(raw) {
            Ok(v) => v,
            Err(e) => {
                if !quiet {
                    ui::error(&format!("{e:#}"));
                }
                return Ok(None);
            }
        };
        let slug = parsed
            .repo
            .clone()
            .ok_or_else(|| anyhow::anyhow!("commit target has no repository"))?;
        let branch = parsed
            .base
            .clone()
            .ok_or_else(|| anyhow::anyhow!("commit target has no branch"))?;
        let (owner, name) = super::parse_slug(&slug)?;
        let dir = crate::infra::config::repo_dir(&owner, &name)?;
        let Some(_repo) = Repo::open(&dir) else {
            if !quiet {
                ui::error(&format!("{slug} doesn’t exist locally."));
                ui::hint(&format!("fetch it first: `agit clone {slug}`"));
            }
            return Ok(None);
        };
        let mut hits: Vec<Link> = link::list(store)
            .into_iter()
            .filter(|l| {
                l.agent.as_deref() == Some(name.as_str())
                    && l.branch.as_deref() == Some(branch.as_str())
                    // A link that records a namespace belongs to that namespace alone: when a
                    // personal and an org repo share a name, each finds its own.
                    && l.owner.as_deref().is_none_or(|o| o == owner)
            })
            .collect();
        return match hits.len() {
            1 => Ok(Some(Target::Branch {
                repo_dir: dir,
                slug,
                branch,
                link: hits.remove(0),
                via: "explicit owner/repo@branch",
            })),
            0 => {
                if let Some(t) = file_line_target(
                    &dir,
                    &slug,
                    &branch,
                    args.message.is_some(),
                    "explicit owner/repo@branch",
                ) {
                    return Ok(Some(t));
                }
                if !quiet {
                    ui::error(&format!(
                        "{slug}@{branch} has no registered live transcript."
                    ));
                    ui::hint("import the runtime session onto this branch first");
                }
                Ok(None)
            }
            _ => {
                if !quiet {
                    ui::error(&format!(
                        "{slug}@{branch} has multiple session links — can’t pick for you."
                    ));
                }
                Ok(None)
            }
        };
    }

    // `@` / omitted / a branch name: the context-resolution chain.
    let wants_ctx = args.target.is_none() || args.target.as_deref() == Some("@");
    let ctx = super::context::resolve(&cwd).ok();

    if let Some(ctx) = &ctx {
        let branch = match args.target.as_deref() {
            None | Some("@") => ctx.branch.clone(),
            Some(b) => b.to_string(),
        };
        let (owner, name) = ctx.owner_name()?;
        let dir = crate::infra::config::repo_dir(&owner, &name)?;
        if let Some(repo) = Repo::open(&dir) {
            if !wants_ctx && !repo.has_ref(&format!("refs/heads/{branch}")) {
                // An explicit name this repo does not have — do not fall into a silent
                // misresolution; take the legacy fallback.
            } else {
                let links = link::list(store);
                let mut hits: Vec<Link> = links
                    .into_iter()
                    .filter(|l| {
                        l.agent.as_deref() == Some(name.as_str())
                            && l.owner.as_deref().is_none_or(|o| o == owner)
                            && (l.branch.as_deref() == Some(branch.as_str())
                                // A legacy link has no branch field: it serves the branch
                                // the repo currently has checked out.
                                || (l.branch.is_none()
                                    && repo.current_branch().as_deref() == Some(branch.as_str())))
                    })
                    .collect();
                return match hits.len() {
                    1 => Ok(Some(Target::Branch {
                        repo_dir: dir,
                        slug: format!("{owner}/{name}"),
                        branch,
                        link: hits.remove(0),
                        via: ctx.via,
                    })),
                    0 => {
                        if let Some(t) = file_line_target(
                            &dir,
                            &format!("{owner}/{name}"),
                            &branch,
                            args.message.is_some(),
                            ctx.via,
                        ) {
                            return Ok(Some(t));
                        }
                        if !quiet {
                            ui::error(&format!(
                                "{} @ {branch} has no registered live transcript (no session link points at it).",
                                ctx.repo
                            ));
                            ui::hint(
                                "this branch is someone else’s history: continue it with `agit resume <branch>`; to settle the current session, `agit import` first",
                            );
                        }
                        Ok(None)
                    }
                    many => {
                        if !quiet {
                            ui::error(&format!(
                                "branch {branch} has {many} session links — can’t pick for you:"
                            ));
                            for l in link::list(store).iter().take(8) {
                                println!("  {:12} {}", l.source, link::short(&l.session_id));
                            }
                            ui::hint(
                                "session identity conflicts shouldn’t happen; remove extra store links by hand (`agit status` lists them)",
                            );
                        }
                        Ok(None)
                    }
                };
            }
        } else if !quiet {
            ui::error(&format!("{} doesn’t exist locally.", ctx.repo));
            ui::hint(&format!("fetch it first: `agit clone {}`", ctx.repo));
            return Ok(None);
        }
    }

    // The legacy form: agent name / session id prefix.
    if let Some(t) = args.target.as_deref() {
        match locate(store, Some(t))? {
            Located::Found(l) => {
                let agent = match (&args.name, &l.agent) {
                    (Some(n), _) => {
                        repo::valid_name(n)?;
                        n.clone()
                    }
                    (None, Some(a)) => a.clone(),
                    (None, None) => {
                        if !quiet {
                            ui::error("this session has no home yet — it needs a name.");
                            ui::hint(&format!(
                                "agit commit {} -n <name>",
                                link::short(&l.session_id)
                            ));
                        }
                        return Ok(None);
                    }
                };
                return Ok(Some(Target::Legacy { link: l, agent }));
            }
            Located::Explained(code) => {
                let _ = code;
                return Ok(None);
            }
        }
    }

    if !quiet {
        ui::error(
            "no settlement target: not inside an agent session, no pinned branch, no argument given.",
        );
        ui::hint("try `agit commit <branch>`, or `agit import` / `agit switch` first");
    }
    Ok(None)
}

// ───────────────────── Settlement engine ──────────────────────

struct SettleOpts {
    milestone: Option<String>,
    tag: Option<String>,
    code: bool,
    message: Option<String>,
    paths: Vec<String>,
    quiet: bool,
}

/// The commit one turn lands.
struct Chunk {
    /// The byte offset inside the settlement region that this turn's end covers.
    end_byte: usize,
    /// The first line of the user prompt (truncated to 72).
    gist: String,
    events: usize,
    dropped: usize,
}

/// Why the trailing turn cannot settle yet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InFlight {
    /// Only a user prompt; the agent has made no reply of any kind.
    Unanswered,
    /// The agent started a tool call whose result is not written back to the transcript yet;
    /// `name` is that tool.
    OpenCall { name: String },
}

/// Whether the last turn is still running.
///
/// Both tests are required. Looking only at "is there a reply" treats the moment the agent runs
/// `agit commit` from inside a turn as the end of that turn — it has already spoken and already
/// started a call (this very `agit commit`), so half a turn is cut and settled as a whole one,
/// and the bytes appended after the turn really ends can never join it: skipping by turn ordinal
/// finds the count unchanged and reports "nothing new". Once half a turn lands as a commit, that
/// turn's second half can only ever hang off the next turn.
///
/// Pairing is therefore a hard test: any open tool call ([`OpenCall`]) that falls inside the
/// trailing turn means the turn has not ended. A call left open in an earlier turn does not count
/// — a later user prompt already closed that turn, and [`crate::domain::install`] fills in a
/// placeholder output for it when the transcript goes back into a runtime.
fn in_flight_tail(ir: &Session, open: &[OpenCall]) -> Option<InFlight> {
    let groups = crate::domain::turn::groups_of(ir);
    let last_g = groups.last()?;
    let answered = last_g.iter().any(|&j| {
        matches!(
            ir.events[j].kind,
            EventKind::AssistantReply
                | EventKind::ToolUse
                | EventKind::FileEdit
                | EventKind::CompactSummary
        )
    });
    if !answered {
        return Some(InFlight::Unanswered);
    }
    let first_line = last_g.iter().filter_map(|&j| ir.events[j].line).min()?;
    open.iter()
        .find(|c| c.line >= first_line)
        .map(|c| InFlight::OpenCall {
            name: c.name.clone(),
        })
}

/// Tell the user why the trailing turn is not in this settlement and when it will be.
///
/// Saying so is required: `agit status` reports `in sync` and `agit push` publishes without
/// complaint — neither distinguishes "the last turn is still running". Without this line, what
/// the user sees is a turn stopped halfway on the web while every command says all is well.
fn explain_in_flight(why: &InFlight) {
    match why {
        InFlight::Unanswered => println!(
            "{}",
            ui::dim("  a turn is in flight — settle after the agent replies")
        ),
        InFlight::OpenCall { name } => {
            println!(
                "{}",
                ui::dim(&format!(
                    "  the current turn is still in flight: `{name}` was called and has not returned yet"
                ))
            );
            println!(
                "{}",
                ui::dim(
                    "  HEAD stays at the previous turn; run `agit commit` (then `agit push`) again once the agent finishes this one"
                )
            );
        }
    }
}

/// Cut the settlement region into one chunk per turn.
///
/// Line numbers come from the IR (`Event.line` is the 0-based line number of
/// `text.lines().enumerate()`). A turn with no line number merges into the next one: when the
/// turn splitter cannot give an exact boundary, better merged than cut in the wrong place.
/// **An in-flight trailing turn** (see [`in_flight_tail`]) does not enter the list.
fn turn_chunks(region: &str, ir: &Session, open: &[OpenCall]) -> Vec<Chunk> {
    // The byte extent of each line (newline included).
    let mut ends: Vec<usize> = vec![];
    let mut off = 0usize;
    for l in region.split_inclusive('\n') {
        off += l.len();
        ends.push(off);
    }
    if ends.is_empty() || *ends.last().unwrap() != region.len() {
        ends.push(region.len()); // the last line carries no newline
    }
    let chain = crate::domain::turn::chain_of(ir);
    let groups = crate::domain::turn::groups_of(ir);
    let mut chunks: Vec<Option<Chunk>> = vec![];
    for (i, g) in groups.iter().enumerate() {
        let end_line = g.iter().filter_map(|&j| ir.events[j].line).max();
        // ends[i] is the byte offset after line i; the end of line l is ends[l].
        let end_byte = end_line.map(|l| ends.get(l).copied().unwrap_or(region.len()));
        let events = g.len();
        let dropped = chain.turns.get(i).map(|t| t.dropped).unwrap_or(0);
        chunks.push(end_byte.map(|e| {
            let gist = chain
                .turns
                .get(i)
                .map(|t| t.gist.clone())
                .unwrap_or_default();
            Chunk {
                end_byte: e,
                gist,
                events,
                dropped,
            }
        }));
    }
    // `None` (a turn with no line number) merges into the next one: take the next known end.
    // When the whole chain has no line numbers, per-turn settlement is impossible (the caller
    // gets an empty list and degrades to a no-op report).
    let mut out: Vec<Chunk> = vec![];
    for (i, c) in chunks.iter().enumerate() {
        let c = match c {
            Some(c) => c,
            None => match chunks[i + 1..].iter().flatten().next() {
                Some(n) => n,
                None => continue, // no line numbers at all: the caller degrades to one chunk
            },
        };
        if out.last().is_some_and(|p: &Chunk| p.end_byte == c.end_byte) {
            continue; // a card merges into the next turn's boundary
        }
        out.push(Chunk {
            end_byte: c.end_byte,
            gist: c.gist.clone(),
            events: c.events,
            dropped: c.dropped,
        });
    }
    // An in-flight trailing turn stays in the runtime. There is something to pop only when that
    // turn cut a chunk of its own: with no line number it never entered `out`, and popping would
    // take the turn before it instead.
    if in_flight_tail(ir, open).is_some() && chunks.last().is_some_and(|c| c.is_some()) {
        out.pop();
    }
    out
}

/// Extend a committed snapshot with bytes written after a materialized runtime baseline.
///
/// The baseline is a rendering of HEAD's VIEW, not HEAD's evidence carrier. Re-wrapping the full
/// live prefix would therefore replace LOG with that projection and would also mint new envelope
/// provenance for every inherited VIEW event. Keep both committed sequences byte-for-byte and
/// envelope only the genuinely new runtime region. A compact event in the new region is the one
/// exception for VIEW: it intentionally starts a new projection at that boundary.
fn extend_materialized_snapshot(
    base_log: &str,
    base_view: &str,
    appended: &str,
    source: &str,
    session: &str,
) -> crate::Result<(String, String)> {
    let addition = transcript::wrap_lines(appended, source, session);
    let log = append_envelopes(base_log, &addition);

    let appended_ir = crate::adapter::get(source)?.parse(appended)?;
    let view = if transcript::last_compact_boundary(&appended_ir).is_some() {
        let projected = transcript::view_of_live(appended, source)?;
        transcript::wrap_lines(&projected, source, session)
    } else {
        append_envelopes(base_view, &addition)
    };
    Ok((log, view))
}

fn append_envelopes(base: &str, suffix: &str) -> String {
    let mut out = base.to_owned();
    if !out.is_empty() && !out.ends_with('\n') && !suffix.is_empty() {
        out.push('\n');
    }
    out.push_str(suffix);
    out
}

/// Whether a branch that no session link points at can still take a `-m` commit as a **file
/// line**.
///
/// Three things must hold at once: the caller passed `-m`, the branch already exists, and the
/// meta at its tip declares it a file line. Missing any one returns `None` and leaves the caller
/// to treat it as a session line (reporting the missing link) — a session line with no link is
/// someone else's history, not something to take over here.
fn file_line_target(
    repo_dir: &Path,
    slug: &str,
    branch: &str,
    has_message: bool,
    via: &'static str,
) -> Option<Target> {
    if !has_message {
        return None;
    }
    let repo = Repo::open(repo_dir)?;
    let tip = format!("refs/heads/{branch}");
    if !repo.has_ref(&tip) || !meta::read_at_ref(&repo, &tip)?.is_file_line() {
        return None;
    }
    Some(Target::FileLine {
        repo_dir: repo_dir.to_path_buf(),
        slug: slug.to_string(),
        branch: branch.to_string(),
        via,
    })
}

/// A `-m` commit on the file line: the same branch / lock / seal checks as [`settle`], then
/// straight into [`file_commit`] — there is no transcript to read and no session to claim.
fn settle_file_line(
    repo_dir: &Path,
    slug: &str,
    branch: &str,
    owner: &str,
    opts: SettleOpts,
) -> CmdResult {
    let quiet = opts.quiet;
    let Some(primary) = Repo::open(repo_dir) else {
        anyhow::bail!("{slug} doesn’t exist locally ({})", repo_dir.display());
    };
    // The file line's checkout: the main checkout itself while it sits on main, otherwise main's
    // linked worktree. `-m` takes the shared files staged in that checkout.
    let repo = match checkout_for_settlement(&primary, slug, branch)? {
        Ok(repo) => repo,
        Err(code) => return Ok(code),
    };
    if let Some(tx) = mergetx::locking(repo.root(), branch) {
        ui::error(&format!(
            "{} is locked by an open merge transaction ({} → {}).",
            tx.target, tx.source, tx.target
        ));
        ui::hint("`agit merge --status` shows progress; `--continue` to land / `--abort` to drop");
        return Ok(ExitCode::Precondition);
    }
    if super::branch::is_sealed(&repo, branch) {
        ui::error(&format!("`{branch}` is sealed — read-only."));
        return Ok(ExitCode::Policy);
    }
    let msg = opts
        .message
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("a file-line settlement needs `-m`"))?;

    let branch_ref = format!("refs/heads/{branch}");
    let tip = optional_branch_commit(&repo, &branch_ref)?;
    let head_meta = match tip.as_deref() {
        Some(tip) => meta::read_at_ref_result(&repo, tip)?,
        None => None,
    };
    anyhow::ensure!(
        head_meta.as_ref().is_some_and(|m| m.is_file_line()),
        "`{branch}` is not a file line — only the file line takes a session-less `-m` commit"
    );

    // The author fields come from the credentials, the same as on the transcript path.
    repo.git(&["config", "user.name", owner])?;
    let email = credentials::current_email().unwrap_or_else(|| format!("{owner}@agit.local"));
    repo.git(&["config", "user.email", &email])?;

    file_commit(
        &repo,
        slug,
        branch,
        tip.as_deref(),
        &head_meta,
        msg,
        &opts.paths,
        quiet,
    )
}

/// The checkout for the settlement's target branch. `Err(exit code)` means the reason is already
/// explained and the caller exits with that code.
///
/// Three cases: the branch exists → its own worktree (the main checkout stays on main, and two
/// concurrent session settlements each write their own); it does not exist and the repo already
/// has commits → refuse (a branch is born only from import / fork / new / run); a brand-new repo
/// → `symbolic-ref` moves the main checkout's HEAD from init's default main onto this session
/// branch, so the first turn commit lands on the session branch.
/// **main is the file line and never claims a session**: standing in with main would record the
/// opening turn of a conversation on the team's memory trunk.
fn checkout_for_settlement(
    primary: &Repo,
    slug: &str,
    branch: &str,
) -> crate::Result<Result<Repo, ExitCode>> {
    let target_ref = format!("refs/heads/{branch}");
    if primary.has_ref(&target_ref) {
        return Ok(Ok(super::worktree::checkout(primary, branch)?));
    }
    if primary.commit_count() > 0 {
        ui::error(&format!("{slug} has no branch `{branch}`."));
        ui::hint("branches are born only via import / fork / new / run");
        return Ok(Err(ExitCode::Precondition));
    }
    primary.git(&["symbolic-ref", "HEAD", &target_ref])?;
    Ok(Ok(primary.clone()))
}

/// The main settlement flow. Returns the process exit code.
fn settle(
    store: &Store,
    repo_dir: &Path,
    slug: &str,
    branch: &str,
    lk: Link,
    owner: &str,
    opts: SettleOpts,
) -> CmdResult {
    let quiet = opts.quiet;
    let fresh = !repo_dir.join(".git").exists();
    let primary = Repo::open_or_init(repo_dir)?;
    let repo = match checkout_for_settlement(&primary, slug, branch)? {
        Ok(repo) => repo,
        Err(code) => return Ok(code),
    };

    // The merge transaction lock blocks **the locked branch**, not the whole repository.
    //
    // Locking a repository freezes a second session running in parallel, while a merge
    // transaction only CASes the target branch's head — settling another branch never touches it.
    //
    // The hook's suspension test reads `AGIT_MERGE_TX` from the session environment first (scoped
    // exactly to that merge agent session) and falls back to the lock file when it is absent. The
    // design says "suspend automatically for a session marked with AGIT_MERGE_TX", not "the whole
    // repository stops for the duration of the transaction".
    if quiet {
        if mergetx::hook_suspended(repo.root(), branch) {
            return Ok(ExitCode::Ok);
        }
    } else if let Some(tx) = mergetx::locking(repo.root(), branch) {
        ui::error(&format!(
            "{} is locked by an open merge transaction ({} → {}).",
            tx.target, tx.source, tx.target
        ));
        ui::hint("`agit merge --status` shows progress; `--continue` to land / `--abort` to drop");
        return Ok(ExitCode::Precondition);
    }
    // A sealed branch is not writable.
    if super::branch::is_sealed(&repo, branch) {
        ui::error(&format!("`{branch}` is sealed — read-only."));
        ui::hint(&format!(
            "fork it to keep working: `agit fork {branch} -b <new> --resume`"
        ));
        return Ok(ExitCode::Policy);
    }

    let bytes = lk.read_bytes().map_err(|e| {
        if quiet {
            anyhow::anyhow!("quiet: {e:#}")
        } else {
            e
        }
    })?;
    let memory_link = lk.clone();
    let milestone = opts.milestone.is_some();
    let code = settle_bytes(
        store, &repo, slug, branch, lk, &bytes, owner, opts, fresh, quiet,
    )?;

    // Memory: what changed in the runtime directory since the last sync is collected onto this
    // branch as one file commit. Collect it even when the transcript has no new turn — memory can
    // be edited between turns. A failure here does not drag down a settlement that already
    // landed.
    //
    // The strict RC path's result file must name the branch's **final** tip: the supervisor
    // compares it against the post-settlement watermark, and since the memory commit lands after
    // the transcript commits, the receipt moves forward with it; when there is only memory and no
    // new turn, the receipt is written here too.
    if code == ExitCode::Ok
        && let Some(cwd) = memory_link.cwd.as_deref()
    {
        match super::memory::collect(&primary, branch, slug, &memory_link.source, Path::new(cwd)) {
            Ok(Some(report)) => {
                // Record the receipt before reporting: the commit is a fact, and a warning in
                // the report (the baseline was not written) does not change it.
                if let Some(commit) = &report.commit {
                    record_supervisor_result(commit)?;
                }
                if !quiet {
                    super::memory::report_collect(&report, branch);
                }
            }
            Ok(None) => {}
            Err(error) => {
                if !quiet {
                    ui::warning(&format!("memory was not collected: {error:#}"));
                }
            }
        }
    }
    if milestone && !quiet {
        super::memory::remind_pending(&primary, branch);
    }
    Ok(code)
}

fn record_supervisor_result(commit_sha: &str) -> crate::Result<()> {
    let Some(path) = std::env::var_os(SUPERVISOR_RESULT_ENV) else {
        return Ok(());
    };
    anyhow::ensure!(
        !commit_sha.is_empty(),
        "strict supervisor settlement produced an empty commit id"
    );
    std::fs::write(Path::new(&path), format!("{commit_sha}\n"))?;
    Ok(())
}

/// Resolve one exact branch ref without collapsing Git failures into the unborn-branch case.
///
/// `git_opt(rev-parse)` cannot distinguish a genuinely absent ref from a corrupt ref/object store
/// or a Git execution error. `for-each-ref` gives us a successful, strict enumeration (including
/// warnings for broken refs); filtering its result ourselves also avoids confusing `main` with
/// `main/child`. Once found, the object must still peel to a commit.
fn optional_branch_commit(repo: &Repo, branch_ref: &str) -> crate::Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["for-each-ref", "--format=%(refname)", "--", branch_ref])
        .output()
        .map_err(|error| {
            anyhow::anyhow!("git for-each-ref {branch_ref} failed to start: {error}")
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "git for-each-ref {branch_ref} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    anyhow::ensure!(
        output.stderr.is_empty(),
        "git for-each-ref {branch_ref} reported corruption: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let refs = String::from_utf8(output.stdout)
        .map_err(|_| anyhow::anyhow!("git for-each-ref returned a non-UTF-8 ref name"))?;
    let mut exact = refs.lines().filter(|name| *name == branch_ref);
    if exact.next().is_none() {
        return Ok(None);
    }
    anyhow::ensure!(
        exact.next().is_none(),
        "git for-each-ref returned {branch_ref} more than once"
    );
    let commit = repo.git(&["rev-parse", "--verify", &format!("{branch_ref}^{{commit}}")])?;
    anyhow::ensure!(
        !commit.is_empty(),
        "git rev-parse returned an empty commit id for {branch_ref}"
    );
    Ok(Some(commit))
}

/// Snapshot every ordinary worktree path into an immutable tree for an unborn branch.
///
/// The historical root-commit path staged through the real index and invoked `git commit`, which
/// meant a second first writer could silently observe the branch created by the winner and parent
/// its already-consumed turn onto that mutable HEAD. Build the shared-file portion in a temporary
/// index instead. AgentGit-owned paths are removed from that index and installed explicitly by
/// [`unborn_session_snapshot_tree`], so neither the real index nor the worktree changes before the
/// expected-absent ref CAS.
fn unborn_worktree_tree(repo: &Repo) -> crate::Result<String> {
    let empty_tree = super::plumbing::raw_git(
        repo,
        &["hash-object", "-w", "-t", "tree", "--stdin"],
        Some(""),
    )?;
    let git_dir = repo.git(&["rev-parse", "--absolute-git-dir"])?;
    // Seed from the real index so paths already staged with `git add -f` remain tracked even when
    // the worktree's ignore rules would hide them from a brand-new index. The temporary index sits
    // beside the real one so Git's split-index extension can still resolve sharedindex objects.
    let temporary_index = tempfile::Builder::new()
        .prefix("agit-unborn-index-")
        .tempfile_in(git_dir.trim())?
        .into_temp_path();
    let index: &std::path::Path = temporary_index.as_ref();
    let real_index = file_commit_git_path(repo, "index")?;
    match std::fs::copy(&real_index, index) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::remove_file(index)?;
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot snapshot the real index {} for an unborn settlement: {error}",
                real_index.display()
            ));
        }
    }
    let run = |args: &[&str]| -> crate::Result<String> {
        let output = std::process::Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repo.root())
            .args(args)
            .env("GIT_INDEX_FILE", index)
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "git {} failed while building an unborn settlement tree: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    };

    if !index.exists() {
        run(&["read-tree", "--empty"])?;
    }
    // This deliberately matches the old root `git add -A`: the worktree layer wins over any
    // pre-existing staged layer, ignored untracked files stay ignored, and tracked deletions land.
    run(&["add", "-A", "--", "."])?;
    let managed = [
        meta::FILE,
        meta::LEGACY_LOG_FILE,
        meta::LEGACY_VIEW_FILE,
        meta::LOG_FILE,
        meta::VIEW_FILE,
        meta::EVENTS_DIR,
    ];
    let mut reset = vec!["reset", "-q", empty_tree.trim(), "--"];
    reset.extend(managed);
    run(&reset)?;
    let tree = run(&["write-tree"])?;
    let tree = tree.trim().to_owned();
    let leaked: Vec<String> = repo
        .ls_tree_result(&tree)?
        .into_iter()
        .filter(|path| meta::is_storage_path(path))
        .collect();
    anyhow::ensure!(
        leaked.is_empty(),
        "unborn settlement tree still contains managed storage paths: {}",
        leaked.join(", ")
    );
    Ok(tree)
}

/// Install one canonical v1 session snapshot on top of an unborn branch's shared-file tree.
///
/// `plumbing::session_snapshot_tree` also performs a checked-out-HEAD upgrade preflight, which is
/// correct for an existing branch but cannot resolve `HEAD^{commit}` while this branch is unborn.
/// The caller has already proved the root namespace is free in the worktree; this helper only
/// performs immutable tree edits and keeps user-owned `.gitattributes` rules.
fn unborn_session_snapshot_tree(
    repo: &Repo,
    base_tree: &str,
    log: &str,
    view: &str,
    meta_text: &str,
) -> crate::Result<String> {
    let attributes = super::plumbing::regular_blob_text_at(repo, base_tree, meta::ATTRS_FILE)?;
    let mut edits: std::collections::BTreeMap<String, Option<Vec<u8>>> = repo
        .ls_tree_result(base_tree)?
        .into_iter()
        .filter(|path| meta::is_storage_path(path))
        .map(|path| (path, None))
        .collect();
    for (path, bytes) in storage::snapshot_files(log, view)? {
        edits.insert(path, Some(bytes));
    }
    edits.insert(meta::FILE.to_owned(), Some(meta_text.as_bytes().to_vec()));
    edits.insert(
        meta::ATTRS_FILE.to_owned(),
        Some(storage::attributes_text_strict(attributes.as_deref())?.into_bytes()),
    );
    super::plumbing::tree_apply_owned(repo, base_tree, edits.into_iter().collect())
}

/// The content half of a settlement (its own function so tests can build bytes directly and skip
/// the runtime-directory lookup).
#[allow(clippy::too_many_arguments)]
fn settle_bytes(
    store: &Store,
    repo: &Repo,
    slug: &str,
    branch: &str,
    mut lk: Link,
    bytes: &[u8],
    owner: &str,
    opts: SettleOpts,
    fresh: bool,
    quiet: bool,
) -> CmdResult {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let materialized_mode = lk.baseline_bytes.is_some();
    // The CAS expected value for writing the watermark back is rebuilt from `lk` itself
    // (isomorphic to the bytes `link::write` persists): `lk` was read earlier in the call chain,
    // and reading the disk now can hand back a new claim written by a reroute — exactly the thing
    // that must not serve as `expected`. When the serialization does not match (a hand-edited
    // link file included), skip the write-back; that direction is the safe one.
    let link_disk_at_entry = lk.to_json().ok().map(|j| format!("{j}\n").into_bytes());

    // Freeze the target branch exactly once before reading any committed state. Every metadata,
    // sequence, parent-tree and CAS decision below must refer to this immutable object id: if a
    // concurrent settlement advances the branch after these reads, our final expected-old CAS
    // fails instead of parenting the same runtime turn onto the newer tip a second time.
    //
    // `None` preserves the unborn-branch path used by a brand-new repository. `checkout_for_settlement`
    // already selected the requested branch before entering this function.
    let branch_ref = format!("refs/heads/{branch}");
    let settlement_tip = optional_branch_commit(repo, &branch_ref)?;
    let has_head = settlement_tip.is_some();
    let head_meta = if let Some(tip) = settlement_tip.as_deref() {
        // Once a branch has a commit, absence and corruption are different states:
        // malformed/non-UTF-8/invalid meta must stop every mutating path instead of being
        // mistaken for an old v0 branch.
        Some(meta::read_at_ref_result(repo, tip)?.ok_or_else(|| {
            anyhow::anyhow!(
                "existing branch tip {tip} has no {}; adopt or repair it explicitly before committing",
                meta::FILE
            )
        })?)
    } else {
        None
    };
    // The form always comes from `meta.line`. Inferring it from "does the tree hold a session
    // file" misreads a new session branch, which has no log.jsonl either until its first turn
    // lands, as a file line — that is the W1 deadlock.
    let file_line = head_meta.as_ref().is_some_and(|m| m.is_file_line());

    // Canonical session objects carry repository-scoped placeholders. The
    // runtime remains plaintext; conversion happens here, before envelope
    // hashes and commit identities are formed. A pure file line remains on the
    // existing push-gate policy and must not needlessly unlock a session vault.
    let global_secrets = if file_line {
        crate::domain::secret_filter::Matcher::empty()
    } else {
        crate::domain::secret_filter::VaultStore::open_default()?.matcher()?
    };
    let secret_dictionary = crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?;
    let protected_full = if file_line {
        crate::domain::secret_filter::ProtectionReport {
            text: text.clone(),
            replacements: 0,
            new_records: 0,
            new_heuristic_records: 0,
            intact: 0,
        }
    } else {
        // The dictionary is created lazily, by the first finding. On a machine
        // whose keystore cannot hold a key that first finding is also the first
        // time a commit can fail for a reason that has nothing to do with the
        // commit — say which decision needed the keystore, since the underlying
        // error only knows that a KEK could not be stored. Quietly degrading to
        // a key file beside the vault is not on the table (docs/05, §3.2); the
        // file keystore is a setting the user makes.
        secret_dictionary
            .protect_jsonl(&text, &global_secrets)
            .map_err(|error| {
                anyhow::anyhow!(
                    "cannot protect this session's secrets before committing: {error:#}\n\
                     the repository dictionary keeps its key in the configured keystore \
                     (`agit config secrets.keystore`), and settlement will not fall back to \
                     an unprotected one"
                )
            })?
    };

    // ── Materialized baseline or native continuation ──
    let (region_start, region, head_turn_base) = if let Some(base) = lk.baseline_bytes {
        let base = base as usize;
        if bytes.len() < base {
            ui::error(
                "the live transcript is shorter than its materialization baseline — it was truncated.",
            );
            ui::hint(
                "transcripts are append-only. Investigate with `agit doctor`; if unrecoverable, re-adopt with `agit import`",
            );
            return Ok(ExitCode::Policy);
        }
        if let Some(h) = &lk.baseline_hash {
            let actual = hex::encode(sha2::Sha256::digest(&bytes[..base]));
            use sha2::Digest as _;
            if &actual != h {
                ui::error(
                    "the live transcript was rewritten (non-append) inside its materialization baseline.",
                );
                ui::hint(
                    "those bytes belong to branch history; this session can never settle again — start a fresh line: `agit fork <branch> -b <new>`",
                );
                return Ok(ExitCode::Policy);
            }
        }
        let head_turn = head_meta.as_ref().and_then(|s| s.turn).unwrap_or(0);
        (base, text[base..].to_string(), head_turn)
    } else {
        // Native continuation: the continuity check, against the committed envelope blob.
        let committed = match head_meta.as_ref() {
            // Unborn repositories and file lines legitimately have no session carrier. A newborn
            // session line may also be unclaimed until its first settled turn.
            None => String::new(),
            Some(snapshot) if snapshot.is_file_line() || snapshot.session.is_empty() => {
                String::new()
            }
            Some(_) => {
                let tip = settlement_tip
                    .as_deref()
                    .expect("committed metadata requires a frozen branch tip");
                storage::materialize_at(repo.root(), tip, meta::LOG_FILE).map_err(|error| {
                    anyhow::anyhow!("cannot read committed LOG at {tip}: {error:#}")
                })?
            }
        };
        if transcript::continuity(&committed, &protected_full.text) == Continuity::Diverged {
            // Both classification questions have to be asked on a view where
            // projection differences cannot be mistaken for content differences.
            //
            // «Is this the same session?» is decided on hydrated plaintext. The
            // settled prefix already carries placeholders from whichever records
            // were active when it landed, so comparing it against the live
            // plaintext reports Diverged for every session that ever projected
            // anything — and «already claimed by another session» is the one
            // verdict a user cannot argue with.
            //
            // Both sides are hydrated, not just the settled one. A transcript
            // can legitimately carry this repository's own placeholder as
            // content — an agent that ran `agit show`, or read back
            // `session/log.jsonl` — and projection keeps a syntactically valid
            // token opaque, so such a token settles verbatim. Expanding it on
            // the committed side alone would make the session look foreign for
            // exactly the reason this comparison exists to remove. Neither
            // hydrated view is ever written: `protected_full` is what settles.
            let settled_plaintext = secret_dictionary.hydrate_jsonl(&committed)?;
            let live_plaintext = secret_dictionary.hydrate_jsonl(&text)?;
            let plaintext_continues =
                transcript::continuity_of_content(&settled_plaintext.text, &live_plaintext.text)
                    != Continuity::Diverged;
            // «Would a registered rule rewrite what is already settled?» is
            // decided on the settled *content*, not on the envelopes carrying
            // it. `_session_id` and `_object_hash` are AgentGit's own generated
            // identities and the next snapshot recomputes them from scratch, so
            // a registered literal that happens to be a substring of one
            // rewrites nothing — the scanner masks those same two fields for
            // the mirror-image reason (see `mask_valid_envelope_stream`).
            //
            // Projecting the *live* text instead cannot answer this at all: any
            // heuristic placeholder the prefix already holds is absent from a
            // registered-only view of the live text, so an ordinary forward
            // projection would read as a rewrite. Projecting the settled
            // content leaves its existing placeholders opaque and changes bytes
            // only where that content still carries a registered value in the
            // clear — precisely the condition that must be refused. Count
            // replacements rather than diffing: `protect_*` re-serializes every
            // line it parses, so an unchanged stream comes back with different
            // bytes.
            let settled_content = transcript::unwrap_strict(&committed)?;
            let registered_rewrite =
                secret_dictionary.protect_registered_jsonl(&settled_content)?;
            let settled_prefix_is_stable = registered_rewrite.replacements == 0;
            if plaintext_continues && settled_prefix_is_stable {
                // Forward-only projection is allowed to make the next snapshot
                // canonical while its parent retains the pre-dictionary bytes.
                // Both tests above read the records that actually change the
                // settled prefix, not whether this invocation happened to
                // create them: a failed/no-op/CAS-conflicted attempt may
                // already have saved the heuristic mapping before this retry.
                // This creates no rewritten object/ref and is exactly why the
                // dictionary keeps a versioned pure projection boundary.
                //
                // The reverse direction — `agit secrets allow` on a value the
                // settled prefix already carries as a placeholder — lands here
                // too, and is likewise legitimate: allow is defined to change
                // future projection only, this snapshot is that future, and the
                // parent commit keeps the protected bytes.
            } else if plaintext_continues {
                ui::error("a registered secret now matches the already-settled plaintext prefix.");
                ui::hint(
                    "existing Git objects cannot be replaced without changing version IDs; migrate that unpublished history explicitly before pushing",
                );
                return Ok(ExitCode::Policy);
            } else if settled_plaintext.unresolved > 0 {
                // Reached only once the comparison has already failed, and it
                // says which of the two failures happened: the settled history
                // could not be read back, which is a different fact from «this
                // is someone else's branch». The dictionary lives in `.git` and
                // is never fetched, so history settled on another device — or
                // whose dictionary was deleted — arrives with placeholders
                // nothing here can expand. Saying «claimed by another session»
                // would send the user to `agit import` to abandon a lineage
                // that is in fact their own.
                ui::error(&format!(
                    "{} repository secret placeholder(s) in the settled history cannot be resolved on this device.",
                    settled_plaintext.unresolved
                ));
                ui::hint(
                    "the repository dictionary lives in .git and is never fetched: settle from the device that owns it, or re-adopt this history explicitly with `agit import`",
                );
                return Ok(ExitCode::Policy);
            } else {
                let held = head_meta
                    .as_ref()
                    .map(|m| m.session.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?");
                ui::error(&format!(
                    "{slug} @ {branch} is already claimed by another session."
                ));
                println!(
                    "  this branch’s session is {} — and {} is not its continuation",
                    ui::bold(&meta::short(held)),
                    ui::bold(&link::short(&lk.session_id))
                );
                ui::hint(
                    "one branch, one session: adopt this one onto a new branch with `agit import <session> -n <agent> -b <new-branch>`; add `--onto <ref>` if it continues an existing turn",
                );
                return Ok(ExitCode::Policy);
            }
        }
        let settled = head_meta.as_ref().and_then(|s| s.turn).map(|t| t as usize);
        // No turn ordinal (a newborn session line, or history pushed in from outside): parse
        // the committed content and count its turns.
        let settled = match settled {
            Some(n) => n,
            None => {
                if committed.trim().is_empty() {
                    0
                } else {
                    let src = head_meta
                        .as_ref()
                        .map(|m| m.runtime.as_str())
                        .filter(|r| !r.is_empty())
                        .unwrap_or("codex");
                    match transcript::unwrap_strict(&committed)
                        .ok()
                        .and_then(|t| crate::adapter::get(src).ok().and_then(|a| a.parse(&t).ok()))
                    {
                        Some(ir) => crate::domain::turn::chain_of(&ir).len(),
                        None => 0,
                    }
                }
            }
        };
        (0, text.clone(), settled as u32)
    };

    if file_line {
        // A file line takes file commits only; any transcript input is refused.
        if !region.trim().is_empty() && opts.message.is_none() {
            ui::error(&format!(
                "`{branch}` is the file line — it takes `agit commit -m` (pure files), not transcript turns."
            ));
            ui::hint("start a session: `agit new -b <name>` (inherits this line’s memory/skills)");
            return Ok(ExitCode::Precondition);
        }
    }

    // Parse the settlement region. A newborn session line's meta may not carry a runtime yet
    // (import knows only what the link says), so an empty value falls back to inference instead
    // of looking up an adapter by the empty string.
    let runtime = if materialized_mode {
        // A slow-path resume may cross harnesses. The link names the runtime that is producing the
        // appended bytes; HEAD names the runtime that produced the inherited envelopes.
        lk.source.clone()
    } else {
        head_meta
            .as_ref()
            .map(|m| m.runtime.clone())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| {
                crate::adapter::infer_runtime(&region)
                    .unwrap_or(&lk.source)
                    .to_string()
            })
    };
    let ir = match crate::adapter::get(&runtime).and_then(|a| a.parse(&region)) {
        Ok(ir) => ir,
        Err(e) => {
            if !quiet {
                ui::error(&format!("failed to parse the increment: {e:#}"));
            }
            return Ok(if quiet {
                ExitCode::Ok
            } else {
                ExitCode::Precondition
            });
        }
    };
    // This is where the link's cwd is filled in.
    //
    // `import` cannot reach it (claude-code's index has no cwd field), and this is the one place
    // that makes good on the line `agit import` prints: "working dir: filled in when a version is
    // recorded". The cwd is the authority a zero-argument command resolves its context from —
    // without it, `agit commit` and `agit status` after an import cannot tell where they are.
    // Settlement is the first moment the transcript has actually been read, and the cwd the
    // transcript states is more trustworthy than the process's current directory; fall back to
    // the latter only when the transcript gives none.
    if lk.cwd.is_none() {
        lk.cwd = ir.cwd.clone().filter(|c| !c.is_empty()).or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
    }

    let open_calls = crate::adapter::get(&runtime)
        .map(|a| a.open_tool_calls(&region))
        .unwrap_or_default();
    let in_flight = in_flight_tail(&ir, &open_calls);
    let chunks = turn_chunks(&region, &ir, &open_calls);

    let new_chunks: Vec<&Chunk> = if region_start == 0 && head_turn_base == 0 {
        chunks.iter().collect()
    } else {
        // Native mode: `chunks` is the full chain, so skip what already settled. Materialized
        // mode: `head_turn_base` is the base ordinal and every chunk is new.
        if region_start == 0 {
            chunks.iter().skip(head_turn_base as usize).collect()
        } else {
            chunks.iter().collect()
        }
    };

    if new_chunks.is_empty() {
        // No new turn. Two legal moves remain: a `-m` file commit, or a no-op.
        if let Some(msg) = &opts.message {
            return file_commit(
                repo,
                slug,
                branch,
                settlement_tip.as_deref(),
                &head_meta,
                msg,
                &opts.paths,
                quiet,
            );
        }
        if !quiet {
            let tip_id = settlement_tip
                .as_deref()
                .map(|tip| tip[..10.min(tip.len())].to_string())
                .unwrap_or_default();
            println!("nothing new since {tip_id}.");
            let tail = region.rsplit('\n').find(|l| !l.is_empty()).unwrap_or("");
            if !tail.is_empty() && serde_json::from_str::<serde_json::Value>(tail).is_err() {
                println!(
                    "{}",
                    ui::dim(
                        "  skipped a half-written trailing line (in-flight turns don’t settle)"
                    )
                );
            } else if let Some(why) = &in_flight {
                explain_in_flight(why);
            }
        }
        return Ok(ExitCode::Ok);
    }

    if opts.message.is_some() {
        ui::error("there are turns to settle — a `-m` file commit isn’t legal right now.");
        ui::hint(
            "settle turns with `agit commit` first; shared-file changes go through `agit commit -m`",
        );
        return Ok(ExitCode::Precondition);
    }

    // Slow-path resume materializes only HEAD VIEW. Preserve HEAD LOG as the immutable evidence
    // prefix and HEAD VIEW as the provenance-bearing projection prefix; the runtime baseline is
    // merely a derived copy and must never be wrapped back over either one. V0 snapshots could
    // legally carry VIEW-only surgery markers, so make those reachable while retaining the old LOG
    // bytes as an exact prefix before the first v1 snapshot is built.
    let materialized_base = if materialized_mode {
        let snapshot = head_meta.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "a materialized session cannot settle without a committed HEAD snapshot"
            )
        })?;
        let tip = settlement_tip
            .as_deref()
            .expect("materialized settlement requires a frozen branch tip");
        let log = storage::materialize_at(repo.root(), tip, meta::LOG_FILE)
            .map_err(|error| anyhow::anyhow!("cannot read committed LOG at {tip}: {error:#}"))?;
        let view = storage::materialize_at(repo.root(), tip, meta::VIEW_FILE)
            .map_err(|error| anyhow::anyhow!("cannot read committed VIEW at {tip}: {error:#}"))?;
        let log = if snapshot.layout == meta::LayoutVersion::V0 {
            storage::make_view_reachable(&log, &view)?
        } else {
            log
        };
        Some((log, view))
    } else {
        None
    };

    // Prove the metadata destination is contained in the repository before either storage bytes
    // or an optional --code side commit can be created. meta::write repeats the check at publish
    // time; this early boundary prevents a rejected meta symlink from leaving a half-upgraded tree.
    meta::ensure_write_safe(repo.root())?;

    // Root LOG/VIEW/events were ordinary user paths in v0. Prove that both the immutable source
    // tree and the real checkout leave that namespace free before the first v1 byte is written (or
    // before --code can create its side commit). V1 snapshots instead rely on storage's topology
    // checks below, which also reject symlinked event namespaces.
    if has_head
        && head_meta
            .as_ref()
            .map(|snapshot| snapshot.layout)
            .unwrap_or(meta::LayoutVersion::V0)
            == meta::LayoutVersion::V0
    {
        let tip = settlement_tip
            .as_deref()
            .expect("v0 upgrade preflight requires a frozen branch tip");
        super::plumbing::ensure_v1_upgrade_preflight(repo, tip)?;
    } else if !has_head {
        // Root LOG/VIEW/events are user-owned until the first v1 commit is published. Refuse an
        // unborn settlement before object construction rather than overwriting tracked, ignored
        // or untracked bytes while materializing the winning checkout after its CAS.
        super::plumbing::ensure_v1_namespace_available_in_worktree(repo)?;
    }

    // Test-only scheduling point for the exact historical race: another settlement may advance
    // the branch after all baseline reads but before object construction. The frozen tip remains
    // both our parent and expected-old value, so the final CAS must reject our stale proposal.
    maybe_interleave_settlement(repo, branch);

    // ── One commit per turn ──
    //
    // The identity is the one the branch already claimed; a newborn session line (between
    // `import` creating the branch and its first turn) has claimed none, and the first turn mints
    // it here on the spot — that moment is what `session_hash` exists for.
    let claim = head_meta
        .as_ref()
        .map(|m| m.session.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let cwd = lk.cwd.clone().unwrap_or_default();
            meta::session_hash(&cwd, protected_full.text.as_bytes())
        });
    let source = runtime.as_str();

    repo.git(&["config", "user.name", owner])?;
    let email = credentials::current_email().unwrap_or_else(|| format!("{owner}@agit.local"));
    repo.git(&["config", "user.email", &email])?;

    let cwd = lk.cwd.clone().unwrap_or_else(|| ".".into());
    let total = new_chunks.len();
    let mut last_sha = String::new();
    // Every branch, including an unborn one, is settled without touching the real index/worktree:
    // build every turn as an unreachable object chain first, then move the ref once with the
    // frozen old-tip expectation. In particular, two first writers both expect the ref to be
    // absent; exactly one root commit can publish and the loser cannot parent onto mutable HEAD.
    let old_head = settlement_tip;
    let mut pending_parent = old_head.clone();
    let mut landed = Vec::with_capacity(total);

    // --code: commit the code repo before the last chunk, so the anchor names the new sha.
    let mut code_anchor: Option<(String, Completeness)> = None;
    if opts.code {
        code_anchor = code_commit(&cwd, &format!("{slug}@{branch}"))?;
    }
    let cwd_state = meta::cwd_state_of(Path::new(&cwd));
    let unborn_base_tree = if old_head.is_none() {
        Some(unborn_worktree_tree(repo)?)
    } else {
        None
    };

    for (i, c) in new_chunks.iter().enumerate() {
        let turn_no = head_turn_base + 1 + i as u32;
        let absolute_end = region_start + c.end_byte;
        let prefix = &text[..absolute_end];

        let (log, view) = match &materialized_base {
            Some((base_log, base_view)) => {
                let protected_addition =
                    secret_dictionary.protect_jsonl(&region[..c.end_byte], &global_secrets)?;
                extend_materialized_snapshot(
                    base_log,
                    base_view,
                    &protected_addition.text,
                    source,
                    &claim,
                )?
            }
            None => {
                let protected_prefix = secret_dictionary.protect_jsonl(prefix, &global_secrets)?;
                let log = transcript::wrap_lines(&protected_prefix.text, source, &claim);
                let view = transcript::view_of_live(prefix, source)?;
                let protected_view = secret_dictionary.protect_jsonl(&view, &global_secrets)?;
                let view = transcript::wrap_lines(&protected_view.text, source, &claim);
                (log, view)
            }
        };

        let mut snap = Meta::new(claim.clone(), source.to_string(), cwd.clone());
        snap.kind = Kind::Turn;
        snap.turn = Some(turn_no);
        snap.baseline_bytes = Some(absolute_end as u64);
        snap.code = code_anchor
            .as_ref()
            .map(|(c, _)| c.clone())
            .or_else(|| meta::code_of(Path::new(&cwd)));
        let last = i + 1 == total;
        if last {
            snap.cwd_state = cwd_state.clone();
        }
        // Only the last turn's anchor is the workspace as of this settlement. Earlier turns in
        // the same settlement happened while the code stood somewhere else, and today's sha is
        // merely "nearby" for them — mark those Unknown rather than dressing an uncheckable
        // claim up as Exact or Partial.
        snap.completeness = if !last {
            snap.code.as_ref().map(|_| Completeness::Unknown)
        } else {
            code_anchor.as_ref().map(|(_, k)| *k).or_else(|| {
                snap.code.as_ref().map(|_| {
                    if cwd_state_is_dirty(cwd_state.as_ref()) {
                        Completeness::Partial
                    } else {
                        Completeness::Exact
                    }
                })
            })
        };
        if last {
            snap.milestone = opts.milestone.clone();
        }
        let subject = {
            let one = c.gist.split_whitespace().collect::<Vec<_>>().join(" ");
            let s: String = one.chars().take(72).collect();
            if s.is_empty() {
                format!("agit: turn #{turn_no}")
            } else {
                s
            }
        };
        let mut msg = subject.clone();
        if i + 1 == total {
            if let Some(m) = &opts.milestone {
                msg.push_str(&format!("\n\nmilestone: {m}"));
            }
            if opts.code
                && let Some((a, _)) = &code_anchor
            {
                msg.push_str(&format!("\n\ncode: {a}"));
            }
        }
        let protected_message = secret_dictionary.protect_text(&msg, &global_secrets)?;
        let snap_text = meta::to_text(&snap)?;
        if old_head.is_none() {
            let base = pending_parent
                .as_deref()
                .or(unborn_base_tree.as_deref())
                .expect("an unborn settlement has a shared-file base tree");
            let tree = unborn_session_snapshot_tree(repo, base, &log, &view, &snap_text)?;
            let parents = pending_parent
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            last_sha =
                super::plumbing::commit_tree(repo, &tree, &parents, &protected_message.text)?;
            pending_parent = Some(last_sha.clone());
        } else if let Some(parent) = pending_parent.as_deref() {
            let tree =
                super::plumbing::session_snapshot_tree(repo, parent, &log, &view, &snap_text)?;
            last_sha =
                super::plumbing::commit_tree(repo, &tree, &[parent], &protected_message.text)?;
            pending_parent = Some(last_sha.clone());
        } else {
            unreachable!("an existing branch settlement always has a frozen parent")
        }
        let protected_subject = protected_message
            .text
            .lines()
            .next()
            .unwrap_or(&subject)
            .to_owned();
        landed.push((turn_no, protected_subject, last_sha.clone()));
    }

    match (old_head.as_deref(), pending_parent.as_deref()) {
        (Some(old), Some(new)) => {
            super::plumbing::update_branch_cas_and_refresh(repo, branch, new, old, false)?;
        }
        (None, Some(new)) => {
            // `None` is not an instruction to inspect mutable HEAD: update-ref's empty old value
            // is an atomic assertion that this branch is still absent. The checkout journal is
            // durable before that CAS, so a hard stop after publication can only converge forward
            // to this immutable root on the next startup.
            super::plumbing::update_absent_branch_cas_and_refresh(repo, branch, new)?;
        }
        (_, None) => unreachable!("new_chunks is non-empty, so settlement built a commit"),
    }
    if !quiet {
        for (turn_no, subject, sha) in &landed {
            println!("#{turn_no} {} {subject}", &sha[..9.min(sha.len())]);
        }
    }

    // Settling advances the link's baseline.
    lk.agent = Some(slug.split('/').nth(1).unwrap_or(slug).to_string());
    lk.branch = Some(branch.to_string());
    let new_baseline = (region_start + new_chunks.last().map(|c| c.end_byte).unwrap_or(0)) as u64;
    if lk.baseline_bytes.is_some() {
        // Materialized mode: the baseline advances to what has settled.
        lk.baseline_bytes = Some(new_baseline);
        lk.baseline_hash = None; // the region joins history, so doctor reads the commit chain
    }
    let _ = new_baseline;
    // The same lock (`link::lock`) as import's claim/rollback critical section, plus a CAS:
    // write only while the disk still holds what it held when settlement started — a claim
    // rerouted mid-settlement must not be flattened by a whole-file write of the old watermark.
    // The turns themselves already landed on the branch under an expected-old CAS, so skipping
    // the watermark advance costs at most a rescan of the already-committed prefix next time.
    let _guard = link::lock(store, &lk.source, &lk.session_id)?;
    let link_disk_now = std::fs::read(link::link_path(store, &lk.source, &lk.session_id)).ok();
    // No link on disk = no claim to flatten, so a first settlement persists as usual.
    if link_disk_now.is_none() || link_disk_now == link_disk_at_entry {
        link::write(store, &lk)?;
    } else if !quiet {
        ui::warning(
            "the session link was re-routed while this settlement ran — leaving the new claim in place",
        );
    }

    // --tag goes on the last chunk. It is a user tag; the PRD has no per-commit machine tag.
    if let Some(t) = &opts.tag {
        repo.git(&["tag", t, &last_sha])?;
        if !quiet {
            ui::success(&format!("tagged {t}"));
        }
    }

    if !quiet {
        println!(
            "\n{} settled {} turns → {}",
            ui::ok(ui::theme::symbols().check),
            total,
            ui::bold(&format!("{slug} @ {branch}"))
        );
        if let Some(why) = &in_flight {
            explain_in_flight(why);
        }
        if protected_full.replacements > 0 {
            println!(
                "{}",
                ui::dim(&format!(
                    "  protected {} secret occurrence(s) with {} new repository-local key(s)",
                    protected_full.replacements, protected_full.new_records
                ))
            );
        }
        // Say it out loud rather than in dim text: this settlement contains a
        // finding that no local key can reverse, so `agit push` will refuse it
        // and the user needs to act on the content itself.
        if protected_full.intact > 0 {
            ui::warning(&format!(
                "{} secret finding(s) exceeded the reversible record limit and stayed in the clear",
                protected_full.intact
            ));
            ui::hint(
                "the repo-wide push gate will reject them; remove the value from the session content, or shorten it so it can be protected",
            );
        }
        if fresh {
            println!(
                "{}",
                ui::dim(&format!(
                    "  created the local repo for {slug}; the first `agit push` will create the remote"
                ))
            );
        }
        println!(
            "{}",
            ui::dim("  next: agit push to publish · agit log to read history")
        );
    }
    record_supervisor_result(&last_sha)?;
    Ok(ExitCode::Ok)
}

/// A `-m` file commit: it touches shared files only (the three session files are excluded), and
/// the message is required.
#[allow(clippy::too_many_arguments)]
fn file_commit(
    repo: &Repo,
    slug: &str,
    branch: &str,
    expected_tip: Option<&str>,
    head_meta: &Option<Meta>,
    msg: &str,
    paths: &[String],
    quiet: bool,
) -> CmdResult {
    // Validate literal pathspecs before any normalization or staging. Ancestor pathspecs are
    // allowed and are sanitized after `git add`, just as ordinary `git commit` would stage them.
    let layout = head_meta
        .as_ref()
        .map(|snapshot| snapshot.layout)
        .unwrap_or(meta::LayoutVersion::V0);
    for path in paths {
        if meta::is_storage_path_for(layout, path) {
            ui::error(&format!(
                "`{path}` is managed by the AgentGit storage format and cannot be included in a file commit."
            ));
            return Ok(ExitCode::Precondition);
        }
    }

    // This must precede every `git add`: a symlinked session/meta.json is corruption, not a reason
    // to stage shared files and fail only after the index has already changed.
    meta::ensure_write_safe(repo.root())?;
    let original = FileCommitState::capture(repo)?;
    let result = file_commit_inner(
        repo,
        slug,
        branch,
        expected_tip,
        head_meta,
        msg,
        paths,
        quiet,
        layout,
    );
    match result {
        Ok(FileCommitOutcome::Published) => Ok(ExitCode::Ok),
        Ok(FileCommitOutcome::Noop) => {
            original.restore()?;
            Ok(ExitCode::Ok)
        }
        Err(error) => match original.restore() {
            Ok(()) => Err(error),
            Err(rollback) => anyhow::bail!(
                "file commit failed ({error:#}) and restoring its original index/managed files also failed ({rollback:#})"
            ),
        },
    }
}

fn file_commit_git_path(repo: &Repo, name: &str) -> crate::Result<std::path::PathBuf> {
    let value = repo.git(&["rev-parse", "--git-path", name])?;
    anyhow::ensure!(!value.is_empty(), "git returned an empty path for {name}");
    let path = std::path::PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        repo.root().join(path)
    })
}

/// Run one native Git hook against the real checkout/index.
///
/// Resolve through `rev-parse --git-path` so `core.hooksPath` works, then use Git's portable shell
/// alias dispatcher to execute the path. That dispatcher handles native binaries and shebang
/// scripts on Git for Windows too, while remaining available at the project's Git 2.28 floor
/// (`git hook run` only arrived in 2.36). A `-m` commit never opens an editor, so Git documents
/// `GIT_EDITOR=:` for its hooks; expose the same repository/index environment and cwd.
fn run_file_commit_hook(repo: &Repo, name: &str, args: &[&std::ffi::OsStr]) -> crate::Result<()> {
    let path = file_commit_git_path(repo, &format!("hooks/{name}"))?;
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        // Git explicitly documents `core.hooksPath=/dev/null` as the way to disable hooks. The
        // resolved candidate is then `/dev/null/<hook>`, whose lookup fails with ENOTDIR rather
        // than ENOENT. Both mean that this hook is absent; permission and I/O failures still fail
        // closed because Git would not treat those as an intentional disablement.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot inspect {name} hook {}: {error}",
                path.display()
            ));
        }
    };
    anyhow::ensure!(
        metadata.is_file(),
        "{name} hook {} is not a regular file",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Ok(());
        }
    }
    let index = file_commit_git_path(repo, "index")?;
    let git_dir = repo.git(&["rev-parse", "--absolute-git-dir"])?;
    let mut command = std::process::Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-c")
        .arg(r#"alias.agit-run-hook=!f() { hook="$1"; shift; "$hook" "$@"; }; f"#)
        .arg("-C")
        .arg(repo.root())
        .arg("agit-run-hook")
        .arg(&path)
        .args(args)
        .current_dir(repo.root())
        .env("GIT_DIR", git_dir)
        .env("GIT_WORK_TREE", repo.root())
        .env("GIT_INDEX_FILE", index)
        .env("GIT_PREFIX", "")
        .env("GIT_EDITOR", ":");
    let status = command
        .status()
        .map_err(|error| anyhow::anyhow!("cannot run {name} hook {}: {error}", path.display()))?;
    anyhow::ensure!(
        status.success(),
        "{name} hook declined the file commit ({status})"
    );
    Ok(())
}

fn file_commit_message_path(repo: &Repo) -> crate::Result<std::path::PathBuf> {
    file_commit_git_path(repo, "COMMIT_EDITMSG")
}

#[derive(Clone, Copy)]
enum FileCommitCleanup {
    Strip { comment_char: Option<char> },
    Whitespace,
    Verbatim,
}

/// Resolve Git's configured cleanup mode before any hook runs.
///
/// A file commit is authored with `-m` semantics and never opens an editor. Git therefore maps an
/// unset or explicit `default` mode to `whitespace`; `scissors` also only cuts when the message is
/// edited, so it has whitespace behavior here. Read through `git config` rather than parsing files
/// so includes, scope precedence and command-environment overrides stay native.
fn file_commit_cleanup(repo: &Repo, initial_message: &str) -> crate::Result<FileCommitCleanup> {
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args(["config", "--null", "--get", "commit.cleanup"])
        .output()?;
    if output.status.code() == Some(1) {
        return Ok(FileCommitCleanup::Whitespace);
    }
    if !output.status.success() {
        anyhow::bail!(
            "git config --get commit.cleanup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    anyhow::ensure!(
        output.stdout.last() == Some(&0)
            && output.stdout[..output.stdout.len() - 1]
                .iter()
                .all(|byte| *byte != 0),
        "git config returned a malformed commit.cleanup value"
    );
    let value = std::str::from_utf8(&output.stdout[..output.stdout.len() - 1])
        .map_err(|_| anyhow::anyhow!("commit.cleanup is not valid UTF-8"))?;
    match value {
        "strip" => Ok(FileCommitCleanup::Strip {
            comment_char: file_commit_uses_auto_comment_char(repo)?
                .then(|| auto_file_commit_comment_char(initial_message))
                .transpose()?,
        }),
        "verbatim" => Ok(FileCommitCleanup::Verbatim),
        "default" | "whitespace" | "scissors" => Ok(FileCommitCleanup::Whitespace),
        _ => anyhow::bail!("invalid commit.cleanup mode {value:?}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FileCommitGitVersion {
    major: u32,
    minor: u32,
}

impl FileCommitGitVersion {
    const COMMENT_STRING_ALIAS: Self = Self {
        major: 2,
        minor: 45,
    };
}

fn parse_file_commit_git_version(output: &[u8]) -> crate::Result<FileCommitGitVersion> {
    let output = std::str::from_utf8(output)
        .map_err(|_| anyhow::anyhow!("git --version output is not valid UTF-8"))?;
    let version = output
        .trim()
        .strip_prefix("git version ")
        .and_then(|rest| rest.split_ascii_whitespace().next())
        .ok_or_else(|| anyhow::anyhow!("malformed git --version output {output:?}"))?;
    let mut components = version.split('.');
    let major = components
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("malformed git version {version:?}"))?;
    let minor = components
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("malformed git version {version:?}"))?;
    Ok(FileCommitGitVersion { major, minor })
}

fn file_commit_git_version() -> crate::Result<FileCommitGitVersion> {
    let output = std::process::Command::new("git")
        .arg("--version")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git --version failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_file_commit_git_version(&output.stdout)
}

fn auto_comment_char_from_config(
    version: FileCommitGitVersion,
    config: &[u8],
) -> crate::Result<bool> {
    if config.is_empty() {
        return Ok(false);
    }
    anyhow::ensure!(
        config.last() == Some(&0),
        "git config returned malformed comment configuration"
    );
    let mut effective = None;
    for record in config[..config.len() - 1].split(|byte| *byte == 0) {
        let Some(separator) = record.iter().position(|byte| *byte == b'\n') else {
            anyhow::bail!("git config returned malformed comment configuration");
        };
        let (name, value) = record.split_at(separator);
        let value = &value[1..];
        match name {
            b"core.commentchar" => effective = Some(value),
            b"core.commentstring" if version >= FileCommitGitVersion::COMMENT_STRING_ALIAS => {
                effective = Some(value);
            }
            b"core.commentstring" => {}
            _ => anyhow::bail!("git config returned unexpected comment key"),
        }
    }
    Ok(effective.is_some_and(|value| value.eq_ignore_ascii_case(b"auto")))
}

fn file_commit_uses_auto_comment_char(repo: &Repo) -> crate::Result<bool> {
    let version = file_commit_git_version()?;
    let output = std::process::Command::new("git")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo.root())
        .args([
            "config",
            "--null",
            "--get-regexp",
            r"^core\.(commentchar|commentstring)$",
        ])
        .output()?;
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    if !output.status.success() {
        anyhow::bail!(
            "git config --get-regexp for comment configuration failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    auto_comment_char_from_config(version, &output.stdout)
}

/// Match Git's `core.commentChar=auto` candidate order and timing: select from the initial authored
/// message before prepare/commit hooks edit `COMMIT_EDITMSG`.
fn auto_file_commit_comment_char(message: &str) -> crate::Result<char> {
    const CANDIDATES: &[u8] = b"#;@!$%^&|:";
    let mut available = [true; CANDIDATES.len()];
    let bytes = message.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if index != 0 && !matches!(bytes[index - 1], b'\n' | b'\r') {
            continue;
        }
        if let Some(candidate) = CANDIDATES.iter().position(|candidate| *candidate == byte) {
            available[candidate] = false;
        }
    }
    CANDIDATES
        .iter()
        .zip(available)
        .find_map(|(candidate, available)| available.then_some(char::from(*candidate)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unable to select a comment character that is not used in the commit message"
            )
        })
}

fn cleanup_file_commit_message(
    repo: &Repo,
    edited: String,
    cleanup: FileCommitCleanup,
) -> crate::Result<String> {
    let cleaned = match cleanup {
        FileCommitCleanup::Strip { comment_char } => {
            if let Some(comment_char) = comment_char {
                let config = format!("core.commentChar={comment_char}");
                super::plumbing::raw_git(
                    repo,
                    &["-c", &config, "stripspace", "--strip-comments"],
                    Some(&edited),
                )?
            } else {
                super::plumbing::raw_git(repo, &["stripspace", "--strip-comments"], Some(&edited))?
            }
        }
        FileCommitCleanup::Whitespace => {
            super::plumbing::raw_git(repo, &["stripspace"], Some(&edited))?
        }
        FileCommitCleanup::Verbatim => edited,
    };
    // Native Git accepts a whitespace-only message in verbatim mode, but still rejects a truly
    // zero-byte message. Other modes have already reduced whitespace-only input to zero bytes.
    anyhow::ensure!(
        !cleaned.is_empty(),
        "file commit message became empty after hooks and cleanup"
    );
    Ok(cleaned)
}

/// Mirror the default authored-message path of `git commit -m`: pre-commit sees the prepared real
/// index, prepare-commit-msg and commit-msg share COMMIT_EDITMSG, and the final hook-edited bytes
/// get the configured non-editor cleanup before `commit-tree` consumes them.
fn prepare_file_commit_message(repo: &Repo, message: &str) -> crate::Result<String> {
    let initial = format!("{message}\n");
    let cleanup = file_commit_cleanup(repo, &initial)?;
    run_file_commit_hook(repo, "pre-commit", &[])?;
    let path = file_commit_message_path(repo)?;
    std::fs::write(&path, initial.as_bytes())
        .map_err(|error| anyhow::anyhow!("cannot write {}: {error}", path.display()))?;
    run_file_commit_hook(
        repo,
        "prepare-commit-msg",
        &[path.as_os_str(), std::ffi::OsStr::new("message")],
    )?;
    run_file_commit_hook(repo, "commit-msg", &[path.as_os_str()])?;
    let edited = std::fs::read_to_string(&path)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    cleanup_file_commit_message(repo, edited, cleanup)
}

/// Create a file-commit object without moving a ref while preserving the cleaned message bytes.
///
/// `commit-tree -m` discards trailing blank lines even after `commit.cleanup=verbatim`; `-F -`
/// retains the exact COMMIT_EDITMSG payload, matching native `git commit` for every cleanup mode.
fn commit_file_commit_tree(
    repo: &Repo,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> crate::Result<String> {
    repo.ensure_committer()?;
    let mut args = vec!["commit-tree", tree];
    for parent in parents {
        args.push("-p");
        args.push(parent);
    }
    args.extend(["-F", "-"]);
    let commit = super::plumbing::raw_git(repo, &args, Some(message))?;
    let commit = commit.trim();
    anyhow::ensure!(
        !commit.is_empty() && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "git commit-tree returned an invalid object id"
    );
    Ok(commit.to_owned())
}

struct FileCommitTreeEntry {
    mode: String,
    kind: String,
    oid: String,
}

fn file_commit_tree_entry(
    repo: &Repo,
    tree: &str,
    path: &str,
) -> crate::Result<Option<FileCommitTreeEntry>> {
    let output = repo.git_bytes_result(&["ls-tree", "-z", "--full-name", tree, "--", path])?;
    if output.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        output.last() == Some(&0) && output[..output.len() - 1].iter().all(|byte| *byte != 0),
        "git returned multiple or unterminated tree entries for {path}"
    );
    let record = &output[..output.len() - 1];
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| anyhow::anyhow!("git returned a malformed tree entry for {path}"))?;
    anyhow::ensure!(
        &record[tab + 1..] == path.as_bytes(),
        "git returned the wrong tree path for {path}"
    );
    let header = std::str::from_utf8(&record[..tab])
        .map_err(|_| anyhow::anyhow!("git returned a non-UTF-8 tree entry for {path}"))?;
    let mut fields = header.split_ascii_whitespace();
    let mode = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("git tree entry for {path} omitted its mode"))?;
    let kind = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("git tree entry for {path} omitted its type"))?;
    let oid = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("git tree entry for {path} omitted its object id"))?;
    anyhow::ensure!(
        fields.next().is_none(),
        "git tree entry for {path} has extra fields"
    );
    Ok(Some(FileCommitTreeEntry {
        mode: mode.into(),
        kind: kind.into(),
        oid: oid.into(),
    }))
}

fn file_commit_tree_paths(repo: &Repo, base: &str, tree: &str) -> crate::Result<Vec<String>> {
    let output = repo.git_bytes_result(&[
        "diff-tree",
        "-r",
        "--no-renames",
        "--name-only",
        "-z",
        base,
        tree,
        "--",
    ])?;
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(str::to_owned)
                .map_err(|_| anyhow::anyhow!("file-commit tree path is not valid UTF-8"))
        })
        .collect()
}

fn normalize_staged_file_commit_attributes(repo: &Repo, base: &str) -> crate::Result<()> {
    if !staged_paths(repo, base)?
        .iter()
        .any(|path| path == meta::ATTRS_FILE)
    {
        return Ok(());
    }
    let output = repo.git_bytes_result(&["ls-files", "--stage", "-z", "--", meta::ATTRS_FILE])?;
    let text = if output.is_empty() {
        None
    } else {
        anyhow::ensure!(
            output.last() == Some(&0) && output[..output.len() - 1].iter().all(|byte| *byte != 0),
            "staged .gitattributes has multiple or unterminated index entries"
        );
        let record = &output[..output.len() - 1];
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| anyhow::anyhow!("staged .gitattributes index entry is malformed"))?;
        anyhow::ensure!(
            &record[tab + 1..] == meta::ATTRS_FILE.as_bytes(),
            "git returned the wrong staged attributes path"
        );
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|_| anyhow::anyhow!("staged .gitattributes index entry is not UTF-8"))?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("staged .gitattributes omitted its mode"))?;
        let oid = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("staged .gitattributes omitted its object id"))?;
        let stage = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("staged .gitattributes omitted its stage"))?;
        anyhow::ensure!(
            fields.next().is_none(),
            "staged .gitattributes index entry has extra fields"
        );
        anyhow::ensure!(
            mode == "100644" && stage == "0",
            "refusing staged .gitattributes mode {mode} stage {stage}"
        );
        let bytes = repo.git_bytes_result(&["cat-file", "blob", oid])?;
        Some(
            String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("staged .gitattributes is not UTF-8"))?,
        )
    };
    let normalized = storage::attributes_text_strict(text.as_deref())?;
    let oid = super::plumbing::raw_git(
        repo,
        &["hash-object", "-w", "--no-filters", "--stdin"],
        Some(&normalized),
    )?;
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{},{}", oid.trim(), meta::ATTRS_FILE),
    ])?;
    Ok(())
}

fn validate_file_commit_tree(
    repo: &Repo,
    base: &str,
    tree: &str,
    layout: meta::LayoutVersion,
    expected_meta: Option<&[u8]>,
) -> crate::Result<()> {
    let changed = file_commit_tree_paths(repo, base, tree)?;
    let leaked: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|path| {
            meta::is_storage_path_for(layout, path)
                && !(expected_meta.is_some() && *path == meta::FILE)
        })
        .collect();
    anyhow::ensure!(
        leaked.is_empty(),
        "file commit hook staged managed storage paths: {}",
        leaked.join(", ")
    );
    anyhow::ensure!(
        changed.iter().any(|path| path != meta::FILE),
        "file commit hooks left no shared-file changes staged"
    );

    let actual_meta = file_commit_tree_entry(repo, tree, meta::FILE)?;
    match (expected_meta, actual_meta) {
        (Some(expected), Some(entry)) => {
            anyhow::ensure!(
                entry.mode == "100644" && entry.kind == "blob",
                "file commit hook changed staged {} to mode {} type {}",
                meta::FILE,
                entry.mode,
                entry.kind
            );
            let actual = repo.git_bytes_result(&["cat-file", "blob", &entry.oid])?;
            anyhow::ensure!(
                actual == expected,
                "file commit hook changed the staged AgentGit metadata"
            );
        }
        (Some(_), None) => anyhow::bail!("file commit hook removed the staged AgentGit metadata"),
        (None, Some(_)) => {
            anyhow::bail!("file commit hook introduced AgentGit metadata on an unborn branch")
        }
        (None, None) => {}
    }

    if layout == meta::LayoutVersion::V1 {
        let entry = file_commit_tree_entry(repo, tree, meta::ATTRS_FILE)?
            .ok_or_else(|| anyhow::anyhow!("file commit tree has no v1 .gitattributes"))?;
        anyhow::ensure!(
            entry.mode == "100644" && entry.kind == "blob",
            "file commit hook changed staged {} to mode {} type {}",
            meta::ATTRS_FILE,
            entry.mode,
            entry.kind
        );
        let bytes = repo.git_bytes_result(&["cat-file", "blob", &entry.oid])?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("staged .gitattributes is not UTF-8"))?;
        let normalized = storage::attributes_text_strict(Some(text))?;
        anyhow::ensure!(
            normalized.as_bytes() == bytes,
            "file commit hook staged non-canonical v1 .gitattributes"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn file_commit_inner(
    repo: &Repo,
    slug: &str,
    branch: &str,
    expected_tip: Option<&str>,
    head_meta: &Option<Meta>,
    msg: &str,
    paths: &[String],
    quiet: bool,
    layout: meta::LayoutVersion,
) -> crate::Result<FileCommitOutcome> {
    // V1 owns only the marked AgentGit blocks, not the whole shared attributes file. Normalize
    // those blocks before any git add so user rules can be committed while malformed/symlinked
    // storage policy still fails without changing the index.
    if layout == meta::LayoutVersion::V1 {
        storage::ensure_attributes(repo.root())?;
    }
    if paths.is_empty() {
        repo.git(&["add", "-A", "--", "."])?;
    } else {
        for p in paths {
            repo.git(&["add", "--", p])?;
        }
    }
    // Pathspecs such as `.` or `session` can include managed descendants even though the literal
    // argument itself is not a storage path. Always sanitize the index after every add, and fail if
    // Git cannot prove those paths were removed from the file-commit payload.
    let managed: &[&str] = match layout {
        meta::LayoutVersion::V0 => &[meta::FILE, meta::LEGACY_LOG_FILE, meta::LEGACY_VIEW_FILE],
        meta::LayoutVersion::V1 => &[
            meta::LOG_FILE,
            meta::VIEW_FILE,
            meta::EVENTS_DIR,
            meta::FILE,
            meta::LEGACY_LOG_FILE,
            meta::LEGACY_VIEW_FILE,
        ],
    };
    // Every committed-state comparison is against the immutable tip frozen by settle_bytes. An
    // unborn branch uses Git's own empty-tree object rather than consulting mutable HEAD.
    let empty_tree;
    let base = match expected_tip {
        Some(tip) => tip,
        None => {
            empty_tree = repo
                .git(&["hash-object", "-w", "-t", "tree", "--stdin"])?
                .trim()
                .to_owned();
            empty_tree.as_str()
        }
    };
    let mut reset = vec!["reset", "-q", base, "--"];
    reset.extend_from_slice(managed);
    repo.git(&reset)?;
    if layout == meta::LayoutVersion::V1 {
        normalize_staged_file_commit_attributes(repo, base)?;
    }
    let staged = staged_paths(repo, base)?;
    let leaked: Vec<&str> = staged
        .iter()
        .map(String::as_str)
        .filter(|path| meta::is_storage_path_for(layout, path))
        .collect();
    anyhow::ensure!(
        leaked.is_empty(),
        "file commit still contains managed storage paths after index sanitization: {}",
        leaked.join(", ")
    );
    if staged.is_empty() {
        if !quiet {
            println!(
                "nothing staged (changes are all on the exclusion list). `agit commit` settles turns."
            );
        }
        return Ok(FileCommitOutcome::Noop);
    }
    // A file commit's meta keeps HEAD's form and identity fields, sets kind=File, and leaves
    // turn alone. **The form does not move** — a file commit landing on a session line does not
    // turn it into a file line (both forms are fixed at birth and never convert).
    let expected_meta = if let Some(tip) = head_meta {
        let mut m = tip.clone();
        m.kind = Kind::File;
        let expected = meta::to_text(&m)?.into_bytes();
        meta::write(repo.root(), &m)?;
        repo.git(&["add", "--", meta::FILE])?;
        Some(expected)
    } else {
        None
    };
    let staged = staged_paths(repo, base)?;
    let leaked: Vec<&str> = staged
        .iter()
        .map(String::as_str)
        .filter(|path| meta::is_storage_path_for(layout, path) && *path != meta::FILE)
        .collect();
    anyhow::ensure!(
        leaked.is_empty(),
        "file commit contains managed storage paths: {}",
        leaked.join(", ")
    );

    let message = prepare_file_commit_message(repo, msg)?;
    // Hooks may update ordinary staged files just like native `git commit`. Re-normalize the two
    // managed worktree files after they return, but do not implicitly stage those repairs: the
    // hook's real-index choices remain authoritative and are validated in the frozen tree below.
    meta::ensure_write_safe(repo.root())?;
    if let Some(expected) = expected_meta.as_deref() {
        std::fs::write(repo.root().join(meta::FILE), expected)?;
    }
    if layout == meta::LayoutVersion::V1 {
        storage::ensure_attributes(repo.root())?;
        normalize_staged_file_commit_attributes(repo, base)?;
    }

    // Keep Git's normal staging semantics: the real index (including pre-existing partial staging)
    // is the source of the tree. Freeze it once, then validate that exact immutable object. This
    // closes the validation/write-tree gap in which a concurrent `git add` could otherwise smuggle
    // a managed path or mode into the commit after the checks had passed.
    let tree = repo.git(&["write-tree"])?;
    validate_file_commit_tree(repo, base, tree.trim(), layout, expected_meta.as_deref())?;

    // Build an unreachable commit with the frozen tip as its only parent, then publish it with
    // expected-old CAS. `git commit` cannot express that final CAS and may otherwise silently
    // parent the old metadata onto a tip advanced by a hook/concurrent process.
    let parents = expected_tip.into_iter().collect::<Vec<_>>();
    let commit = commit_file_commit_tree(repo, tree.trim(), &parents, &message)?;
    maybe_interleave_settlement(repo, branch);
    let refname = format!("refs/heads/{branch}");
    super::plumbing::update_ref_cas(repo, &refname, &commit, expected_tip)?;
    // Native Git documents post-commit as notification-only: it runs after the commit is final and
    // cannot affect the outcome. In particular, never convert its failure into a transactional
    // error after the CAS, because the outer rollback owns only the index and the two managed
    // worktree files, not a ref that may already have advanced again.
    let _ = run_file_commit_hook(repo, "post-commit", &[]);
    if !quiet {
        let subject = message.lines().next().unwrap_or(msg);
        ui::success(&format!(
            "file commit landed on {slug} @ {branch}: {subject}"
        ));
    }
    Ok(FileCommitOutcome::Published)
}

enum FileCommitOutcome {
    Noop,
    Published,
}

fn staged_paths(repo: &Repo, base: &str) -> crate::Result<Vec<String>> {
    let bytes = repo.git_bytes_result(&["diff", "--cached", "--name-only", "-z", base, "--"])?;
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(str::to_owned)
                .map_err(|_| anyhow::anyhow!("staged path is not valid UTF-8"))
        })
        .collect()
}

// This snapshots every path the file-commit implementation itself mutates. As with native Git,
// arbitrary worktree side effects created by a user hook are owned by that hook and are not
// guessed at or deleted during rollback.
struct FileCommitState {
    index: FileSnapshot,
    attributes: FileSnapshot,
    meta: FileSnapshot,
}

impl FileCommitState {
    fn capture(repo: &Repo) -> crate::Result<Self> {
        let git_index = repo.git(&["rev-parse", "--git-path", "index"])?;
        let git_index = std::path::PathBuf::from(git_index.trim());
        let git_index = if git_index.is_absolute() {
            git_index
        } else {
            repo.root().join(git_index)
        };
        Ok(Self {
            index: FileSnapshot::capture(git_index)?,
            attributes: FileSnapshot::capture(repo.root().join(meta::ATTRS_FILE))?,
            meta: FileSnapshot::capture(repo.root().join(meta::FILE))?,
        })
    }

    fn restore(&self) -> crate::Result<()> {
        // Restore worktree bytes before the index: if either path cannot be restored, leaving the
        // exact original index in place would falsely describe a worktree state that no longer
        // exists. A successful rollback restores all three byte layers.
        self.attributes.restore()?;
        self.meta.restore()?;
        self.index.restore()?;
        Ok(())
    }
}

enum FileSnapshotBody {
    Missing,
    Regular {
        bytes: Vec<u8>,
        permissions: std::fs::Permissions,
    },
    // Storage preflights reject these before changing them. Retain the shape so rollback can
    // verify that the rejected operation did not replace the user's path behind our back.
    NonRegular,
}

struct FileSnapshot {
    path: std::path::PathBuf,
    body: FileSnapshotBody,
}

impl FileSnapshot {
    fn capture(path: std::path::PathBuf) -> crate::Result<Self> {
        let body = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => FileSnapshotBody::Regular {
                bytes: std::fs::read(&path)?,
                permissions: metadata.permissions(),
            },
            Ok(_) => FileSnapshotBody::NonRegular,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileSnapshotBody::Missing,
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, body })
    }

    fn restore(&self) -> crate::Result<()> {
        match &self.body {
            FileSnapshotBody::Missing => match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    std::fs::remove_file(&self.path)?;
                    Ok(())
                }
                Ok(_) => anyhow::bail!(
                    "cannot restore missing path {} because it became non-regular",
                    self.path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
            FileSnapshotBody::Regular { bytes, permissions } => {
                match std::fs::symlink_metadata(&self.path) {
                    Ok(metadata) if !metadata.file_type().is_file() => anyhow::bail!(
                        "cannot restore regular file {} because it became non-regular",
                        self.path.display()
                    ),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                let parent = self.path.parent().ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot restore path without a parent: {}",
                        self.path.display()
                    )
                })?;
                std::fs::create_dir_all(parent)?;
                let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                {
                    use std::io::Write as _;
                    temporary.write_all(bytes)?;
                }
                temporary.as_file().sync_all()?;
                std::fs::set_permissions(temporary.path(), permissions.clone())?;
                temporary.persist(&self.path).map_err(|error| error.error)?;
                Ok(())
            }
            FileSnapshotBody::NonRegular => match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) if !metadata.file_type().is_file() => Ok(()),
                Ok(_) => anyhow::bail!(
                    "cannot restore non-regular path {} because it became a regular file",
                    self.path.display()
                ),
                Err(error) => Err(error.into()),
            },
        }
    }
}

/// `--code`: make one commit in the cwd's code repo and return the anchor (exact).
///
/// `Ok(None)` means a safety gate refused the automatic commit **on the code side**; the session
/// settlement still proceeds and records partial for a dirty worktree. Turning this expected
/// degradation into an `Err` would throw away the transcript as well, while the warning said only
/// "skipping the automatic commit".
fn code_commit(cwd: &str, what: &str) -> crate::Result<Option<(String, Completeness)>> {
    let p = Path::new(cwd);
    let is_repo = meta::code_of(p).is_some();
    if !is_repo {
        ui::warning("--code: cwd isn’t a git repo — skipping the code commit and anchor");
        return Ok(None);
    }
    // **The filter check comes before any git command that touches the worktree.**
    //
    // Standing in front of `git add -A` is not enough: ahead of it sits a `git status
    // --porcelain`, and `status` **runs the clean filter** while working out which paths changed,
    // so that arbitrary command has already run before the gate comes down.
    //
    // `GIT_SAFE` turns off hooks and fsmonitor, but a filter cannot be turned off: git reads the
    // worktree's `.gitattributes`, takes the name it assigns a file, looks up
    // `filter.<name>.clean` in the repository config, and runs it. The agent can write both of
    // those — so "record the changes" becomes "run an arbitrary command for the agent, with no
    // approval". Git has no `--no-filters` (`-c filter.X.clean=` has to name each filter, and the
    // names are chosen by the other side), so the answer on sight is to not do it.
    //
    // `git config --get-regexp` only reads config and never touches the worktree, so it is itself
    // safe.
    if let Some(names) = meta::configured_clean_filters(p) {
        ui::warning(&format!(
            "--code: this repo configures a git clean/smudge filter ({names}) — skipping the automatic commit"
        ));
        println!(
            "{}",
            ui::dim(
                "  a filter is an arbitrary command git runs while staging, and the agent can write both the filter and .gitattributes"
            )
        );
        println!(
            "{}",
            ui::dim("  commit it yourself if that filter is one you set up")
        );
        return Ok(None);
    }

    let dirty = code_repo_dirty(p);
    if dirty {
        let msg = format!("work: {what} (agit commit --code)");
        // The same reason as the adjacent `code_repo_dirty`, and these two need guarding more
        // than `status` does: `add` runs `core.fsmonitor` and `commit` runs hooks (`pre-commit` /
        // `commit-msg` / `prepare-commit-msg`) — and this function runs **inside** the repo the
        // agent has just been writing to. One line written into `.git/hooks/pre-commit` and the
        // next `agit commit --code` executes it for the agent, with no approval at all. See
        // `meta::GIT_SAFE`.
        let out = std::process::Command::new("git")
            .args(crate::domain::meta::GIT_SAFE)
            .args(["add", "-A"])
            .current_dir(p)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git add -A failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let out = std::process::Command::new("git")
            .args(crate::domain::meta::GIT_SAFE)
            .args(["commit", "-m", &msg])
            .current_dir(p)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let sha = meta::code_of(p).expect("just verified it’s a repo");
    Ok(Some((sha, Completeness::Exact)))
}

fn cwd_state_is_dirty(state: Option<&meta::CwdState>) -> bool {
    match state.map(|state| state.worktree) {
        Some(meta::WorktreeStatus::Clean) | None => false,
        Some(meta::WorktreeStatus::Dirty)
        | Some(meta::WorktreeStatus::Conflicted)
        | Some(meta::WorktreeStatus::Unknown) => true,
    }
}

fn code_repo_dirty(cwd: &Path) -> bool {
    // **The filter gate belongs in the status check itself.**
    //
    // `git status --porcelain` **runs the clean filter** while working out which paths changed.
    // Keeping the check inside the status decision keeps a future call site from going around the
    // filter gate.
    //
    // A configured filter counts as "dirty": that is the conservative answer to "cannot tell",
    // and `code_commit` refuses the automatic commit outright further up.
    if meta::configured_clean_filters(cwd).is_some() {
        return true;
    }
    // This runs inside **the user's own code repo**, which the agent has just written to.
    // `git status` executes `core.fsmonitor` — see `meta::GIT_SAFE`.
    std::process::Command::new("git")
        .args(crate::domain::meta::GIT_SAFE)
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

// ───────────────────── Legacy-form lookup (compat) ──────────────────────

enum Located {
    Found(Link),
    Explained(ExitCode),
}

fn locate(store: &Store, target: Option<&str>) -> crate::Result<Located> {
    let all = link::list(store);
    if all.is_empty() {
        ui::error("no sessions adopted yet.");
        ui::hint("adopt one with `agit import` and record the first version");
        return Ok(Located::Explained(ExitCode::Precondition));
    }
    let Some(sel) = target else {
        if let [only] = all.as_slice() {
            return Ok(Located::Found(only.clone()));
        }
        ui::error(&format!(
            "{} sessions adopted — say which one to settle:",
            all.len()
        ));
        return Ok(Located::Explained(ExitCode::Interactive));
    };
    let by_agent: Vec<Link> = all
        .iter()
        .filter(|l| l.agent.as_deref() == Some(sel))
        .cloned()
        .collect();
    let by_session: Vec<Link> = all
        .iter()
        .filter(|l| l.session_id.starts_with(sel))
        .cloned()
        .collect();
    if !by_agent.is_empty() && !by_session.is_empty() {
        ui::error(&format!(
            "`{sel}` is both an agent name and a session id prefix — ambiguous."
        ));
        return Ok(Located::Explained(ExitCode::Ref));
    }
    if let [only] = by_agent.as_slice() {
        return Ok(Located::Found(only.clone()));
    }
    match by_session.len() {
        1 => Ok(Located::Found(by_session.into_iter().next().unwrap())),
        0 => {
            ui::error(&format!(
                "no agent named `{sel}`, and no session id starts with it."
            ));
            Ok(Located::Explained(ExitCode::Ref))
        }
        n => {
            ui::error(&format!("`{sel}` matches {n} sessions."));
            Ok(Located::Explained(ExitCode::Ref))
        }
    }
}

/// The legacy call site of `agit import`: record the initial version (now = per-turn settlement).
/// `owner` is the namespace the repo lives in (one's own name or the org it belongs to) and
/// `author` is the signed-in account: the version is recorded in the former's repo and signed with
/// the latter's name. When the link records a namespace, that is the directory — no guessing by
/// name.
pub fn record(store: &Store, lk: Link, agent: &str, owner: &str, author: &str) -> CmdResult {
    let repo_dir = if lk.owner.is_some() {
        crate::infra::config::repo_dir(owner, agent)?
    } else {
        super::clone::checkout_for_recording(owner, agent)?
    };
    let repo = Repo::open_or_init(&repo_dir)?;
    let branch = lk
        .branch
        .clone()
        .or_else(|| repo.current_branch())
        .unwrap_or_else(|| "main".into());
    let slug = format!("{owner}/{agent}");
    settle(
        store,
        &repo_dir,
        &slug,
        &branch,
        lk,
        author,
        SettleOpts {
            milestone: None,
            tag: None,
            code: false,
            message: None,
            paths: vec![],
            quiet: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::link::Link;
    use crate::domain::store::Store;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        (d, Store::at(root))
    }

    const META: &str = r#"{"type":"session_meta","payload":{"id":"AB","cwd":"/repo/one"}}"#;

    fn codex_user(t: &str) -> String {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{t}\"}}]}}}}\n"
        )
    }

    fn codex_asst(t: &str) -> String {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{t}\"}}]}}}}\n"
        )
    }

    fn opts() -> SettleOpts {
        SettleOpts {
            milestone: None,
            tag: None,
            code: false,
            message: None,
            paths: vec![],
            quiet: false,
        }
    }

    fn setup_repo() -> (tempfile::TempDir, Repo) {
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("agents/alice/photo")).unwrap();
        (d, r)
    }

    #[cfg(unix)]
    fn install_hook(repo: &Repo, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let path = file_commit_git_path(repo, &format!("hooks/{name}")).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn captured_index_bytes(state: &FileCommitState) -> Vec<u8> {
        match &state.index.body {
            FileSnapshotBody::Regular { bytes, .. } => bytes.clone(),
            _ => panic!("initialized repository must have a real index"),
        }
    }

    fn captured_index_bytes_or_missing(state: &FileCommitState) -> Option<Vec<u8>> {
        match &state.index.body {
            FileSnapshotBody::Missing => None,
            FileSnapshotBody::Regular { bytes, .. } => Some(bytes.clone()),
            FileSnapshotBody::NonRegular => panic!("git index must be regular or absent"),
        }
    }

    fn raw_commit_message(repo: &Repo, commit: &str) -> Vec<u8> {
        let object = repo
            .git_bytes_result(&["cat-file", "commit", commit])
            .unwrap();
        let body = object
            .windows(2)
            .position(|window| window == b"\n\n")
            .expect("commit object has a header terminator")
            + 2;
        object[body..].to_vec()
    }

    fn link() -> Link {
        let mut l = Link::new("codex", "AB", Some(Path::new("/repo/one")));
        l.agent = Some("photo".into());
        l.branch = Some("main".into());
        l
    }

    fn run_code_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(crate::domain::meta::GIT_SAFE)
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn a_repository_owned_filter_skips_only_the_code_side_commit() {
        let d = tempfile::tempdir().unwrap();
        run_code_git(d.path(), &["init"]);
        run_code_git(d.path(), &["config", "user.name", "Test"]);
        run_code_git(d.path(), &["config", "user.email", "test@example.com"]);
        run_code_git(
            d.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/code.git",
            ],
        );
        std::fs::write(d.path().join("seed.txt"), "seed\n").unwrap();
        run_code_git(d.path(), &["add", "seed.txt"]);
        run_code_git(d.path(), &["commit", "-m", "seed"]);
        let before = run_code_git(d.path(), &["rev-parse", "HEAD"]);

        // The agent can write the repository config in a permissive session, then rely on an
        // unattended settlement to execute it. Detection must happen before status/add touch the
        // worktree, but it must degrade only the optional code-side commit.
        run_code_git(
            d.path(),
            &["config", "filter.agent-controlled.clean", "false"],
        );
        std::fs::write(d.path().join("work.txt"), "uncommitted\n").unwrap();

        assert_eq!(
            meta::configured_clean_filters(d.path()).as_deref(),
            Some("agent-controlled")
        );
        assert!(
            code_commit(d.path().to_str().unwrap(), "test")
                .unwrap()
                .is_none()
        );
        assert_eq!(run_code_git(d.path(), &["rev-parse", "HEAD"]), before);
        assert!(d.path().join("work.txt").exists());
    }

    #[test]
    fn a_non_git_cwd_skips_only_the_optional_code_side_commit() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            code_commit(d.path().to_str().unwrap(), "test").unwrap(),
            None
        );
    }

    #[test]
    fn unknown_cwd_state_is_conservatively_partial() {
        let state = meta::CwdState {
            origin: None,
            head: None,
            branch: None,
            worktree: meta::WorktreeStatus::Unknown,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
            status_digest: None,
        };
        assert!(cwd_state_is_dirty(Some(&state)));
        assert!(!cwd_state_is_dirty(None));
        assert!(!cwd_state_is_dirty(Some(&meta::CwdState {
            worktree: meta::WorktreeStatus::Clean,
            ..state
        })));
    }

    #[test]
    fn optional_settlement_tip_requires_the_exact_branch_ref() {
        let (_d, repo) = setup_repo();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::write(repo.root().join("seed.txt"), "seed\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("seed").unwrap();
        repo.git(&["branch", "-m", "seed"]).unwrap();
        repo.git(&["branch", "main/child"]).unwrap();

        assert_eq!(
            optional_branch_commit(&repo, "refs/heads/main").unwrap(),
            None,
            "a child ref must not make its absent parent branch look born"
        );
        assert_eq!(
            optional_branch_commit(&repo, "refs/heads/main/child").unwrap(),
            Some(repo.git(&["rev-parse", "refs/heads/main/child"]).unwrap())
        );
    }

    #[test]
    fn optional_settlement_tip_propagates_a_corrupt_ref() {
        let (_d, repo) = setup_repo();
        let refs = repo.root().join(".git/refs/heads");
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(refs.join("main"), "not-an-object-id\n").unwrap();

        let error = optional_branch_commit(&repo, "refs/heads/main").unwrap_err();
        assert!(error.to_string().contains("for-each-ref"), "{error:#}");
    }

    /// Two complete turns of conversation → two turn commits, each message the first line of
    /// that turn's user prompt, with turn ordinals one and two.
    #[test]
    fn settles_one_commit_per_turn() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let text = format!(
            "{META}\n{}{}{}{}",
            codex_user("fix the refund path"),
            codex_asst("ok, reading the code first"),
            codex_user("add one more test"),
            codex_asst("test added")
        );
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        assert_eq!(repo.commit_count(), 2, "one turn is one commit");
        let subjects = repo.git(&["log", "--format=%s", "--reverse"]).unwrap();
        let lines: Vec<&str> = subjects.lines().collect();
        assert_eq!(
            lines[0], "fix the refund path",
            "the commit message is that turn's first prompt line"
        );
        assert_eq!(lines[1], "add one more test");
        // the turn ordinal goes into meta
        let snap = meta::read_at_ref(&repo, "HEAD").unwrap();
        assert_eq!(snap.turn, Some(2));
        assert_eq!(snap.kind, Kind::Turn);
        // An intermediate commit's transcript stops at the first turn.
        let first = repo.git(&["rev-parse", "HEAD~1"]).unwrap();
        let t1 = repo.show(first.trim(), meta::LOG_FILE).unwrap();
        assert!(
            !t1.contains("add one more test"),
            "the first commit does not contain the second turn"
        );
    }

    /// Only the last turn of a settlement is anchored to the workspace it was settled from;
    /// every earlier turn in the same batch gets `Unknown`. An implementation that stamps
    /// today's sha as `Exact` on all of them claims a code state those turns never saw.
    #[test]
    fn only_the_last_settled_turn_claims_an_exact_code_anchor() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let code = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(code.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@x")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@x")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/code.git",
        ]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        let mut lk = Link::new("codex", "AB", Some(code.path()));
        lk.agent = Some("photo".into());
        lk.branch = Some("main".into());
        let text = format!(
            "{META}\n{}{}{}{}",
            codex_user("fix the refund path"),
            codex_asst("ok, reading the code first"),
            codex_user("add one more test"),
            codex_asst("test added")
        );
        let exit = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk,
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(exit, ExitCode::Ok);
        assert_eq!(repo.commit_count(), 2);
        let last = meta::read_at_ref(&repo, "HEAD").unwrap();
        assert!(last.code.is_some());
        assert_eq!(last.completeness, Some(Completeness::Exact));
        let state = last.cwd_state.as_ref().expect("cwd state is captured");
        let code_head = run_code_git(code.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            state.origin.as_deref(),
            Some("https://example.invalid/code.git")
        );
        assert_eq!(state.head.as_deref(), Some(code_head.as_str()));
        assert_eq!(state.worktree, meta::WorktreeStatus::Clean);
        assert_eq!(
            (
                state.staged,
                state.unstaged,
                state.untracked,
                state.conflicted
            ),
            (0, 0, 0, 0)
        );
        let first = meta::read_at_ref(&repo, "HEAD~1").unwrap();
        assert!(first.code.is_some());
        assert_eq!(first.completeness, Some(Completeness::Unknown));
        assert!(first.cwd_state.is_none());
    }

    /// Two first writers can both freeze the same genuinely absent branch ref. The loser must
    /// publish with an expected-absent CAS, not fall back to `git commit` after the winner has made
    /// mutable HEAD exist: otherwise the same logical "turn 1" becomes a child turn on that root.
    #[test]
    fn concurrent_first_settlements_publish_exactly_one_parentless_root() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        std::fs::write(repo.root().join("memory.md"), "staged layer\n").unwrap();
        repo.git(&["add", "--", "memory.md"]).unwrap();
        std::fs::write(repo.root().join("memory.md"), "worktree layer\n").unwrap();
        std::fs::write(repo.root().join(".gitignore"), "ignored-staged.txt\n").unwrap();
        std::fs::write(
            repo.root().join("ignored-staged.txt"),
            "staged ignored layer\n",
        )
        .unwrap();
        repo.git(&["add", "--", ".gitignore"]).unwrap();
        repo.git(&["add", "-f", "--", "ignored-staged.txt"])
            .unwrap();
        std::fs::write(
            repo.root().join("ignored-staged.txt"),
            "worktree ignored layer\n",
        )
        .unwrap();
        std::fs::write(
            repo.root().join(meta::ATTRS_FILE),
            "*.user-defined binary\n",
        )
        .unwrap();

        let stale = format!(
            "{META}\n{}{}",
            codex_user("stale first turn"),
            codex_asst("must not be published")
        );
        let winner = format!(
            "{META}\n{}{}",
            codex_user("winning first turn"),
            codex_asst("only this root is published")
        );
        let winner_store = s.clone();
        let winner_head = std::rc::Rc::new(std::cell::RefCell::new(None));
        let winner_head_from_hook = winner_head.clone();
        interleave_next_settlement(move |repo, branch| {
            let code = settle_bytes(
                &winner_store,
                repo,
                "alice/photo",
                branch,
                link(),
                winner.as_bytes(),
                "alice",
                opts(),
                true,
                false,
            )
            .unwrap();
            assert_eq!(code, ExitCode::Ok);
            *winner_head_from_hook.borrow_mut() = Some(repo.git(&["rev-parse", "HEAD"]).unwrap());
        });

        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            stale.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("already exists") || message.contains("cannot lock ref"),
            "{message}"
        );

        let winner_head = winner_head
            .borrow()
            .clone()
            .expect("the interleaved first writer published");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), winner_head);
        assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]).unwrap(), "1");
        assert_eq!(
            repo.git(&["rev-list", "--parents", "-n", "1", "HEAD"])
                .unwrap()
                .split_ascii_whitespace()
                .count(),
            1,
            "the winning first turn must be a parentless root"
        );
        assert_eq!(
            repo.git(&["show", "-s", "--format=%s", "HEAD"]).unwrap(),
            "winning first turn"
        );
        let committed_log = storage::materialize_at(repo.root(), "HEAD", meta::LOG_FILE).unwrap();
        assert!(committed_log.contains("winning first turn"));
        assert!(!committed_log.contains("stale first turn"));
        assert_eq!(
            storage::materialize_worktree(repo.root(), meta::LOG_FILE).unwrap(),
            committed_log,
            "the stale proposal must not alter the winning managed checkout"
        );
        assert_eq!(
            repo.show("HEAD", "memory.md").unwrap(),
            "worktree layer",
            "root settlement keeps the historical git-add -A worktree-wins semantics"
        );
        assert_eq!(
            repo.show("HEAD", "ignored-staged.txt").unwrap(),
            "worktree ignored layer",
            "a force-added ignored path remains tracked when the temporary index is refreshed"
        );
        assert!(
            repo.show("HEAD", meta::ATTRS_FILE)
                .unwrap()
                .contains("*.user-defined binary"),
            "canonical storage attributes must preserve user rules"
        );
        assert_eq!(
            repo.git(&["write-tree"]).unwrap(),
            repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap(),
            "only the CAS winner may align the real index"
        );
        assert!(
            repo.git(&["status", "--porcelain"]).unwrap().is_empty(),
            "the stale first writer must leave the winner's checkout clean"
        );
    }

    /// A concurrent settler can advance the branch after this process has read the committed
    /// prefix. The stale process must keep the original tip as both its parent and CAS expectation;
    /// otherwise it can parent the already-consumed runtime turn onto the new tip a second time.
    #[test]
    fn concurrent_tip_advance_after_snapshot_cannot_repeat_runtime_turn() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let first = format!("{META}\n{}{}", codex_user("turn one"), codex_asst("got it"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            first.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();

        let full = format!("{first}{}{}", codex_user("turn two"), codex_asst("done"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            full.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        let concurrent = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let concurrent_log = storage::materialize_at(repo.root(), &concurrent, meta::LOG_FILE)
            .expect("concurrent settlement must be readable");
        assert!(concurrent_log.contains("turn two"));

        // Recreate the stale reader's starting point while retaining the already-built concurrent
        // result. The failpoint below lands that result after settle_bytes freezes `old` and reads
        // its baseline, exactly where the historical race occurred.
        crate::commands::plumbing::update_branch_cas_and_refresh(
            &repo,
            "main",
            &old,
            &concurrent,
            false,
        )
        .unwrap();
        assert!(
            !storage::materialize_at(repo.root(), "HEAD", meta::LOG_FILE)
                .unwrap()
                .contains("turn two")
        );

        let link_path = link::link_path(&s, "codex", "AB");
        let link_before = std::fs::read(&link_path).unwrap();
        let old_for_hook = old.clone();
        let concurrent_for_hook = concurrent.clone();
        interleave_next_settlement(move |repo, branch| {
            crate::commands::plumbing::update_branch_cas_and_refresh(
                repo,
                branch,
                &concurrent_for_hook,
                &old_for_hook,
                false,
            )
            .unwrap();
        });

        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            full.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected"), "{error:#}");

        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), concurrent);
        assert_eq!(
            storage::materialize_at(repo.root(), "HEAD", meta::LOG_FILE).unwrap(),
            concurrent_log,
            "the failed stale settlement must not alter the winning checkout"
        );
        assert_eq!(
            repo.git(&["rev-list", "--count", "HEAD"]).unwrap(),
            "2",
            "the second runtime turn must remain reachable exactly once"
        );
        assert_eq!(
            repo.git(&["log", "--format=%s", "--grep=^turn two$", "HEAD"])
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(std::fs::read(&link_path).unwrap(), link_before);
        assert!(
            repo.git(&["status", "--porcelain"]).unwrap().is_empty(),
            "a rejected stale settlement must leave index and worktree clean"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_commit_treats_dev_null_hooks_path_as_disabled() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        repo.git(&["config", "core.hooksPath", "/dev/null"])
            .unwrap();
        std::fs::write(
            repo.root().join("memory.md"),
            "hooks deliberately disabled\n",
        )
        .unwrap();

        let mut options = opts();
        options.message = Some("commit without hooks".into());
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();

        assert_eq!(code, ExitCode::Ok);
        assert_eq!(
            repo.show("HEAD", "memory.md").unwrap(),
            "hooks deliberately disabled"
        );
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn file_commit_git_version_parser_accepts_supported_vendor_formats() {
        assert_eq!(
            parse_file_commit_git_version(b"git version 2.28.0\n").unwrap(),
            FileCommitGitVersion {
                major: 2,
                minor: 28,
            }
        );
        assert_eq!(
            parse_file_commit_git_version(b"git version 2.47.1.windows.1\n").unwrap(),
            FileCommitGitVersion {
                major: 2,
                minor: 47,
            }
        );
        assert_eq!(
            parse_file_commit_git_version(b"git version 2.39.5 (Apple Git-154)\n").unwrap(),
            FileCommitGitVersion {
                major: 2,
                minor: 39,
            }
        );
        assert!(parse_file_commit_git_version(b"git version unknown\n").is_err());
    }

    #[test]
    fn file_commit_comment_alias_respects_git_version_and_config_order() {
        let old = FileCommitGitVersion {
            major: 2,
            minor: 44,
        };
        let modern = FileCommitGitVersion::COMMENT_STRING_ALIAS;

        assert!(
            auto_comment_char_from_config(old, b"core.commentchar\nAuTo\0").unwrap(),
            "core.commentChar=auto predates the alias"
        );
        assert!(
            !auto_comment_char_from_config(old, b"core.commentstring\nauto\0").unwrap(),
            "Git before 2.45 ignores core.commentString"
        );
        assert!(
            auto_comment_char_from_config(old, b"core.commentchar\nauto\0core.commentstring\n#\0",)
                .unwrap(),
            "an ignored new alias cannot override the old key"
        );
        assert!(
            auto_comment_char_from_config(
                modern,
                b"core.commentchar\n#\0core.commentstring\nauto\0",
            )
            .unwrap(),
            "the later alias wins on Git 2.45 and newer"
        );
        assert!(
            !auto_comment_char_from_config(
                modern,
                b"core.commentstring\nauto\0core.commentchar\n;\0",
            )
            .unwrap(),
            "the legacy spelling can still override the new alias when it appears later"
        );
        assert!(!auto_comment_char_from_config(modern, b"").unwrap());
        assert!(auto_comment_char_from_config(modern, b"core.commentchar\nauto").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_commit_respects_git_cleanup_configuration() {
        let raw = "hook subject   \n\n\n# hash comment   \n; semicolon comment   \nbody   \n# ------------------------ >8 ------------------------\ncut body   \n\n\n";
        let whitespace = "hook subject\n\n# hash comment\n; semicolon comment\nbody\n# ------------------------ >8 ------------------------\ncut body\n";
        let strip_hash = "hook subject\n\n; semicolon comment\nbody\ncut body\n";
        let strip_semicolon = "hook subject\n\n# hash comment\nbody\n# ------------------------ >8 ------------------------\ncut body\n";
        for (case, cleanup, comment_char, hook_message, expected) in [
            ("default", "default", "#", raw, whitespace),
            ("whitespace", "whitespace", "#", raw, whitespace),
            // `-m` never opens an editor, so Git does not apply the scissors cut line.
            ("scissors", "scissors", "#", raw, whitespace),
            ("strip", "strip", "#", raw, strip_hash),
            ("strip custom comment", "strip", ";", raw, strip_semicolon),
            ("verbatim", "verbatim", "#", raw, raw),
            // Native Git accepts non-empty whitespace when cleanup is explicitly verbatim.
            ("verbatim whitespace", "verbatim", "#", "   \n\n", "   \n\n"),
        ] {
            let (_d, s) = store();
            let (_h, repo) = setup_repo();
            init_main_file_line(&repo);
            repo.git(&["config", "commit.cleanup", cleanup]).unwrap();
            repo.git(&["config", "core.commentChar", comment_char])
                .unwrap();
            std::fs::write(repo.root().join("memory.md"), format!("{case}\n")).unwrap();
            install_hook(
                &repo,
                "commit-msg",
                &format!("printf '%s' '{hook_message}' > \"$1\""),
            );

            let mut options = opts();
            options.message = Some(format!("original {case}"));
            let code = settle_bytes(
                &s,
                &repo,
                "alice/photo",
                "main",
                link(),
                b"",
                "alice",
                options,
                false,
                false,
            )
            .unwrap();

            assert_eq!(code, ExitCode::Ok, "{case}");
            assert_eq!(
                raw_commit_message(&repo, "HEAD"),
                expected.as_bytes(),
                "{case} message bytes differ from native git commit cleanup"
            );
            assert!(
                repo.git(&["status", "--porcelain"]).unwrap().is_empty(),
                "{case} left the checkout dirty"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_commit_auto_comment_char_preserves_hash_lines_added_by_real_hook() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        repo.git(&["config", "commit.cleanup", "strip"]).unwrap();
        repo.git(&["config", "core.commentChar", "auto"]).unwrap();
        std::fs::write(repo.root().join("memory.md"), "auto comment char\n").unwrap();
        install_hook(
            &repo,
            "commit-msg",
            r#"printf '# hash line
; selected auto comment
body
' > "$1""#,
        );

        let mut options = opts();
        // Git sees this initial hash-prefixed line before hooks and therefore selects `;`, the
        // first available character in its `#;@!$%^&|:` candidate order.
        options.message = Some("# initial hash line".into());
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();

        assert_eq!(code, ExitCode::Ok);
        assert_eq!(
            raw_commit_message(&repo, "HEAD"),
            b"# hash line\nbody\n",
            "auto cleanup must preserve the hook's hash-authored content"
        );
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_commit_comment_string_auto_preserves_hash_lines_added_by_real_hook() {
        if file_commit_git_version().unwrap() < FileCommitGitVersion::COMMENT_STRING_ALIAS {
            return;
        }

        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        repo.git(&["config", "commit.cleanup", "strip"]).unwrap();
        repo.git(&["config", "core.commentString", "auto"]).unwrap();
        std::fs::write(repo.root().join("memory.md"), "auto comment string\n").unwrap();
        install_hook(
            &repo,
            "commit-msg",
            r#"printf '# hash line
; selected auto comment
body
' > "$1""#,
        );

        let mut options = opts();
        options.message = Some("# initial hash line".into());
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();

        assert_eq!(code, ExitCode::Ok);
        assert_eq!(raw_commit_message(&repo, "HEAD"), b"# hash line\nbody\n");
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_commit_runs_hooks_in_order_and_post_failure_cannot_undo_publication() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        repo.git(&["config", "core.hooksPath", ".git/file-commit-hooks"])
            .unwrap();
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        std::fs::write(repo.root().join("memory.md"), "shared memory\n").unwrap();

        install_hook(
            &repo,
            "pre-commit",
            r#"test "${GIT_EDITOR-}" = ":"
git diff --cached --name-only > .git/file-commit-pre-index
git show :session/meta.json > .git/file-commit-pre-meta
printf 'staged by pre-commit\n' > hook-added.md
printf '*.hook binary\n' > .gitattributes
git add -- hook-added.md .gitattributes
printf 'pre\n' >> .git/file-commit-hook-order"#,
        );
        install_hook(
            &repo,
            "prepare-commit-msg",
            r#"test "${2-}" = "message"
test -f "$1"
printf '\n# kept hook comment\nprepared-by-hook\n' >> "$1"
printf 'prepare\n' >> .git/file-commit-hook-order"#,
        );
        install_hook(
            &repo,
            "commit-msg",
            r#"grep -q '^prepared-by-hook$' "$1"
printf '\ncommit-msg-edited\n' >> "$1"
printf 'commit-msg\n' >> .git/file-commit-hook-order"#,
        );
        install_hook(
            &repo,
            "post-commit",
            r#"git diff --cached --quiet
test "$(git show HEAD:hook-added.md)" = "staged by pre-commit"
git rev-parse HEAD > .git/file-commit-post-head
printf 'post\n' >> .git/file-commit-hook-order
exit 42"#,
        );

        let mut options = opts();
        options.message = Some("base message".into());
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok, "post-commit is notification-only");

        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        assert_ne!(head, old);
        assert_eq!(
            std::fs::read_to_string(repo.root().join(".git/file-commit-hook-order")).unwrap(),
            "pre\nprepare\ncommit-msg\npost\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join(".git/file-commit-post-head"))
                .unwrap()
                .trim(),
            head
        );
        let pre_index =
            std::fs::read_to_string(repo.root().join(".git/file-commit-pre-index")).unwrap();
        assert!(pre_index.lines().any(|path| path == "memory.md"));
        assert_eq!(
            std::fs::read(repo.root().join(".git/file-commit-pre-meta")).unwrap(),
            repo.git_bytes_result(&["show", &format!("HEAD:{}", meta::FILE)])
                .unwrap(),
            "pre-commit must read the same prepared real-index metadata that is published"
        );
        assert_eq!(
            repo.show_raw("HEAD", "hook-added.md").unwrap(),
            "staged by pre-commit\n",
            "a hook must operate on the same real index used to build the commit tree"
        );
        assert_eq!(
            repo.show_raw("HEAD", meta::ATTRS_FILE).unwrap(),
            storage::attributes_text_strict(Some("*.hook binary\n")).unwrap(),
            "hook-staged user attributes must retain their rules with canonical managed blocks"
        );
        let message = repo.git(&["show", "-s", "--format=%B", "HEAD"]).unwrap();
        let base = message.find("base message").unwrap();
        let comment = message.find("# kept hook comment").unwrap();
        let prepared = message.find("prepared-by-hook").unwrap();
        let committed = message.find("commit-msg-edited").unwrap();
        assert!(base < comment && comment < prepared && prepared < committed);
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty(),
            "post-commit must see an index matching the published HEAD"
        );
    }

    #[cfg(unix)]
    #[test]
    fn blocking_file_commit_hooks_restore_ref_index_meta_and_attributes_exactly() {
        for (blocking, expected_order) in [
            ("pre-commit", "pre\n"),
            ("prepare-commit-msg", "pre\nprepare\n"),
            ("commit-msg", "pre\nprepare\ncommit-msg\n"),
        ] {
            let (_d, s) = store();
            let (_h, repo) = setup_repo();
            init_main_file_line(&repo);
            let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();

            std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.staged binary\n").unwrap();
            repo.git(&["add", "--", meta::ATTRS_FILE]).unwrap();
            std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.worktree binary\n").unwrap();
            std::fs::write(repo.root().join("memory.md"), "staged memory\n").unwrap();
            repo.git(&["add", "--", "memory.md"]).unwrap();
            std::fs::write(repo.root().join("memory.md"), "unstaged memory\n").unwrap();
            std::fs::write(repo.root().join("untracked.md"), "keep untracked\n").unwrap();

            let status_before = repo
                .git_bytes_result(&["status", "--porcelain=v1", "-z"])
                .unwrap();
            let attrs_before = std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap();
            let meta_before = std::fs::read(repo.root().join(meta::FILE)).unwrap();
            let staged_before = repo
                .git_bytes_result(&["diff", "--cached", head_before.trim(), "--"])
                .unwrap();
            let unstaged_before = repo.git_bytes_result(&["diff", "--"]).unwrap();
            let index_before = captured_index_bytes(&FileCommitState::capture(&repo).unwrap());

            let dirty_managed = r#"printf 'hook attrs\n' > .gitattributes
printf 'hook meta\n' > session/meta.json
git add -- .gitattributes session/meta.json"#;
            let pre_commit = if blocking == "pre-commit" {
                format!("printf 'pre\\n' >> .git/file-commit-hook-order\n{dirty_managed}\nexit 23")
            } else {
                "printf 'pre\\n' >> .git/file-commit-hook-order".into()
            };
            install_hook(&repo, "pre-commit", &pre_commit);
            let prepare_commit_msg = if blocking == "prepare-commit-msg" {
                format!(
                    "printf 'prepare\\n' >> .git/file-commit-hook-order\n{dirty_managed}\nexit 24"
                )
            } else {
                "printf 'prepare\\n' >> .git/file-commit-hook-order".into()
            };
            install_hook(&repo, "prepare-commit-msg", &prepare_commit_msg);
            let commit_msg = if blocking == "commit-msg" {
                format!(
                    "printf 'commit-msg\\n' >> .git/file-commit-hook-order\n{dirty_managed}\nexit 25"
                )
            } else {
                "printf 'commit-msg\\n' >> .git/file-commit-hook-order".into()
            };
            install_hook(&repo, "commit-msg", &commit_msg);
            install_hook(
                &repo,
                "post-commit",
                "printf 'post\\n' >> .git/file-commit-hook-order",
            );

            let mut options = opts();
            options.message = Some(format!("blocked by {blocking}"));
            let error = settle_bytes(
                &s,
                &repo,
                "alice/photo",
                "main",
                link(),
                b"",
                "alice",
                options,
                false,
                false,
            )
            .unwrap_err();
            assert!(error.to_string().contains(blocking), "{error:#}");
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);

            let index_after = captured_index_bytes(&FileCommitState::capture(&repo).unwrap());
            assert_eq!(index_after, index_before, "{blocking} changed index bytes");
            assert_eq!(
                std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
                attrs_before,
                "{blocking} changed attributes worktree bytes"
            );
            assert_eq!(
                std::fs::read(repo.root().join(meta::FILE)).unwrap(),
                meta_before,
                "{blocking} changed metadata worktree bytes"
            );
            assert_eq!(
                repo.git_bytes_result(&["diff", "--cached", head_before.trim(), "--"])
                    .unwrap(),
                staged_before
            );
            assert_eq!(
                repo.git_bytes_result(&["diff", "--"]).unwrap(),
                unstaged_before
            );
            assert_eq!(
                repo.git_bytes_result(&["status", "--porcelain=v1", "-z"])
                    .unwrap(),
                status_before
            );
            assert_eq!(
                std::fs::read_to_string(repo.root().join(".git/file-commit-hook-order")).unwrap(),
                expected_order
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn successful_hooks_cannot_publish_managed_paths_modes_or_bad_attributes() {
        for (case, pre_commit, expected_error) in [
            (
                "managed path",
                r#"oid="$(printf 'tampered log\n' | git hash-object -w --stdin)"
git update-index --add --cacheinfo 100644 "$oid" LOG
printf 'pre\n' >> .git/file-commit-hook-order"#,
                "managed storage paths",
            ),
            (
                "metadata mode",
                r#"git update-index --chmod=+x -- session/meta.json
printf 'pre\n' >> .git/file-commit-hook-order"#,
                "mode 100755",
            ),
            (
                "attributes",
                r#"printf '# agit:storage-v1 defaults begin\nunclosed hook block\n' > .gitattributes
git add -- .gitattributes
printf 'pre\n' >> .git/file-commit-hook-order"#,
                "has no end marker",
            ),
        ] {
            let (_d, s) = store();
            let (_h, repo) = setup_repo();
            init_main_file_line(&repo);
            std::fs::write(repo.root().join("memory.md"), "shared memory\n").unwrap();

            let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
            let status_before = repo
                .git_bytes_result(&["status", "--porcelain=v1", "-z"])
                .unwrap();
            let attrs_before = std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap();
            let meta_before = std::fs::read(repo.root().join(meta::FILE)).unwrap();
            let index_before = captured_index_bytes(&FileCommitState::capture(&repo).unwrap());

            install_hook(&repo, "pre-commit", pre_commit);
            install_hook(
                &repo,
                "prepare-commit-msg",
                "printf 'prepare\\n' >> .git/file-commit-hook-order",
            );
            install_hook(
                &repo,
                "commit-msg",
                "printf 'commit-msg\\n' >> .git/file-commit-hook-order",
            );
            install_hook(
                &repo,
                "post-commit",
                "printf 'post\\n' >> .git/file-commit-hook-order",
            );

            let mut options = opts();
            options.message = Some(format!("reject {case}"));
            let error = settle_bytes(
                &s,
                &repo,
                "alice/photo",
                "main",
                link(),
                b"",
                "alice",
                options,
                false,
                false,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(expected_error),
                "{case}: {error:#}"
            );
            assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
            assert_eq!(
                captured_index_bytes(&FileCommitState::capture(&repo).unwrap()),
                index_before,
                "{case} changed index bytes"
            );
            assert_eq!(
                std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
                attrs_before,
                "{case} changed attributes worktree bytes"
            );
            assert_eq!(
                std::fs::read(repo.root().join(meta::FILE)).unwrap(),
                meta_before,
                "{case} changed metadata worktree bytes"
            );
            assert_eq!(
                repo.git_bytes_result(&["status", "--porcelain=v1", "-z"])
                    .unwrap(),
                status_before
            );
            assert_eq!(
                std::fs::read_to_string(repo.root().join(".git/file-commit-hook-order")).unwrap(),
                "pre\nprepare\ncommit-msg\n",
                "post-commit must not run when the frozen tree is rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unborn_file_commit_hook_cannot_introduce_agentgit_metadata() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        std::fs::write(repo.root().join("memory.md"), "first shared file\n").unwrap();
        let status_before = repo
            .git_bytes_result(&["status", "--porcelain=v1", "-z"])
            .unwrap();
        let index_before =
            captured_index_bytes_or_missing(&FileCommitState::capture(&repo).unwrap());

        install_hook(
            &repo,
            "pre-commit",
            r#"mkdir -p session
printf 'hook-owned metadata\n' > session/meta.json
git add -- session/meta.json
printf 'pre\n' >> .git/file-commit-hook-order"#,
        );
        install_hook(
            &repo,
            "prepare-commit-msg",
            "printf 'prepare\\n' >> .git/file-commit-hook-order",
        );
        install_hook(
            &repo,
            "commit-msg",
            "printf 'commit-msg\\n' >> .git/file-commit-hook-order",
        );
        install_hook(
            &repo,
            "post-commit",
            "printf 'post\\n' >> .git/file-commit-hook-order",
        );

        let mut options = opts();
        options.message = Some("first shared commit".into());
        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            true,
            false,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("managed storage paths"),
            "{error:#}"
        );
        assert_eq!(
            optional_branch_commit(&repo, "refs/heads/main").unwrap(),
            None
        );
        assert_eq!(
            captured_index_bytes_or_missing(&FileCommitState::capture(&repo).unwrap()),
            index_before
        );
        assert!(!repo.root().join(meta::FILE).exists());
        assert_eq!(
            repo.git_bytes_result(&["status", "--porcelain=v1", "-z"])
                .unwrap(),
            status_before
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join(".git/file-commit-hook-order")).unwrap(),
            "pre\nprepare\ncommit-msg\n"
        );
    }

    /// A file commit stages through the real index so it must treat all of those mutations as one
    /// transaction. If another writer wins the branch after our tree/commit object was built, the
    /// stale proposal is unreachable and every pre-existing staged, unstaged and untracked layer
    /// is restored byte-for-byte.
    #[test]
    fn stale_file_commit_cas_restores_exact_index_and_worktree_layers() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        let old = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let old_tree = repo
            .git(&["rev-parse", &format!("{old}^{{tree}}")])
            .unwrap();
        let winner = crate::commands::plumbing::commit_tree(
            &repo,
            old_tree.trim(),
            &[old.trim()],
            "concurrent winner",
        )
        .unwrap();

        #[cfg(unix)]
        {
            install_hook(
                &repo,
                "pre-commit",
                "printf 'pre\\n' >> .git/file-commit-hook-order",
            );
            install_hook(
                &repo,
                "prepare-commit-msg",
                "printf 'prepare\\n' >> .git/file-commit-hook-order",
            );
            install_hook(
                &repo,
                "commit-msg",
                "printf 'commit-msg\\n' >> .git/file-commit-hook-order",
            );
            install_hook(
                &repo,
                "post-commit",
                "printf 'post\\n' >> .git/file-commit-hook-order",
            );
        }

        std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.staged binary\n").unwrap();
        repo.git(&["add", "--", meta::ATTRS_FILE]).unwrap();
        std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.worktree binary\n").unwrap();

        std::fs::write(repo.root().join("memory.md"), "staged memory\n").unwrap();
        repo.git(&["add", "--", "memory.md"]).unwrap();
        std::fs::write(repo.root().join("memory.md"), "unstaged memory\n").unwrap();
        std::fs::write(repo.root().join("untracked.md"), "keep me untracked\n").unwrap();

        // Ask Git for status before snapshotting the raw index: status may refresh stat fields, and
        // the rollback contract starts after that user-visible state has stabilized.
        let status_before = repo
            .git_bytes_result(&["status", "--porcelain=v1", "-z"])
            .unwrap();
        let attrs_before = std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap();
        let meta_before = std::fs::read(repo.root().join(meta::FILE)).unwrap();
        let staged_before = repo
            .git_bytes_result(&["diff", "--cached", old.trim(), "--"])
            .unwrap();
        let unstaged_before = repo.git_bytes_result(&["diff", "--"]).unwrap();
        let state_before = FileCommitState::capture(&repo).unwrap();
        let index_before = match &state_before.index.body {
            FileSnapshotBody::Regular { bytes, .. } => bytes.clone(),
            _ => panic!("initialized repository must have a real index"),
        };

        let old_for_hook = old.clone();
        let winner_for_hook = winner.clone();
        interleave_next_settlement(move |repo, branch| {
            crate::commands::plumbing::update_ref_cas(
                repo,
                &format!("refs/heads/{branch}"),
                winner_for_hook.trim(),
                Some(old_for_hook.trim()),
            )
            .unwrap();
        });

        let mut options = opts();
        options.message = Some("stale shared files".into());
        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), winner);
        assert_eq!(
            repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap(),
            old_tree,
            "the winning commit must remain untouched"
        );

        // Compare the index before invoking any command that could refresh its stat cache.
        let state_after = FileCommitState::capture(&repo).unwrap();
        let index_after = match state_after.index.body {
            FileSnapshotBody::Regular { bytes, .. } => bytes,
            _ => panic!("rollback must restore the original real index"),
        };
        assert_eq!(index_after, index_before, "real index bytes changed");
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            attrs_before,
            "attributes worktree layer changed"
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::FILE)).unwrap(),
            meta_before,
            "metadata worktree layer changed"
        );
        assert_eq!(
            std::fs::read(repo.root().join("memory.md")).unwrap(),
            b"unstaged memory\n"
        );
        assert_eq!(
            std::fs::read(repo.root().join("untracked.md")).unwrap(),
            b"keep me untracked\n"
        );
        assert_eq!(
            repo.git_bytes_result(&["diff", "--cached", winner.trim(), "--"])
                .unwrap(),
            staged_before
        );
        assert_eq!(
            repo.git_bytes_result(&["diff", "--"]).unwrap(),
            unstaged_before
        );
        assert_eq!(
            repo.git_bytes_result(&["status", "--porcelain=v1", "-z"])
                .unwrap(),
            status_before
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_to_string(repo.root().join(".git/file-commit-hook-order")).unwrap(),
            "pre\nprepare\ncommit-msg\n",
            "post-commit must not run when the settlement CAS loses"
        );
    }

    /// The CAS implementation still follows ordinary `git commit` staging rules: explicit
    /// pathspecs are refreshed from the worktree, already-staged paths outside the pathspec are
    /// included, and their unstaged worktree layer remains after a successful commit.
    #[test]
    fn file_commit_cas_preserves_pathspec_and_partial_staging_semantics() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);

        std::fs::write(repo.root().join("chosen.md"), "chosen staged\n").unwrap();
        repo.git(&["add", "--", "chosen.md"]).unwrap();
        std::fs::write(repo.root().join("chosen.md"), "chosen worktree\n").unwrap();

        std::fs::write(repo.root().join("outside.md"), "outside staged\n").unwrap();
        repo.git(&["add", "--", "outside.md"]).unwrap();
        std::fs::write(repo.root().join("outside.md"), "outside unstaged\n").unwrap();
        std::fs::write(repo.root().join("untracked.md"), "not selected\n").unwrap();

        let mut options = opts();
        options.message = Some("selected shared files".into());
        options.paths = vec!["chosen.md".into()];
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        assert_eq!(
            repo.show_raw("HEAD", "chosen.md").unwrap(),
            "chosen worktree\n"
        );
        assert_eq!(
            repo.show_raw("HEAD", "outside.md").unwrap(),
            "outside staged\n"
        );
        assert!(repo.show_raw("HEAD", "untracked.md").is_none());
        assert_eq!(
            std::fs::read_to_string(repo.root().join("outside.md")).unwrap(),
            "outside unstaged\n"
        );
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty(),
            "the committed index must match the CAS-published tree"
        );
        assert_eq!(
            repo.git(&["diff", "--name-only", "--"]).unwrap(),
            "outside.md"
        );
        assert!(
            repo.git(&["status", "--porcelain", "--", "untracked.md"])
                .unwrap()
                .contains("untracked.md")
        );
    }

    /// A brand-new repo plus `-b exp`: the first turn commit must land on exp, and no session
    /// may claim main. An implementation that leaves HEAD on init's default main records the
    /// conversation on the file line instead.
    #[test]
    fn fresh_repo_settles_onto_named_session_branch_not_main() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let text = format!("{META}\n{}{}", codex_user("turn one"), codex_asst("reply"));
        let r2 = checkout_for_settlement(&repo, "alice/photo", "exp").unwrap();
        assert!(r2.is_ok());
        assert_eq!(
            repo.current_branch().as_deref(),
            Some("exp"),
            "HEAD points at the session branch"
        );
        assert!(
            !repo.has_ref("refs/heads/main") && !repo.has_ref("refs/heads/exp"),
            "nothing is committed yet; main above all must not be born — the file line comes later"
        );

        // Settling onto a branch that does not exist, once the repo has content → refuse (only
        // the four commands give birth to a branch).
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "exp",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        let refused = checkout_for_settlement(&repo, "alice/photo", "ghost").unwrap();
        assert!(matches!(refused, Err(ExitCode::Precondition)));
    }

    /// A trailing turn with only a user prompt and no reply does not settle (a turn is atomic).
    #[test]
    fn trailing_in_progress_turn_is_not_settled() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let text = format!(
            "{META}\n{}{}{}",
            codex_user("turn one"),
            codex_asst("reply one"),
            codex_user("nobody has answered me yet") // in flight: not a word of reply
        );
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 1, "the trailing turn is not settled");
        let head_t = repo.show("HEAD", meta::LOG_FILE).unwrap();
        assert!(!head_t.contains("nobody has answered me yet"));
    }

    fn codex_call(id: &str) -> String {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"custom_tool_call\",\"call_id\":\"{id}\",\"name\":\"exec_command\",\"input\":\"agit commit\"}}}}\n"
        )
    }

    fn codex_output(id: &str, t: &str) -> String {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"custom_tool_call_output\",\"call_id\":\"{id}\",\"output\":\"{t}\"}}}}\n"
        )
    }

    /// The agent runs `agit commit` from inside a turn: the turn already has a reply and a
    /// call, but the call has not returned — it is still half a turn and does not settle. Once
    /// the output and the wrap-up are written back, the whole turn lands as one commit and not a
    /// word of its second half is lost.
    #[test]
    fn a_turn_with_an_open_tool_call_waits_for_the_call_to_return() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let half = format!(
            "{META}\n{}{}{}{}{}",
            codex_user("turn one"),
            codex_asst("reply one"),
            codex_user("push yourself up"),
            codex_asst("ok, settling first"),
            codex_call("c1")
        );
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            half.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 1, "an open call blocks settlement");
        let t1 = repo.show("HEAD", meta::LOG_FILE).unwrap();
        assert!(!t1.contains("push yourself up"));

        let full = format!(
            "{half}{}{}",
            codex_output("c1", "settled"),
            codex_asst("pushed")
        );
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            full.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 2, "a finished turn is one commit");
        let snap = meta::read_at_ref(&repo, "HEAD").unwrap();
        assert_eq!(snap.turn, Some(2));
        let t2 = repo.show("HEAD", meta::LOG_FILE).unwrap();
        assert!(t2.contains("push yourself up"));
        assert!(t2.contains("settled"), "the output is in this turn: {t2}");
        assert!(t2.contains("pushed"), "the wrap-up is in this turn: {t2}");
    }

    /// An open call blocks settlement only in the **trailing** turn: a call that never returned
    /// in an earlier turn (it was interrupted) is already closed by the user prompt that follows,
    /// and that turn settles as usual.
    #[test]
    fn an_open_call_in_an_earlier_turn_does_not_block_settlement() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let text = format!(
            "{META}\n{}{}{}{}{}",
            codex_user("turn one"),
            codex_call("interrupted"),
            codex_user("different question"),
            codex_asst("reply two"),
            codex_output("later", "?")
        );
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 2);
    }

    /// Claude Code likewise: a trailing turn whose `tool_use` has no `tool_result` yet does not
    /// settle.
    #[test]
    fn a_claude_turn_with_an_open_tool_use_is_not_settled() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let cc_user = |id: &str, t: &str| {
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"CC\",\"cwd\":\"/repo/one\",\"uuid\":\"{id}\",\"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}\n"
            )
        };
        let cc_asst = |id: &str, t: &str| {
            format!(
                "{{\"type\":\"assistant\",\"sessionId\":\"CC\",\"cwd\":\"/repo/one\",\"uuid\":\"{id}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{t}\"}}]}}}}\n"
            )
        };
        let cc_call = "{\"type\":\"assistant\",\"sessionId\":\"CC\",\"cwd\":\"/repo/one\",\"uuid\":\"u4\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{\"command\":\"agit commit\"}}]}}\n";
        let cc_result = "{\"type\":\"user\",\"sessionId\":\"CC\",\"cwd\":\"/repo/one\",\"uuid\":\"u5\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"ok\"}]}}\n";
        let mut lk = Link::new("claude-code", "CC", Some(Path::new("/repo/one")));
        lk.agent = Some("photo".into());
        lk.branch = Some("main".into());
        let half = format!(
            "{}{}{}{}{cc_call}",
            cc_user("u1", "turn one"),
            cc_asst("u2", "reply one"),
            cc_user("u3", "push it up"),
            cc_asst("u3b", "ok")
        );
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk.clone(),
            half.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 1);

        let full = format!("{half}{cc_result}{}", cc_asst("u6", "pushed"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk,
            full.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 2);
        let t2 = repo.show("HEAD", meta::LOG_FILE).unwrap();
        assert!(t2.contains("pushed"), "{t2}");
    }

    #[test]
    fn hook_input_only_keeps_a_non_empty_session_id() {
        assert_eq!(
            HookInput::parse(r#"{"session_id":"abc","cwd":"/x","hook_event_name":"Stop"}"#)
                .unwrap()
                .session_id
                .as_deref(),
            Some("abc")
        );
        assert_eq!(
            HookInput::parse(r#"{"session_id":"","cwd":"/x"}"#)
                .unwrap()
                .session_id,
            None
        );
        assert!(HookInput::parse("not json").is_none());
    }

    /// A hook settles only a session whose claim is registered: both a `hooks ingest`
    /// pre-registration and an id that was never registered mean "not adopted yet", and neither
    /// may fall back onto another session's branch.
    #[test]
    fn a_hook_settles_only_links_with_a_claimed_branch() {
        let (_d, s) = store();
        let pending = Link::new("claude-code", "new-session", Some(Path::new("/repo/one")));
        link::write(&s, &pending).unwrap();
        assert!(hook_claim(&pending).is_none());
        assert!(hook_target(&s, "new-session", None).is_none());
        assert!(hook_target(&s, "never-seen", None).is_none());

        let mut claimed = Link::new("claude-code", "old-session", Some(Path::new("/repo/one")));
        claimed.agent = Some("photo".into());
        claimed.branch = Some("s1".into());
        assert_eq!(hook_claim(&claimed), Some(("photo".into(), "s1".into())));
    }

    /// The link records only a bare agent name; the owner is recovered from the injected session
    /// identity or the directory binding, and counts only when the agent name matches — inside
    /// someone else's read-only checkout the owner is not the current user.
    #[test]
    fn a_hook_recovers_the_owner_from_the_session_identity_or_the_binding() {
        assert_eq!(
            hook_slug("qa", None, Some("alice/qa"), Some("me/qa")),
            "alice/qa",
            "the injected identity wins"
        );
        assert_eq!(
            hook_slug("qa", None, Some("alice/other"), Some("bob/qa")),
            "bob/qa",
            "when the injected identity names another agent, the directory binding decides"
        );
        assert_eq!(
            hook_slug("qa", None, Some("alice/other"), Some("bob/other")),
            super::super::context::qualify("qa"),
            "only when neither matches does it fall back to the current user"
        );
    }

    /// Unchanged content: print the no-op and exit 0.
    #[test]
    fn unchanged_content_is_a_noop() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let text = format!("{META}\n{}{}", codex_user("question"), codex_asst("answer"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(repo.commit_count(), 1);
    }

    /// A baseline written non-append (materialized mode) → refused (Policy).
    #[test]
    fn tampered_baseline_is_refused() {
        use sha2::Digest;
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let materialized = format!("{META}\n{}{}", codex_user("question"), codex_asst("answer"));
        let mut lk = link();
        lk.baseline_bytes = Some(materialized.len() as u64);
        lk.baseline_hash = Some(hex::encode(sha2::Sha256::digest(materialized.as_bytes())));
        // Tamper with the baseline region by swapping one word.
        let tampered = materialized.replacen("question", "X", 1);
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk,
            tampered.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Policy);
        assert_eq!(repo.commit_count(), 0, "a refusal leaves no commit at all");
    }

    /// Materialized baseline mode: settle only what was appended after the baseline, with turn
    /// ordinals continuing the branch history.
    #[test]
    fn baseline_mode_settles_only_the_appended_region() {
        use sha2::Digest;
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        // Record one ordinary turn first (native mode).
        let text = format!("{META}\n{}{}", codex_user("past turn"), codex_asst("reply"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(meta::read_at_ref(&repo, "HEAD").unwrap().turn, Some(1));

        // Materialize: the baseline is the content as of now, and one new turn grows after it.
        let mut lk = link();
        lk.baseline_bytes = Some(text.len() as u64);
        lk.baseline_hash = Some(hex::encode(sha2::Sha256::digest(text.as_bytes())));
        let grown = format!("{text}{}{}", codex_user("new work"), codex_asst("done"));
        // In the real flow `resume` persists this link with its baseline; settlement's watermark
        // CAS uses it as the expected value, so the disk has to hold it first.
        crate::domain::link::write(&s, &lk).unwrap();
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk.clone(),
            grown.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();
        let snap = meta::read_at_ref(&repo, "HEAD").unwrap();
        assert_eq!(snap.turn, Some(2), "turn ordinals continue the history");
        // The link's baseline in the store advances to what settled (the in-memory `lk` is the
        // copy that was passed in, so it is not the one to read).
        let back = crate::domain::link::get(&s, "codex", "AB").unwrap();
        assert_eq!(back.baseline_bytes, Some(grown.len() as u64));
        assert!(back.baseline_hash.is_none());
    }

    /// A slow-path resume starts from a materialized VIEW, which can be only a small projection of
    /// LOG and can contain envelopes inherited from another session. Settling its next turn must
    /// append to the committed evidence/projection instead of treating the runtime baseline as
    /// authoritative and re-minting all of its provenance.
    #[test]
    fn materialized_settlement_preserves_log_evidence_and_inherited_provenance() {
        use sha2::Digest;

        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let inherited_claim = format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN));
        let branch_claim = format!("{}{}", meta::ID_PREFIX, "b".repeat(meta::ID_HEX_LEN));

        let inherited_visible = transcript::wrap_lines(
            &codex_user("inherited visible context"),
            "codex",
            &inherited_claim,
        );
        let hidden_log_event = transcript::wrap_lines(
            &codex_asst("hidden from VIEW by an earlier revert"),
            "codex",
            &inherited_claim,
        );
        let old_log = format!("{inherited_visible}{hidden_log_event}");
        let old_view = inherited_visible.clone();
        let inherited_id = storage::event_id(&inherited_visible).unwrap();
        let hidden_id = storage::event_id(&hidden_log_event).unwrap();

        storage::write_snapshot(repo.root(), &old_log, &old_view).unwrap();
        let mut snapshot = Meta::new(branch_claim.clone(), "codex".into(), "/repo/one".into());
        snapshot.turn = Some(4);
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("fork with a surgically reduced VIEW").unwrap();

        // This is what a cross-harness slow resume owns: a Claude rendering of HEAD VIEW under a
        // freshly minted runtime id. Its bytes deliberately do not equal the old Codex envelope.
        let runtime_id = "019c8f50-15a2-7e60-a9ce-c993712c9d42";
        let baseline = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{runtime_id}\",\"uuid\":\"u1\",\"message\":{{\"role\":\"user\",\"content\":\"inherited visible context\"}}}}\n{{\"type\":\"assistant\",\"sessionId\":\"{runtime_id}\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"ready\"}}]}}}}\n"
        );
        let appended = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{runtime_id}\",\"uuid\":\"u3\",\"parentUuid\":\"u2\",\"message\":{{\"role\":\"user\",\"content\":\"new work\"}}}}\n{{\"type\":\"assistant\",\"sessionId\":\"{runtime_id}\",\"uuid\":\"u4\",\"parentUuid\":\"u3\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}\n"
        );
        let grown = format!("{baseline}{appended}");
        let mut lk = Link::new("claude-code", runtime_id, Some(Path::new("/repo/one")));
        lk.agent = Some("photo".into());
        lk.branch = Some("main".into());
        lk.baseline_bytes = Some(baseline.len() as u64);
        lk.baseline_hash = Some(hex::encode(sha2::Sha256::digest(baseline.as_bytes())));

        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk,
            grown.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);

        let new_log = storage::materialize_at(repo.root(), "HEAD", meta::LOG_FILE).unwrap();
        let new_view = storage::materialize_at(repo.root(), "HEAD", meta::VIEW_FILE).unwrap();
        assert!(
            new_log.starts_with(&old_log),
            "the complete pre-resume LOG must remain the evidence prefix"
        );
        assert!(
            new_view.starts_with(&old_view),
            "the pre-resume VIEW envelopes must retain their original provenance"
        );
        assert!(
            new_log
                .lines()
                .any(|line| storage::event_id(&format!("{line}\n")).unwrap() == hidden_id),
            "an event hidden from VIEW by prior surgery must remain in LOG"
        );

        let first = format!("{}\n", new_log.lines().next().unwrap());
        assert_eq!(storage::event_id(&first).unwrap(), inherited_id);
        let first: transcript::Envelope = serde_json::from_str(first.trim_end()).unwrap();
        assert_eq!(first.session_id, inherited_claim);

        let last: transcript::Envelope =
            serde_json::from_str(new_log.lines().last().unwrap()).unwrap();
        assert_eq!(last.source, "claude-code");
        assert_eq!(last.session_id, branch_claim);
        assert_eq!(
            meta::read_at_ref(&repo, "HEAD").unwrap().runtime,
            "claude-code"
        );
    }

    /// The main that `agit init` creates: its meta says `line: file`, and transcript input is
    /// refused.
    fn init_main_file_line(repo: &Repo) {
        std::fs::write(repo.root().join("AGENTS.md"), "# t\n").unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        storage::ensure_attributes(repo.root()).unwrap();
        repo.add_all().unwrap();
        repo.commit("agit: init (main file line)").unwrap();
    }

    fn seed_v0_session(repo: &Repo, live: &str) {
        let claim = meta::session_hash("/repo/one", live.as_bytes());
        let log = transcript::wrap_lines(live, "codex", &claim);
        let view = transcript::view_of_live(live, "codex").unwrap();
        let view = transcript::wrap_lines(&view, "codex", &claim);
        let mut snapshot = Meta::new(claim, "codex".into(), "/repo/one".into());
        snapshot.layout = meta::LayoutVersion::V0;
        snapshot.turn = Some(1);
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), view).unwrap();
        repo.add_all().unwrap();
        repo.commit("legacy turn").unwrap();
    }

    #[test]
    fn ordinary_v0_settlement_preflights_dirty_user_namespace_before_writing() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let first = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));
        seed_v0_session(&repo, &first);
        let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let legacy_before = std::fs::read(repo.root().join(meta::LEGACY_LOG_FILE)).unwrap();
        std::fs::write(repo.root().join(meta::LOG_FILE), "untracked user LOG\n").unwrap();
        std::fs::write(repo.root().join("memory.md"), "dirty shared bytes\n").unwrap();
        let status_before = repo
            .git(&["status", "--porcelain=v1", "--untracked-files=all"])
            .unwrap();
        let grown = format!("{first}{}{}", codex_user("second"), codex_asst("done"));

        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            grown.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("root paths"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert_eq!(
            std::fs::read(repo.root().join(meta::LEGACY_LOG_FILE)).unwrap(),
            legacy_before
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap(),
            "untracked user LOG\n"
        );
        assert_eq!(
            repo.git(&["status", "--porcelain=v1", "--untracked-files=all"])
                .unwrap(),
            status_before
        );
    }

    #[cfg(unix)]
    #[test]
    fn first_v1_turn_rejects_untracked_events_symlink_without_external_write() {
        use std::os::unix::fs::symlink;

        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        meta::write(
            repo.root(),
            &Meta::new_session_line("codex".into(), "/repo/one".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("newborn v1 session").unwrap();
        let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), repo.root().join(meta::EVENTS_DIR)).unwrap();
        let live = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));

        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            live.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(!repo.root().join(meta::LOG_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn metadata_symlink_is_rejected_before_turn_storage_or_file_staging() {
        use std::os::unix::fs::symlink;

        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        meta::write(
            repo.root(),
            &Meta::new_session_line("codex".into(), "/repo/one".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("newborn v1 session").unwrap();
        let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside meta bytes\n").unwrap();
        std::fs::remove_file(meta::path_in(repo.root())).unwrap();
        symlink(outside.path(), meta::path_in(repo.root())).unwrap();
        let live = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));

        let turn_error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            live.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(
            turn_error.to_string().contains("regular file"),
            "{turn_error:#}"
        );
        assert!(!repo.root().join(meta::LOG_FILE).exists());
        assert!(!repo.root().join(meta::EVENTS_DIR).exists());

        std::fs::write(repo.root().join("memory.md"), "shared\n").unwrap();
        let mut file_options = opts();
        file_options.message = Some("shared file".into());
        let file_error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            file_options,
            false,
            false,
        )
        .unwrap_err();
        assert!(
            file_error.to_string().contains("regular file"),
            "{file_error:#}"
        );
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "outside meta bytes\n"
        );
    }

    #[test]
    fn corrupt_head_meta_blocks_turn_and_file_commits_without_staging() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        meta::ensure_session_dir(repo.root()).unwrap();
        std::fs::write(
            meta::path_in(repo.root()),
            r#"{"layout":"v1","line":"session","session":"bad","runtime":"codex","cwd":"/repo/one","kind":"turn"}"#,
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("corrupt meta").unwrap();
        let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let live = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));

        let turn_error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            live.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(
            turn_error.to_string().contains("metadata"),
            "{turn_error:#}"
        );

        std::fs::write(repo.root().join("memory.md"), "shared\n").unwrap();
        let mut file_options = opts();
        file_options.message = Some("shared file".into());
        let file_error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            file_options,
            false,
            false,
        )
        .unwrap_err();
        assert!(
            file_error.to_string().contains("metadata"),
            "{file_error:#}"
        );
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join("memory.md")).unwrap(),
            "shared\n"
        );
    }

    #[test]
    fn missing_head_meta_blocks_turn_and_file_commits_without_v0_fallback() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        std::fs::write(repo.root().join("README.md"), "foreign history\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("missing meta").unwrap();
        let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let live = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));

        let turn_error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            live.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(turn_error.to_string().contains("has no"), "{turn_error:#}");

        std::fs::write(repo.root().join(meta::LOG_FILE), "user LOG bytes\n").unwrap();
        let mut file_options = opts();
        file_options.message = Some("must not default to v0".into());
        let file_error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            file_options,
            false,
            false,
        )
        .unwrap_err();
        assert!(file_error.to_string().contains("has no"), "{file_error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap(),
            "user LOG bytes\n"
        );
    }

    /// While the main checkout sits on a v0 main and the root still holds ignored user files,
    /// the v1 branch being settled gets its own worktree: the main checkout's bytes and both refs
    /// stay unchanged. An implementation that still serves a branch by switching the main
    /// checkout either overwrites the user's files or fails here.
    #[test]
    fn an_existing_v1_branch_never_touches_the_primary_checkout() {
        let (_d, repo) = setup_repo();
        let mut legacy = Meta::new_file_line();
        legacy.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &legacy).unwrap();
        std::fs::write(repo.root().join(".gitignore"), "LOG\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("v0 main").unwrap();

        repo.git(&["switch", "-c", "v1"]).unwrap();
        storage::write_snapshot(repo.root(), "", "").unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("v1 target").unwrap();
        let v1_before = repo.git(&["rev-parse", "refs/heads/v1"]).unwrap();
        repo.switch("main").unwrap();
        let main_before = repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
        std::fs::write(repo.root().join(meta::LOG_FILE), "ignored user bytes\n").unwrap();

        let checkout = checkout_for_settlement(&repo, "alice/photo", "v1")
            .unwrap()
            .unwrap();
        assert!(checkout.is_linked_worktree());
        assert_eq!(checkout.current_branch().as_deref(), Some("v1"));
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::LOG_FILE)).unwrap(),
            "ignored user bytes\n"
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/main"]).unwrap(),
            main_before
        );
        assert_eq!(
            repo.git(&["rev-parse", "refs/heads/v1"]).unwrap(),
            v1_before
        );
    }

    #[test]
    fn failed_v0_turn_checkout_rolls_back_exactly_and_can_retry() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let mut legacy = Meta::new_session_line("codex".into(), "/repo/one".into());
        legacy.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &legacy).unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), "").unwrap();
        std::fs::write(repo.root().join(meta::LEGACY_VIEW_FILE), "").unwrap();
        repo.add_all().unwrap();
        repo.commit("legacy newborn session").unwrap();

        let head_before = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let status_before = repo
            .git(&["status", "--porcelain", "--untracked-files=all"])
            .unwrap();
        let live = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));

        super::super::plumbing::fail_next_checkout_postflight();
        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            live.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert_eq!(
            repo.git(&["status", "--porcelain", "--untracked-files=all"])
                .unwrap(),
            status_before
        );
        assert!(repo.root().join(meta::LEGACY_LOG_FILE).is_file());
        assert!(repo.root().join(meta::LEGACY_VIEW_FILE).is_file());
        assert!(!repo.root().join(meta::LOG_FILE).exists());
        assert!(!repo.root().join(meta::VIEW_FILE).exists());
        assert!(!repo.root().join(meta::EVENTS_DIR).exists());

        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            live.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        assert_ne!(repo.git(&["rev-parse", "HEAD"]).unwrap(), head_before);
        assert_eq!(
            meta::read_at_ref(&repo, "HEAD").unwrap().layout,
            meta::LayoutVersion::V1
        );
        assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
        assert!(!repo.root().join(meta::LEGACY_LOG_FILE).exists());
        assert!(!repo.root().join(meta::LEGACY_VIEW_FILE).exists());
    }

    /// A file line: the form is written in meta, and transcript input is refused.
    #[test]
    fn file_line_branch_refuses_turn_commits() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        assert!(meta::read_at_ref(&repo, "HEAD").unwrap().is_file_line());

        let text = format!("{META}\n{}{}", codex_user("question"), codex_asst("answer"));
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Precondition, "a file line refuses turns");
    }

    /// The regression test for the W1 deadlock.
    ///
    /// `agit import -b <branch>` after `agit init --seed`: the new branch grows off main's head
    /// and inherits that `line: file` meta byte for byte. An implementation that infers the form
    /// from "is there a session file" takes it for a file line and refuses it (exit code 4),
    /// which blocks the whole main flow. The form is declared by the branch itself instead — born
    /// writing `line: session`, with the identity claimed only at the first settled turn.
    #[test]
    fn a_session_branch_born_off_the_file_line_accepts_its_first_turn() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        repo.git(&["branch", "e2e"]).unwrap();
        repo.switch("e2e").unwrap();
        // What import does when it creates a branch: declare the form, leave the identity empty.
        meta::write(
            repo.root(),
            &Meta::new_session_line("codex".into(), "/repo/one".into()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("agit: claim session line e2e").unwrap();

        let mut lk = link();
        lk.branch = Some("e2e".into());
        let text = format!(
            "{META}\n{}{}",
            codex_user("verify staging"),
            codex_asst("passes")
        );
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "e2e",
            lk,
            text.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();

        assert_eq!(code, ExitCode::Ok, "a session line accepts its first turn");
        let head = meta::read_at_ref(&repo, "refs/heads/e2e").unwrap();
        assert!(head.is_session_line());
        assert!(
            meta::is_bare_id(&head.session),
            "the first turn claims the identity on the spot: {}",
            head.session
        );
        assert_eq!(head.turn, Some(1));
        // This settlement leaves main's form untouched.
        assert!(
            meta::read_at_ref(&repo, "refs/heads/main")
                .unwrap()
                .is_file_line()
        );
    }

    /// A v1 session puts the sequences in the root LOG/VIEW and the envelopes in events; meta
    /// stays under session/.
    #[test]
    fn the_session_triplet_lands_under_session_dir() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let text = format!("{META}\n{}{}", codex_user("question"), codex_asst("answer"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();

        let tracked = repo.ls_tree("HEAD");
        assert!(
            tracked.contains(&"session/meta.json".to_string()),
            "{tracked:?}"
        );
        assert!(tracked.contains(&meta::LOG_FILE.to_string()), "{tracked:?}");
        assert!(
            tracked.contains(&meta::VIEW_FILE.to_string()),
            "{tracked:?}"
        );
        assert!(
            tracked.iter().any(|p| p.starts_with("events/")),
            "{tracked:?}"
        );
        assert!(
            tracked.contains(&meta::ATTRS_FILE.to_string()),
            "{tracked:?}"
        );
        assert!(
            !tracked.contains(&meta::LEGACY_LOG_FILE.to_string()),
            "{tracked:?}"
        );
        assert!(
            !tracked.contains(&meta::LEGACY_VIEW_FILE.to_string()),
            "{tracked:?}"
        );
        assert!(
            !tracked.iter().any(|p| matches!(
                p.as_str(),
                "snapshot.json" | "transcript.jsonl" | "view.jsonl"
            )),
            "no file of the legacy layout may appear again: {tracked:?}"
        );
    }

    /// Settling writes the cwd into the link.
    ///
    /// `agit import` prints the line "working dir: filled in when a version is recorded", and
    /// settlement is what makes good on it: claude-code's index yields no cwd, so without this
    /// the link stays empty and every zero-argument command after an import (they resolve their
    /// context from the cwd) breaks.
    #[test]
    fn settling_fills_in_the_links_working_dir() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let mut lk = link();
        lk.cwd = None; // the real shape right after an import
        let text = format!("{META}\n{}{}", codex_user("question"), codex_asst("answer"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            lk,
            text.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();

        let back = crate::domain::link::get(&s, "codex", "AB").unwrap();
        let cwd = back.cwd.expect("settling must write the cwd into the link");
        assert!(!cwd.is_empty());
        // The cwd stated in the transcript is more trustworthy than the process's current dir.
        assert_eq!(cwd, "/repo/one");
        // The meta records the same cwd (the authority on "which project this memory belongs
        // to").
        assert_eq!(meta::read_at_ref(&repo, "HEAD").unwrap().cwd, "/repo/one");
    }

    /// A `-m` file commit on the file line does not turn it into a session line (the two forms
    /// never convert).
    #[test]
    fn a_file_commit_never_flips_the_line() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);

        std::fs::write(
            repo.root().join("memory.md"),
            "decision: refunds go async\n",
        )
        .unwrap();
        let mut o = opts();
        o.message = Some("memory: refund conclusion".into());
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            o,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);

        let head = meta::read_at_ref(&repo, "HEAD").unwrap();
        assert!(head.is_file_line(), "a file commit keeps the form");
        assert_eq!(head.kind, Kind::File);
    }

    /// `-m` on the file line needs no session link: `main` never claims a session, and treating
    /// "no link" as an error would leave the merge agent as the only way to change shared files.
    /// The test is **the form the branch tip declares** — a session line with no link is still a
    /// session line with no link, and this path is not a way around that.
    #[test]
    fn a_file_line_takes_a_message_commit_without_a_session_link() {
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        let dir = repo.root().to_path_buf();

        assert!(
            file_line_target(&dir, "alice/photo", "main", false, "t").is_none(),
            "without -m there is no file commit to make"
        );
        assert!(
            file_line_target(&dir, "alice/photo", "ghost", true, "t").is_none(),
            "a branch that does not exist is not a file line"
        );

        // A session line (grown off main, with meta declaring session) does not take this path.
        repo.git(&["branch", "work", "main"]).unwrap();
        let claim = format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN));
        let session = Meta::new(claim, "codex".into(), "/repo/one".into());
        let head = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
        let edits = vec![(
            meta::FILE.to_owned(),
            Some(meta::to_text(&session).unwrap().into_bytes()),
        )];
        let tree = super::super::plumbing::tree_apply_owned(&repo, &head, edits).unwrap();
        let commit =
            super::super::plumbing::commit_tree(&repo, &tree, &[&head], "session").unwrap();
        repo.git(&["update-ref", "refs/heads/work", &commit])
            .unwrap();
        assert!(
            file_line_target(&dir, "alice/photo", "work", true, "t").is_none(),
            "a session line with no link is still a session line with no link"
        );

        let target = file_line_target(&dir, "alice/photo", "main", true, "t")
            .expect("a file line plus -m is what takes this path");
        assert!(matches!(target, Target::FileLine { ref branch, .. } if branch == "main"));

        std::fs::write(repo.root().join("README.md"), "# photo\n").unwrap();
        let mut o = opts();
        o.message = Some("docs: add README".into());
        let code = settle_file_line(&dir, "alice/photo", "main", "alice", o).unwrap();
        assert_eq!(code, ExitCode::Ok);
        let head = meta::read_at_ref(&repo, "refs/heads/main").unwrap();
        assert!(head.is_file_line(), "the file line's form is unchanged");
        assert_eq!(
            repo.show_raw("refs/heads/main", "README.md").as_deref(),
            Some("# photo\n")
        );
        assert_eq!(
            repo.git(&["log", "-1", "--format=%s %an", "refs/heads/main"])
                .unwrap()
                .trim(),
            "docs: add README alice"
        );
    }

    #[test]
    fn v1_file_commits_preserve_user_attributes_and_repair_managed_blocks() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);

        for (message, user_rules) in [
            ("add binary rule", "*.bin binary\n"),
            ("replace binary rule", "*.dat binary\n"),
            ("remove binary rule", ""),
        ] {
            std::fs::write(repo.root().join(meta::ATTRS_FILE), user_rules).unwrap();
            let mut options = opts();
            options.message = Some(message.into());
            options.paths = vec![meta::ATTRS_FILE.into()];
            let code = settle_bytes(
                &s,
                &repo,
                "alice/photo",
                "main",
                link(),
                b"",
                "alice",
                options,
                false,
                false,
            )
            .unwrap();
            assert_eq!(code, ExitCode::Ok);
            let committed = repo.show_raw("HEAD", meta::ATTRS_FILE).unwrap();
            assert_eq!(
                committed,
                storage::attributes_text_strict(Some(user_rules)).unwrap()
            );
        }
        let committed = repo.show_raw("HEAD", meta::ATTRS_FILE).unwrap();
        assert!(!committed.contains("*.bin binary"));
        assert!(!committed.contains("*.dat binary"));
        assert!(committed.contains("events/**  -text -merge -diff"));
    }

    #[test]
    fn v1_file_commit_rejects_tampered_attribute_markers_before_staging() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        let malformed = "# agit:storage-v1 defaults begin\n*.bin binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), malformed).unwrap();

        let mut options = opts();
        options.message = Some("must fail".into());
        options.paths = vec![meta::ATTRS_FILE.into()];
        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("has no end marker"), "{error:#}");
        assert!(
            repo.git(&["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            malformed
        );
    }

    #[test]
    fn settling_a_turn_preserves_staged_user_attribute_edits() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let first = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));
        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            first.as_bytes(),
            "alice",
            opts(),
            true,
            false,
        )
        .unwrap();

        let user_attributes = "*.bin binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), user_attributes).unwrap();
        repo.git(&["add", "--", meta::ATTRS_FILE]).unwrap();
        let staged_before = repo
            .git(&["diff", "--cached", "--", meta::ATTRS_FILE])
            .unwrap();
        let grown = format!("{first}{}{}", codex_user("second"), codex_asst("done"));

        settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            grown.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(meta::read_at_ref(&repo, "HEAD").unwrap().turn, Some(2));
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            user_attributes
        );
        assert_eq!(
            repo.git(&["diff", "--cached", "--", meta::ATTRS_FILE])
                .unwrap(),
            staged_before
        );
        assert!(
            !repo
                .show_raw("HEAD", meta::ATTRS_FILE)
                .unwrap()
                .contains("*.bin binary")
        );
    }

    #[test]
    fn first_v1_turn_normalizes_each_dirty_attribute_layer_without_losing_user_diffs() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let first = format!("{META}\n{}{}", codex_user("first"), codex_asst("answer"));
        let committed = "*.committed binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), committed).unwrap();
        seed_v0_session(&repo, &first);

        let staged = "*.staged binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), staged).unwrap();
        repo.git(&["add", "--", meta::ATTRS_FILE]).unwrap();
        let worktree = "*.worktree binary\n";
        std::fs::write(repo.root().join(meta::ATTRS_FILE), worktree).unwrap();
        assert_eq!(
            repo.git(&["status", "--short", "--", meta::ATTRS_FILE])
                .unwrap(),
            "MM .gitattributes"
        );

        let grown = format!("{first}{}{}", codex_user("second"), codex_asst("done"));
        let old_head = repo.git(&["rev-parse", "HEAD"]).unwrap();
        super::super::plumbing::fail_next_checkout_postflight();
        let error = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            grown.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error:#}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]).unwrap(), old_head);
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            staged.as_bytes()
        );
        assert_eq!(
            std::fs::read(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            worktree.as_bytes()
        );

        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            grown.as_bytes(),
            "alice",
            opts(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        assert_eq!(
            meta::read_at_ref(&repo, "HEAD").unwrap().layout,
            meta::LayoutVersion::V1
        );
        assert_eq!(
            repo.show_raw("HEAD", meta::ATTRS_FILE).unwrap(),
            storage::attributes_text_strict(Some(committed)).unwrap()
        );
        assert_eq!(
            repo.git_bytes_result(&["show", &format!(":{}", meta::ATTRS_FILE)])
                .unwrap(),
            storage::attributes_text_strict(Some(staged))
                .unwrap()
                .into_bytes()
        );
        assert_eq!(
            std::fs::read_to_string(repo.root().join(meta::ATTRS_FILE)).unwrap(),
            storage::attributes_text_strict(Some(worktree)).unwrap()
        );
        assert_eq!(
            repo.git(&["status", "--short", "--", meta::ATTRS_FILE])
                .unwrap(),
            "MM .gitattributes",
            "the original staged and unstaged user-rule differences must remain"
        );
    }

    #[test]
    fn file_commit_pathspec_ancestors_cannot_stage_managed_storage() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        init_main_file_line(&repo);
        std::fs::write(repo.root().join("memory.md"), "shared\n").unwrap();
        std::fs::write(repo.root().join(meta::LOG_FILE), "tampered\n").unwrap();
        std::fs::create_dir_all(repo.root().join("events/a/b/c/d")).unwrap();
        std::fs::write(
            repo.root()
                .join("events/a/b/c/d/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "tampered\n",
        )
        .unwrap();

        let mut options = opts();
        options.message = Some("shared only".into());
        options.paths = vec![".".into(), "session".into()];
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);

        let paths = repo.ls_tree("HEAD");
        assert!(paths.contains(&"memory.md".to_owned()), "{paths:?}");
        assert!(!paths.contains(&meta::LOG_FILE.to_owned()), "{paths:?}");
        assert!(
            !paths.iter().any(|path| path.starts_with("events/")),
            "{paths:?}"
        );
        assert_eq!(meta::read_at_ref(&repo, "HEAD").unwrap().kind, Kind::File);
    }

    #[test]
    fn v0_file_commit_keeps_new_v1_names_as_user_files() {
        let (_d, s) = store();
        let (_h, repo) = setup_repo();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = meta::LayoutVersion::V0;
        meta::write(repo.root(), &snapshot).unwrap();
        std::fs::write(repo.root().join(meta::LOG_FILE), "user log\n").unwrap();
        std::fs::write(repo.root().join(meta::VIEW_FILE), "user view\n").unwrap();
        std::fs::create_dir_all(repo.root().join(meta::EVENTS_DIR)).unwrap();
        std::fs::write(repo.root().join("events/user.txt"), "user event data\n").unwrap();
        std::fs::write(repo.root().join(meta::ATTRS_FILE), "*.bin binary\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("legacy file line").unwrap();

        std::fs::write(repo.root().join(meta::LOG_FILE), "updated user log\n").unwrap();
        let mut options = opts();
        options.message = Some("update legacy shared names".into());
        options.paths = vec![".".into()];
        let code = settle_bytes(
            &s,
            &repo,
            "alice/photo",
            "main",
            link(),
            b"",
            "alice",
            options,
            false,
            false,
        )
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        assert_eq!(
            repo.show_raw("HEAD", meta::LOG_FILE).unwrap(),
            "updated user log\n"
        );
        assert_eq!(
            repo.show_raw("HEAD", "events/user.txt").unwrap(),
            "user event data\n"
        );
        assert_eq!(
            meta::read_at_ref(&repo, "HEAD").unwrap().layout,
            meta::LayoutVersion::V0
        );
    }
}
