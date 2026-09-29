//! Publish inspected history in the destination's fixed encryption mode.
//!
//! Selected session branches retain their ancestry. Generated commits and version tags live
//! in isolated storage so native Git metadata and source objects cannot enter the outgoing pack.
//! Ordinary and audited publication inspect the same frozen objects before confirmation.

mod audit;
mod audit_report;
mod audit_workspace;
mod audited_push;
mod consent;
pub(crate) use audited_push::automatic_publication_consent;
mod preview;

use super::{CmdResult, require_login};
use crate::domain::meta;
use crate::domain::repo::{self, Repo};
use crate::domain::secrets;
use crate::hub::PublishRequest;
use crate::hub::identity::{self, RemoteIdentity};
use crate::infra::{config, credentials};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::Path;

#[derive(ClapArgs)]
pub struct Args {
    /// Target: `owner/repo@branch` (or legacy bare repo / `-b branch`). Omittable when the context resolves.
    ///
    /// The `owner/name` form is what the “several agents named X” error asks for — a bare
    /// name is ambiguous the moment this machine holds both your `payments` and a read-only
    /// checkout of somebody else’s.
    #[arg(value_name = "owner/repo@branch")]
    pub agent: Option<String>,

    /// Publish to a separate repository without rebinding the source; local RC retains its confirmed target.
    #[arg(long, value_name = "owner/repo")]
    pub to: Option<String>,

    /// Publish a separate copy from local RC without replacing its confirmed publication target.
    #[arg(long, requires = "to")]
    pub separate: bool,

    /// Branch to publish (repeatable). Default: the context branch.
    #[arg(short = 'b', long, value_name = "branch")]
    pub branch: Vec<String>,

    /// Publish every settled local branch; encrypted mode selects session branches only.
    #[arg(long, conflicts_with = "branch")]
    pub all: bool,

    /// First publish only: only you and the authorized can read it.
    ///
    /// This is what a non-interactive run defaults to. Visibility is settled once, at
    /// first publish: `agit push` never changes an existing agent’s visibility —
    /// otherwise one fat-fingered push could flip what a teammate just made public.
    /// Change it later with `agit repo visibility <owner>/<name> <public|private>`.
    #[arg(long, conflicts_with = "public")]
    pub private: bool,

    /// First publish only: anyone can read the published content.
    #[arg(long)]
    pub public: bool,

    /// Encryption for a new destination; an existing repository's mode is fixed.
    #[arg(long, require_equals = true, value_name = "true|false")]
    pub encryption: Option<bool>,

    /// Accept secret findings in ordinary mode; encrypted publication requires a clean projection.
    #[arg(long)]
    pub allow_secrets: bool,

    /// Review all outgoing readable content in an interactive agent before a separate confirmation.
    #[arg(long)]
    pub audit: bool,

    /// Check without publishing. With --audit, the model review still runs and receives content.
    #[arg(long)]
    pub dry_run: bool,

    /// Expand readable snapshot content after the summary; binary payloads are summarized.
    #[arg(long)]
    pub show_preview: bool,
}

/// Publication rejects inherited Git routing before storage preparation or native review.
pub fn check_audit_environment() -> crate::Result<()> {
    audited_push::check_environment(std::env::vars_os())
}

pub fn run(mut args: Args) -> CmdResult {
    if args.audit && !ui::prompt::can_ask() {
        ui::error(
            "push --audit requires a person at an interactive terminal (not an agent session) for review and final confirmation",
        );
        return Ok(ExitCode::Interactive);
    }
    if let Err(error) = check_audit_environment() {
        ui::error(&error.to_string());
        return Ok(ExitCode::Usage);
    }
    if crate::rc::harness::settlement_is_delegated() {
        ui::error(crate::rc::harness::SUPERVISED_SETTLEMENT_MESSAGE);
        ui::hint("finish the turn and let agitd push it under the live identity lease");
        return Ok(ExitCode::Precondition);
    }
    // The context resolves once: it answers both "which repo" and "which branch", and both
    // answers must come from the same resolution — otherwise "repo from the context, branch
    // from the checkout" is a half-right, half-wrong combination.
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if args.agent.is_none() && args.branch.is_empty() && !args.all {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                let Some(picked) = crate::tui::screens::history::pick(&cwd, "agit push")? else {
                    return Ok(ExitCode::Ok);
                };
                args.agent = Some(picked.target());
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }
    let client = require_login()?;
    let Some(me) = credentials::current_user() else {
        ui::error("no account name in the stored credentials.");
        ui::hint("re-run `agit login`");
        return Ok(ExitCode::Auth);
    };
    let ctx = super::context::resolve(&cwd).ok();

    // ── 1. Decide which agent to push ──
    //
    // A missing or rejected process identity cannot be replaced by local repository discovery.
    let (want_owner, agent, target_branch) = match &args.agent {
        Some(a) => match split_publish_target(a) {
            Ok(v) => v,
            Err(e) => {
                ui::error(&super::terminal_error_message(&e));
                return Ok(ExitCode::Usage);
            }
        },
        None => match ctx.as_ref().and_then(|c| c.owner_name().ok()) {
            // When the context speaks, follow it — the same source `agit commit` uses.
            Some((o, n)) => (Some(o), n, None),
            None => {
                ui::error(
                    "no explicit publish repository; provide owner/repo or set AGIT_SESSION.",
                );
                ui::hint("agit push <owner>/<repo>@<branch>");
                return Ok(ExitCode::Usage);
            }
        },
    };

    let Some(checkout) = pick_checkout(&me, want_owner.as_deref(), &agent)? else {
        return Ok(if ui::prompt::can_ask() {
            ExitCode::Ref
        } else {
            ExitCode::Interactive
        });
    };
    let Some(repo) = Repo::open(&checkout.path) else {
        ui::error(&format!("no local repo for {}.", checkout.slug()));
        ui::hint(&format!(
            "record a first version: `agit import <session-id> --from <runtime> --into <owner>/{agent}@<branch>`"
        ));
        return Ok(match std::fs::symlink_metadata(&checkout.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ExitCode::Ref,
            _ => ExitCode::Precondition,
        });
    };

    if let Err(error) =
        super::migration::check_readonly_repo_startup_local(&repo.clone().local_objects_only())
    {
        ui::error(&format!("push requires settled local storage: {error:#}"));
        return Ok(ExitCode::Precondition);
    }

    let _snapshot = match meta::resolve(repo.root()) {
        Ok(v) => v,
        Err(e) => {
            ui::error(&format!(
                "no readable session metadata in {}: {e:#}",
                checkout.slug()
            ));
            ui::hint(&format!("record one first: `agit commit {agent}`"));
            return Ok(ExitCode::Precondition);
        }
    };

    // ── 2. Local preconditions ──
    //
    // Repository and branch selection must be valid before declaration synchronization or
    // publication can act on the selected destination.
    //
    // The context branch counts only when the context names **this** repo: running
    // `agit push notes` from a session in `me/payments` names a branch that belongs to another
    // repo.
    if target_branch.is_some() && (!args.branch.is_empty() || args.all) {
        ui::error("a branch in `<owner>/<repo>@<branch>` cannot be combined with `-b` or `--all`.");
        ui::hint("choose one target spelling, e.g. `agit push owner/repo@branch`");
        return Ok(ExitCode::Usage);
    }
    let explicit_branches = target_branch
        .into_iter()
        .chain(args.branch.iter().cloned())
        .collect::<Vec<_>>();
    let ctx_branch = ctx
        .as_ref()
        .filter(|c| c.repo == checkout.slug())
        .map(|c| c.branch.clone());
    let heads = repo.local_branches();
    let branches = match plan_branches(&explicit_branches, args.all, ctx_branch.as_deref(), &heads)
    {
        Ok(b) => b,
        Err(r) => {
            if r.code == ExitCode::Ok {
                ui::info(&r.msg);
                if !ui::quiet() {
                    for hint in &r.hints {
                        ui::hint(hint);
                    }
                }
            } else {
                ui::error(&r.msg);
                for hint in &r.hints {
                    ui::hint(hint);
                }
            }
            return Ok(r.code);
        }
    };
    let selection_source = match (
        args.agent.is_some(),
        !explicit_branches.is_empty() || args.all,
    ) {
        (true, false) if ctx_branch.is_some() => super::echo::Source::Mixed,
        (true, _) => super::echo::Source::Explicit,
        (false, true) => super::echo::Source::Mixed,
        (false, false) => super::echo::Source::Environment,
    };
    // A branch that was born but has not settled a single turn is not published. See
    // [`has_settled_turns`].
    //
    // **Only the inferred ones are skipped.** A branch the user named with `-b <branch>` is not
    // skipped quietly: that is the one they meant, and a silent skip that still exits 0 reads as
    // a successful publish. That case says why, and ends in failure.
    let asked_explicitly = !explicit_branches.is_empty();
    let (branches, unsettled): (Vec<String>, Vec<String>) = branches
        .into_iter()
        .partition(|b| has_settled_turns(&repo, b));
    if !unsettled.is_empty() {
        if asked_explicitly {
            ui::error(&format!(
                "`{}` has no settled turns yet — there is nothing to publish on it.",
                unsettled.join("`, `")
            ));
            ui::hint("`agit commit` records the conversation so far, then push");
            return Ok(ExitCode::Precondition);
        }
        ui::info(format_args!(
            "{}",
            ui::dim(&format!(
                "  skipping {} (claimed, nothing settled onto it yet)",
                unsettled.join(", ")
            ))
        ));
    }
    if branches.is_empty() {
        ui::info("nothing to publish yet — no turns have been settled.");
        if !ui::quiet() {
            ui::hint("`agit commit` records the conversation so far, then push");
        }
        return Ok(ExitCode::Ok);
    }

    audited_push::run(
        &args,
        client,
        &me,
        checkout,
        repo,
        &branches,
        selection_source,
    )
}

/// Normalize the unified human-facing target.  The old bare repo spelling is
/// still accepted; `@branch` selects exactly one branch for this push.
fn split_publish_target(arg: &str) -> crate::Result<(Option<String>, String, Option<String>)> {
    let parsed = crate::commands::target::parse(arg)?;
    if parsed.tail != crate::domain::refs::Tail::None {
        anyhow::bail!("push accepts a branch target, not a historic selector: `{arg}`");
    }
    // Legacy bare agent name (`agit push payments`) is parsed by refs as a
    // context-local base rather than a repository selector.
    if parsed.repo.is_none()
        && parsed.tail == crate::domain::refs::Tail::None
        && let Some(ref name) = parsed.base
        && name != "@"
    {
        repo::valid_name(name)?;
        return Ok((None, name.clone(), None));
    }
    let (owner, name) = match parsed.repo {
        Some(repo) if repo.contains('/') => {
            let (owner, name) = super::parse_slug(&repo)?;
            super::canonical_owner(&owner)?;
            (Some(owner), name)
        }
        Some(name) => (None, name),
        None => anyhow::bail!("push target must name a repository"),
    };
    repo::valid_name(&name)?;
    let branch = match parsed.base.as_deref() {
        None => None,
        Some("@") => anyhow::bail!("push target must name a branch explicitly"),
        Some(branch) => Some(branch.to_string()),
    };
    Ok((owner, name, branch))
}

/// Where the first-publish visibility comes from. Priority: command-line flag > repo preference
/// (`agit init --private`) > global `push.visibility` > ask at first publish. The closer a
/// statement is to this one push, the more it weighs: a one-off flag beats the preference the
/// repo recorded, and the repo's preference beats a default set for every repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Flag(bool),
    Repo(bool),
    Global(bool),
    Ask,
}

impl Wanted {
    /// `Some(true)` public, `Some(false)` private, `None` ask at first publish.
    fn value(self) -> Option<bool> {
        match self {
            Wanted::Flag(v) | Wanted::Repo(v) | Wanted::Global(v) => Some(v),
            Wanted::Ask => None,
        }
    }
}

fn wanted_visibility(args: &Args, repo: &Repo) -> Wanted {
    let flag = if args.public {
        Some(true)
    } else if args.private {
        Some(false)
    } else {
        None
    };
    resolve_wanted(
        flag,
        repo.visibility_preference().as_deref(),
        super::config::get("push.visibility").as_deref(),
    )
}

fn resolve_wanted(
    flag: Option<bool>,
    repo_pref: Option<&str>,
    global_pref: Option<&str>,
) -> Wanted {
    if let Some(v) = flag {
        return Wanted::Flag(v);
    }
    if let Some(v) = parse_visibility(repo_pref) {
        return Wanted::Repo(v);
    }
    if let Some(v) = parse_visibility(global_pref) {
        return Wanted::Global(v);
    }
    Wanted::Ask
}

/// Anything but `public` / `private` (including `ask`, a typo, unset) means "not said".
fn parse_visibility(pref: Option<&str>) -> Option<bool> {
    match pref.map(str::trim) {
        Some("public") => Some(true),
        Some("private") => Some(false),
        _ => None,
    }
}

fn visibility_word(public: bool) -> &'static str {
    if public { "public" } else { "private" }
}

/// Visibility at first publish. `answer` is the result of the interactive question; `None` means
/// there was nobody to ask.
///
/// A non-interactive run defaults to **private**: publishing a complete work transcript cannot
/// be undone, and in CI there is nobody to nod for it. A default in the other direction
/// (`public: !private`) reads "said nothing" as "publish all my transcripts".
fn first_visibility(want: Option<bool>, answer: Option<bool>) -> bool {
    match (want, answer) {
        (Some(v), _) => v,
        (None, Some(v)) => v,
        (None, None) => false,
    }
}

/// Ask once at first publish. Off a tty this returns `None`.
fn ask_visibility(agent: &str) -> crate::Result<Option<bool>> {
    println!(
        "{} isn’t on the hub yet — this first push settles who can read it.",
        ui::bold(agent)
    );
    ui::prompt::confirm("make the published content readable by anyone?", false)
}

pub(super) fn branch_failure_code(out: &crate::hub::git::Outcome) -> ExitCode {
    match out.http_status() {
        Some(401) => ExitCode::Auth,
        Some(403 | 409 | 413 | 422) => ExitCode::Policy,
        Some(412 | 428) => ExitCode::Precondition,
        Some(404) => ExitCode::Ref,
        Some(500..=599) => ExitCode::Network,
        None if crate::hub::git::looks_like_auth_failure(&out.stderr) => ExitCode::Auth,
        _ => ExitCode::Failure,
    }
}

/// Pick the checkout by name. `None` means the reason has already been said.
///
/// `want_owner` is the owner spelled out in the positional argument
/// (`agit push alice/payments`) — exactly what the ambiguity hint below asks for, so once it is
/// given the question is not asked a second time.
fn pick_checkout(
    me: &str,
    want_owner: Option<&str>,
    agent: &str,
) -> crate::Result<Option<super::clone::Checkout>> {
    let mut found = super::clone::checkouts_named(me, agent)?;
    if let Some(o) = want_owner {
        found.retain(|c| c.owner == o);
        // None found still falls through: the `Repo::open` error is closer to the facts (record
        // something with `agit import` first), and inventing one here only adds a layer of
        // retelling.
        if found.is_empty() {
            return Ok(Some(super::clone::Checkout {
                owner: o.to_string(),
                name: agent.to_string(),
                path: config::repo_dir(o, agent)?,
            }));
        }
    }
    match found.as_slice() {
        [] => Ok(Some(super::clone::Checkout {
            owner: me.to_string(),
            name: agent.to_string(),
            path: config::repo_dir(me, agent)?,
        })),
        [only] => Ok(Some(only.clone())),
        many => {
            // Your own and a read-only pickup share the name; the wrong one publishes the
            // content under another agent's name.
            ui::error(&format!(
                "this machine has {} agents named `{agent}`:",
                many.len()
            ));
            for c in many {
                println!("  {}  {}", c.slug(), ui::dim(&ui::tilde(&c.path)));
            }
            ui::hint(&format!("name it in full: agit push <owner>/{agent}"));
            Ok(None)
        }
    }
}

/// Is this checkout read-only (nothing can be pushed to it).
///
/// **Both** conditions have to hold. The second is not optional: a copy that was already
/// promoted sits in your namespace with its `upstream` pointing at somebody else's copy — the
/// first condition alone settles that one, while going by "does it hold somebody else's address"
/// alone marks it read-only too, and every push then asks one more question.
pub(crate) fn is_read_only(me: &str, checkout_owner: &str, upstream: Option<&str>) -> bool {
    checkout_owner != me && upstream.is_none()
}

/// Where the remote agent lands.
struct Remote {
    owner: String,
    name: String,
    push_url: String,
    identity: RemoteIdentity,
    /// Visibility as the server records it: `public` or `private`.
    visibility: String,
    encryption_enabled: bool,
    /// A first-publication target is verified against the requested visibility before pinning.
    first_publish: bool,
}

#[derive(Debug)]
pub(super) struct FirstPublicationRefusal;

impl std::fmt::Display for FirstPublicationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the first-publication destination could not be confirmed")
    }
}

impl std::error::Error for FirstPublicationRefusal {}

struct RemotePreparation {
    pin_identity: bool,
    own_namespace: Option<bool>,
    encryption_enabled: bool,
}

fn ensure_remote_with_options(
    client: &crate::hub::Client,
    owner: &str,
    agent: &str,
    want: Option<bool>,
    snap: &meta::Meta,
    repo: &Repo,
    preparation: RemotePreparation,
) -> crate::Result<Remote> {
    let expected = identity::expected_for_transport(repo, client.base())?;
    match super::remote_request(client.get_agent(owner, agent)) {
        Ok(remote) => {
            let encryption_enabled = remote.require_encryption_enabled()?;
            anyhow::ensure!(
                encryption_enabled == preparation.encryption_enabled,
                "repository encryption mode is fixed at creation; create a different repository for the requested mode"
            );
            let observed = RemoteIdentity::new(client.base(), &remote.agent_id)?;
            identity::verify_transport_target(repo, &observed)?;
            return Ok(Remote {
                owner: remote.owner,
                name: remote.name,
                push_url: remote.clone_url,
                identity: observed,
                visibility: remote.visibility,
                encryption_enabled,
                first_publish: false,
            });
        }
        Err(e)
            if e.downcast_ref::<crate::hub::client::ApiError>()
                .is_some_and(|api| api.status == 404) =>
        {
            if expected.is_some() || (preparation.pin_identity && identity::read(repo)?.is_some()) {
                return Err(
                    e.context("the RC remote is unavailable; refusing to create a replacement")
                );
            }
        }
        Err(e) => return Err(e),
    }

    // Visibility is chosen only for creation; the namespace cannot override a private choice.
    let mine = preparation
        .own_namespace
        .unwrap_or_else(|| crate::infra::credentials::current_user().as_deref() == Some(owner));
    let public = match want {
        Some(value) => value,
        None => first_visibility(None, ask_visibility(agent)?),
    };
    // The repo origin lets the server look up "which agents have worked in this repo".
    let origins: Vec<String> = snap
        .code
        .as_deref()
        .and_then(code_origin)
        .into_iter()
        .collect();

    ui::info(format_args!(
        "publishing {} to {}…",
        ui::bold(agent),
        ui::accent(client.base())
    ));
    // An organization repo names the organization to create it under; your own passes nothing,
    // and the server defaults to the caller.
    let resp = super::remote_request(client.publish(&PublishRequest {
        name: agent.to_string(),
        owner: (!mine).then(|| owner.to_string()),
        public,
        encryption_enabled: preparation.encryption_enabled,
        repo_origins: origins,
    }))?;
    let remote_identity = RemoteIdentity::new(client.base(), &resp.agent_id)?;
    let observed = super::remote_request(client.get_agent(owner, agent))?;
    let encryption_enabled = observed.require_encryption_enabled()?;
    anyhow::ensure!(
        encryption_enabled == preparation.encryption_enabled,
        "repository encryption mode is fixed at creation; create a different repository for the requested mode"
    );
    anyhow::ensure!(
        resp.push_url == observed.clone_url
            && observed.clone_url == format!("{}/{owner}/{agent}.git", remote_identity.hub),
        "created repository URL differs from the confirmed destination"
    );
    // A creation response may name a concurrently created repository. Its current audience and
    // immutable identity must agree with this publication before any identity pin or upload.
    if resp.owner != owner
        || resp.name != agent
        || observed.owner != owner
        || observed.name != agent
        || observed.agent_id != remote_identity.agent_id
    {
        return Err(anyhow::anyhow!(
            "{owner}/{agent} changed identity while preparing its first publication; nothing was uploaded"
        )
        .context(FirstPublicationRefusal));
    }
    if observed.visibility != visibility_word(public) {
        return Err(anyhow::anyhow!(
            "{owner}/{agent} is {} on the Hub, but this first publication requested {}; nothing was uploaded",
            observed.visibility,
            visibility_word(public)
        )
        .context(FirstPublicationRefusal));
    }
    // Retained pins belong to supervised work and cannot be rewritten by an ordinary push.
    if preparation.pin_identity && matches!(identity::read(repo), Ok(None)) {
        write_checkout_config(&format!("{owner}/{agent}"), || {
            identity::pin(repo, &remote_identity)
        })?;
    }
    Ok(Remote {
        owner: observed.owner,
        name: observed.name,
        push_url: observed.clone_url,
        identity: remote_identity,
        visibility: observed.visibility,
        encryption_enabled,
        first_publish: true,
    })
}

/// The human-facing spelling of visibility. Another level from the server (organization-visible,
/// say) passes through unchanged rather than being guessed at.
fn visibility_label(visibility: &str) -> &str {
    visibility
}

/// What to tell the user when this cannot go on.
#[derive(Debug)]
struct Refusal {
    msg: String,
    hints: Vec<String>,
    code: ExitCode,
}

/// Whether anything on this branch has been settled.
///
/// The branch and its `agit: claim session line` commit are created when the session is
/// **born**, while the identity (`session`) is claimed only at the first settlement — two things
/// that happen at different moments. So a legitimate intermediate state exists: a session
/// interrupted before it finished a sentence (or opened and closed again) leaves a branch
/// holding only that claim commit, declaring itself a session line while having no identity yet.
///
/// The server's provenance check (correctly) refuses such a tip, and it returns HTTP 422 — a
/// line that reads like "your content is wrong" while it only means "there is nothing here
/// yet". So it is filtered out locally: having nothing to publish is not an error.
///
/// The file line (main) is always pushable — it never claims a session, and the test does not
/// apply to it.
fn has_settled_turns(repo: &Repo, branch: &str) -> bool {
    match meta::read_at_ref(repo, &format!("refs/heads/{branch}")) {
        Some(m) => !m.is_session_line() || meta::is_bare_id(&m.session),
        // A branch with no readable meta is left to the server — nothing local judges for it.
        None => true,
    }
}

/// Decide which branches to push.
///
/// Explicit branch arguments and `--all` precede the supplied process identity. Repository
/// cardinality cannot choose a branch because an unselected line may contain private work.
fn plan_branches(
    explicit: &[String],
    all: bool,
    ctx_branch: Option<&str>,
    heads: &[String],
) -> std::result::Result<Vec<String>, Refusal> {
    if !explicit.is_empty() {
        let unknown: Vec<&str> = explicit
            .iter()
            .filter(|b| !heads.iter().any(|h| h == *b))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(Refusal {
                msg: format!("no local branch named `{}`.", unknown.join("`, `")),
                hints: vec![format!("this repo has: {}", heads.join(", "))],
                code: ExitCode::Ref,
            });
        }
        let mut out: Vec<String> = Vec::new();
        for b in explicit {
            if !out.contains(b) {
                out.push(b.clone());
            }
        }
        return Ok(out);
    }

    // Source tracking refs describe a different graph; only the generated destination refs can
    // establish that a privacy publication is already present.
    if all && !heads.is_empty() {
        return Ok(heads.to_vec());
    }

    if let Some(b) = ctx_branch {
        if heads.iter().any(|h| h == b) {
            return Ok(vec![b.to_string()]);
        }
        return Err(Refusal {
            msg: format!("the context branch `{b}` doesn’t exist in this repo."),
            hints: vec![
                format!("this repo has: {}", heads.join(", ")),
                "settle it first (`agit commit`), or name a branch with -b".into(),
            ],
            code: ExitCode::Ref,
        });
    }

    match heads {
        [] => Err(Refusal {
            msg: "this repo has no branches yet — there is nothing to publish.".into(),
            hints: vec!["record a first version: `agit commit`".into()],
            code: ExitCode::Precondition,
        }),
        _ => Err(Refusal {
            msg: "no explicit publish branch; name a branch or set AGIT_SESSION.".into(),
            hints: vec![
                "agit push <owner>/<repo> -b <branch>".into(),
                "or publish all local branches: agit push <owner>/<repo> --all".into(),
            ],
            code: ExitCode::Ref,
        }),
    }
}

/// Local configuration cannot add tag refs or publish a nested repository implicitly.
#[cfg(test)]
fn publication_push_args(refs: &[String], set_upstream: bool) -> Vec<&str> {
    let mut args = vec!["push", "--no-follow-tags", "--recurse-submodules=no"];
    if set_upstream {
        args.push("-u");
    }
    args.push("origin");
    args.extend(refs.iter().map(String::as_str));
    args
}

/// Bind the live checkout under the same guard as import and settlement configuration writes.
fn bind_publication_origin(
    repo: &Repo,
    slug: &str,
    destination: &identity::RemoteIdentity,
    url: &str,
) -> crate::Result<()> {
    if identity::read(repo)?.as_ref() == Some(destination)
        && repo.remote_is(crate::domain::repo::ORIGIN, url)
    {
        return Ok(());
    }
    write_checkout_config(slug, || {
        identity::pin(repo, destination)?;
        repo.set_remote(url)
    })
}

/// Write the checkout's own git configuration under the repository's exclusive guard, the lock
/// import and settlement hold shared while they write this checkout, so agit processes do not race
/// for git's lock on `.git/config`. The guard covers only the local write: held across the
/// network transfer, it would stall every settlement of the repository for as long as the push
/// takes.
///
/// The upstream `git push -u` records on a first publication is written outside the guard. Git
/// does not fail a push whose upstream it cannot record, and the next push records it again,
/// because a branch without an upstream is pushed with `-u`.
fn write_checkout_config<T>(
    slug: &str,
    write: impl FnOnce() -> crate::Result<T>,
) -> crate::Result<T> {
    let store = crate::domain::store::Store::open_or_init()?;
    let _guard = crate::domain::link::lock_repository_for_write(&store, slug)?;
    write()
}

/// Push tags. Batched because an agent gets a version ID every turn, and a command line has a
/// length limit.
#[cfg(test)]
fn push_tags(
    repo: &Repo,
    tags: &[String],
    identity: &RemoteIdentity,
    allow_secrets: bool,
) -> std::result::Result<(), crate::hub::git::Outcome> {
    for chunk in tags.chunks(100) {
        let specs: Vec<String> = chunk.iter().map(|t| format!("refs/tags/{t}")).collect();
        let args = publication_push_args(&specs, false);
        match crate::hub::git::push_for_remote(repo, &args, identity, allow_secrets) {
            Ok(out) if out.ok() => {}
            Ok(out) => return Err(out),
            Err(e) => {
                return Err(crate::hub::git::Outcome {
                    code: 1,
                    stderr: format!("{e:#}"),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn push_tags_for_test(
    repo: &Repo,
    tags: &[String],
    identity: &RemoteIdentity,
    allow_secrets: bool,
) -> std::result::Result<(), crate::hub::git::Outcome> {
    push_tags(repo, tags, identity, allow_secrets)
}

/// Recover the origin from `<origin>@<short-sha>`.
///
/// Split from the right: an origin holds an `@` itself (`git@github.com:o/r.git` is the most
/// common form).
fn code_origin(code: &str) -> Option<String> {
    let (origin, _) = code.rsplit_once('@')?;
    (!origin.is_empty()).then(|| origin.to_string())
}

/// Where a hit is shown — **by carrier**, not the file name for everything.
///
/// A workspace file shows its basename: the leading path helps little in locating it, and a
/// narrower table reads better.
///
/// Every other carrier is shown whole. Their label is `<type> object <sha8>[/<path>]`, and the
/// oid inside it is the handle those remedies use (`git cat-file blob <oid>`, `git log --all
/// --find-object=<oid>`) — cut down to a basename, it is gone. The blob case especially: when
/// the same file is both in the workspace and in a blob in history, the two hits share rule,
/// line number and redacted excerpt, and taking the basename for both turns them into two
/// **identical** rows that read like a bug in the report, while they are two things to handle
/// separately.
fn where_column(h: &secrets::Hit) -> String {
    let at = h.file.as_deref().unwrap_or_default();
    match h.source {
        secrets::Source::File => Path::new(at)
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default(),
        _ => at.to_string(),
    }
}

/// Truncated results remain visibly incomplete so omitted findings are not mistaken for clean data.
fn report_hits(hits: &[secrets::Hit], truncated: bool) {
    ui::section("suspected secrets");
    let rows: Vec<Vec<String>> = hits
        .iter()
        .take(20)
        .map(|h| {
            vec![
                h.rule.clone(),
                where_column(h),
                h.line.to_string(),
                // Only the redacted excerpt is shown — this output goes into CI logs.
                h.redacted.clone(),
            ]
        })
        .collect();
    println!(
        "{}",
        ui::table::render(&["rule", "at", "line", "excerpt (redacted)"], &rows)
    );
    if hits.len() > 20 {
        println!("{}", ui::dim(&format!("… {} more", hits.len() - 20)));
    }
    if truncated {
        ui::hint(&format!(
            "this list is incomplete: it shows {} findings and stops there, more remain — fix these, then run `agit push` again to see the rest",
            hits.len()
        ));
    }
}

#[cfg(test)]
mod publication_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a repo with two branches off `main`, each carrying a tag.
    fn fixture(tag: &str) -> (std::path::PathBuf, Repo) {
        let dir = std::env::temp_dir().join(format!(
            "agit-push-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = Repo::init(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "one"]).unwrap();
        repo.git(&["tag", "agit-main-one"]).unwrap();
        // Fork a session line off main; each side then takes one more step.
        repo.git(&["checkout", "--quiet", "-b", "refund-fix"])
            .unwrap();
        std::fs::write(dir.join("b.txt"), "two").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "two"]).unwrap();
        repo.git(&["tag", "agit-refund-two"]).unwrap();
        repo.git(&["checkout", "--quiet", "-b", "ghost", "main"])
            .unwrap();
        std::fs::write(dir.join("c.txt"), "three").unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "three"]).unwrap();
        repo.git(&["tag", "agit-ghost-three"]).unwrap();
        (dir, repo)
    }

    /// With nobody to ask in a non-interactive run (CI, scripts), the default is **private**.
    ///
    /// This pins the direction of the default: `public: !private` reads "said nothing" as
    /// "publish my complete work transcript", and that step cannot be undone.
    #[test]
    fn private_by_default_when_non_interactive() {
        assert!(
            !first_visibility(None, None),
            "a non-interactive run must default to private"
        );
        // On a tty the answer stands as given.
        assert!(first_visibility(None, Some(true)));
        assert!(!first_visibility(None, Some(false)));
        // A flag is not asked about, and no answer overrides it.
        assert!(first_visibility(Some(true), None));
        assert!(!first_visibility(Some(false), Some(true)));
    }

    #[test]
    fn visibility_flags_are_read_from_the_args() {
        let base = || Args {
            separate: false,
            encryption: None,
            to: None,
            agent: None,
            branch: vec![],
            all: false,
            private: false,
            public: false,
            allow_secrets: false,
            audit: false,
            dry_run: false,
            show_preview: false,
        };
        let (_d, repo) = {
            let d = tempfile::tempdir().unwrap();
            let r = Repo::init(&d.path().join("agents/alice/photo")).unwrap();
            (d, r)
        };
        assert_eq!(wanted_visibility(&base(), &repo), Wanted::Ask);
        assert_eq!(
            wanted_visibility(
                &Args {
                    private: true,
                    ..base()
                },
                &repo
            ),
            Wanted::Flag(false)
        );
        assert_eq!(
            wanted_visibility(
                &Args {
                    public: true,
                    ..base()
                },
                &repo
            ),
            Wanted::Flag(true)
        );
        // The preference `agit init --private` records in the repo is read by push.
        repo.set_visibility_preference("private").unwrap();
        assert_eq!(wanted_visibility(&base(), &repo), Wanted::Repo(false));
    }

    /// The flag beats the repo preference, which beats the global default; `ask`,
    /// typos and unset all mean "not said". An implementation that let the global
    /// default win over the repo preference would publish a repo created with
    /// `--private` publicly because of a setting made for other repos.
    #[test]
    fn visibility_preference_precedence() {
        assert_eq!(resolve_wanted(None, None, None), Wanted::Ask);
        assert_eq!(resolve_wanted(None, None, Some("ask")), Wanted::Ask);
        assert_eq!(resolve_wanted(None, None, Some("secret")), Wanted::Ask);
        assert_eq!(
            resolve_wanted(None, None, Some("public")),
            Wanted::Global(true)
        );
        assert_eq!(
            resolve_wanted(None, None, Some("private")),
            Wanted::Global(false)
        );
        assert_eq!(
            resolve_wanted(None, Some("private"), Some("public")),
            Wanted::Repo(false)
        );
        assert_eq!(
            resolve_wanted(Some(true), Some("private"), Some("private")),
            Wanted::Flag(true)
        );
        assert_eq!(Wanted::Ask.value(), None);
        assert_eq!(Wanted::Repo(false).value(), Some(false));
    }

    #[test]
    fn visibility_labels_are_not_guessed() {
        assert_eq!(visibility_label("public"), "public");
        assert_eq!(visibility_label("private"), "private");
        // Another level from the server passes through unchanged instead of being folded into
        // "private".
        assert_eq!(visibility_label("internal"), "internal");
    }

    /// The positional argument accepts the spelling its own ambiguity hint asks for.
    #[test]
    fn the_positional_takes_both_a_bare_name_and_owner_slash_name() {
        let (owner, name, branch) = split_publish_target("payments").unwrap();
        assert_eq!((owner, name, branch), (None, "payments".into(), None));
        assert_eq!(
            split_publish_target("alice/payments").unwrap(),
            (Some("alice".into()), "payments".into(), None)
        );
        assert_eq!(
            split_publish_target("alice/payments@refund").unwrap(),
            (
                Some("alice".into()),
                "payments".into(),
                Some("refund".into())
            )
        );
        // The owner must use the hub's lowercase spelling, or the local directory and the remote
        // path each use their own.
        assert!(split_publish_target("Einsia/agent-git-dev").is_err());
        // A repo name does not occupy the ref namespace: the `agit-` prefix is unambiguous in
        // the `owner/<name>` position.
        assert_eq!(
            split_publish_target("alice/agit-dev").unwrap(),
            (Some("alice".into()), "agit-dev".into(), None)
        );
        assert!(split_publish_target("a/b/c").is_err());
    }

    /// What goes up is the context branch, not the one the checkout sits on.
    ///
    /// A rejected import leaves HEAD on `ghost` while the context says `refund-fix`; an
    /// implementation that pushes the checked-out branch publishes content nobody claimed.
    #[test]
    fn the_context_branch_wins_over_the_checked_out_one() {
        let (dir, repo) = fixture("ctx");
        assert_eq!(repo.current_branch().as_deref(), Some("ghost"));
        let heads = repo.local_branches();
        let got = plan_branches(&[], false, Some("refund-fix"), &heads).unwrap();
        assert_eq!(got, ["refund-fix"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Repository cardinality cannot substitute for explicit branch selection.
    #[test]
    fn several_branches_without_a_context_are_refused_not_guessed() {
        let (dir, repo) = fixture("ambig");
        let heads = repo.local_branches();
        let err = plan_branches(&[], false, None, &heads).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        assert!(
            err.hints.iter().any(|h| h.contains("--all")),
            "{:?}",
            err.hints
        );
        let one = ["solo".to_string()];
        assert!(plan_branches(&[], false, None, &one).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `-b` pushes only the branches named; naming one that does not exist is said out loud.
    #[test]
    fn explicit_branches_are_taken_verbatim() {
        let (dir, repo) = fixture("explicit");
        let heads = repo.local_branches();
        let got = plan_branches(
            &["refund-fix".into(), "main".into()],
            false,
            Some("ghost"),
            &heads,
        )
        .unwrap();
        assert_eq!(got, ["refund-fix", "main"]);
        let err = plan_branches(&["nope".into()], false, None, &heads).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Source tracking refs cannot suppress generation after a privacy policy change.
    #[test]
    fn all_projects_every_branch_despite_source_tracking_refs() {
        let (dir, repo) = fixture("all");
        let heads = repo.local_branches();
        let mut got = plan_branches(&[], true, None, &heads).unwrap();
        got.sort();
        assert_eq!(got, ["ghost", "main", "refund-fix"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", "main"])
            .unwrap();
        let mut got = plan_branches(&[], true, None, &heads).unwrap();
        got.sort();
        assert_eq!(got, ["ghost", "main", "refund-fix"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_origin_splits_from_the_right() {
        // An ssh-form origin holds an `@` itself; splitting from the left yields "git".
        assert_eq!(
            code_origin("git@github.com:nana/OpenPad.git@1839e61").as_deref(),
            Some("git@github.com:nana/OpenPad.git")
        );
        assert_eq!(
            code_origin("https://github.com/nana/x.git@abc1234").as_deref(),
            Some("https://github.com/nana/x.git")
        );
        assert!(code_origin("no-at-sign").is_none());
    }

    /// The test for "can this be pushed at all".
    #[test]
    fn a_checkout_is_read_only_only_when_it_is_someone_elses_and_has_no_upstream() {
        // `agit clone alice/photo`: lands under alice's name, with no upstream.
        assert!(is_read_only("me", "alice", None));
        // Your own agent is never read-only.
        assert!(!is_read_only("me", "me", None));
        assert!(!is_read_only("me", "me", Some("http://h/alice/photo.git")));
        // A promoted copy lands under your name, upstream pointing at the source — pushable.
        assert!(!is_read_only("me", "me", Some("http://h/alice/photo.git")));
    }

    /// A promoted copy is not asked about again.
    ///
    /// This is why the upstream test exists: going by "does it point at somebody else's address"
    /// alone marks a perfectly good copy read-only, and every `agit push` then asks once more
    /// whether to create another one.
    #[test]
    fn an_already_promoted_copy_is_not_asked_again() {
        assert!(!is_read_only("me", "me", Some("http://h/alice/photo.git")));
        // Edge case: a collaborator keeps the repo under somebody else's name but has an
        // upstream configured — either it was promoted, or the user wired the remote up
        // themselves, and in neither case does push create another one for them.
        assert!(!is_read_only("me", "alice", Some("http://h/bob/photo.git")));
    }

    #[test]
    fn branch_failure_categories_require_known_status_or_existing_auth_evidence() {
        for (status, expected) in [
            (401, ExitCode::Auth),
            (403, ExitCode::Policy),
            (404, ExitCode::Ref),
            (409, ExitCode::Policy),
            (412, ExitCode::Precondition),
            (413, ExitCode::Policy),
            (422, ExitCode::Policy),
            (428, ExitCode::Precondition),
            (500, ExitCode::Network),
            (503, ExitCode::Network),
            (599, ExitCode::Network),
            (418, ExitCode::Failure),
        ] {
            let out = crate::hub::git::Outcome {
                code: 128,
                stderr: format!("fatal: The requested URL returned error: {status}"),
            };
            assert_eq!(branch_failure_code(&out), expected);
        }
        for (stderr, expected) in [
            ("fatal: Authentication failed", ExitCode::Auth),
            (
                "fatal: could not read Username: terminal prompts disabled",
                ExitCode::Auth,
            ),
            ("fatal: repository 'unknown' not found", ExitCode::Failure),
            ("fatal: could not read object", ExitCode::Failure),
            (
                "! [rejected] main -> main (non-fast-forward)",
                ExitCode::Failure,
            ),
            ("unexpected protocol reply", ExitCode::Failure),
        ] {
            assert_eq!(
                branch_failure_code(&crate::hub::git::Outcome {
                    code: 128,
                    stderr: stderr.into()
                }),
                expected
            );
        }
    }
}
