//! Publish inspected history using registered session privacy settings.
//!
//! Selected session branches retain their ancestry. Generated commits and version tags live
//! in isolated storage so private source objects cannot enter the outgoing pack.
//! Generated commits preserve source attribution, dates, message encoding and descriptions.
//! Ordinary publication inspects new objects; audit inspects the complete frozen history.

mod audit;
mod audit_report;
mod audit_workspace;
mod audited_push;
mod consent;
pub(crate) use audited_push::{
    automatic_publication_consent, automatic_publication_consent_with_client, publication_consent,
    publication_consent_with_client,
};
pub(crate) use consent::{AutoConsent, enroll_public_destination, require_terminal_for_public};
mod preview;

use super::{CmdResult, require_login};
use crate::domain::meta;
use crate::domain::repo::{self, Repo};
use crate::domain::secrets;
use crate::hub::PublishRequest;
use crate::hub::identity::{self, RemoteIdentity};
use crate::infra::{config, credentials};
use crate::{ExitCode, ui};
use anyhow::{Context, ensure};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Target: `owner/repo@branch` (or legacy bare repo / `-b branch`). Omittable when the context resolves.
    ///
    /// The `owner/name` form is what the “several agents named X” error asks for — a bare
    /// name is ambiguous the moment this machine holds both your `payments` and a read-only
    /// checkout of somebody else’s.
    #[arg(value_name = "owner/repo@branch")]
    pub agent: Option<String>,

    /// Select the retained Hub publication target for a device-local RC repository.
    #[arg(long, value_name = "owner/repo")]
    pub to: Option<String>,

    /// Explicitly deliver this repository to another Hub.
    ///
    /// This is a temporary migration escape hatch. It requires credentials already saved for
    /// the destination Hub and deliberately changes the checkout's immutable publication
    /// binding only after the destination has been reviewed and accepted.
    #[arg(long, value_name = "hub-url", conflicts_with = "to")]
    pub to_hub: Option<String>,

    /// Branch to publish (repeatable). Default: the context branch.
    #[arg(short = 'b', long, value_name = "branch")]
    pub branch: Vec<String>,

    /// Publish every settled local branch using its registered session settings.
    #[arg(long, conflicts_with = "branch")]
    pub all: bool,

    /// Ask the Hub about every selected branch, including those whose last acknowledged
    /// publication already matches their local tip.
    #[arg(long)]
    pub verify: bool,

    /// With several branches: when publishing them together fails, publish each one on its own,
    /// so a branch that cannot be published holds back only itself.
    #[arg(long)]
    pub keep_going: bool,

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

    /// Compatibility flag for older servers; local privacy never requires approval.
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
    if args.to_hub.is_some()
        && (std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV).is_some()
            || std::env::var_os(identity::EXPECTED_AGENT_ID_ENV).is_some()
            || std::env::var_os(crate::domain::privacy_receipt::SUPERVISOR_RESULT_ENV).is_some())
    {
        ui::error(
            "--to-hub requires a direct, interactive publication and cannot be used by automatic or supervised push",
        );
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
    let _progress = ui::publication::Progress::start();
    let reading = ui::publication::stage("Reading publication target and local history");
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
                if !args.dry_run {
                    crate::domain::privacy::service::schedule_sync(client.base());
                }
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
    // A branch that was born but has not settled a single turn is not published. See
    // [`has_settled_turns`].
    //
    // **Only the inferred ones are skipped.** A branch the user named with `-b <branch>` is not
    // skipped quietly: that is the one they meant, and a silent skip that still exits 0 reads as
    // a successful publish. That case says why, and ends in failure.
    let asked_explicitly = !explicit_branches.is_empty();
    let settled = settled_turns(&repo, &branches);
    let (branches, unsettled): (Vec<String>, Vec<String>) = branches
        .into_iter()
        .partition(|b| settled.get(b).copied().unwrap_or(true));
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
    let mut publishable = Vec::new();
    let verdicts = match super::project::withheld_all(&checkout.slug(), &branches) {
        Ok(verdicts) => verdicts.into_iter().map(Ok).collect::<Vec<_>>(),
        Err(error) => branches
            .iter()
            .map(|_| Err(anyhow::anyhow!("{error:#}")))
            .collect(),
    };
    for (branch, verdict) in branches.into_iter().zip(verdicts) {
        match verdict {
            Ok(None) => publishable.push(branch),
            Ok(Some(reason)) if asked_explicitly => {
                ui::error(&format!(
                    "`{}@{branch}` will not be uploaded from this computer: {}.",
                    checkout.slug(),
                    reason.describe()
                ));
                ui::hint("its local history stays; nothing was sent to the Hub");
                if reason == super::project::Withholding::UnknownStart {
                    ui::hint(&format!(
                        "to stop it from recording into this branch: `agit project drop {}@{branch}`",
                        checkout.slug()
                    ));
                }
                return Ok(ExitCode::Policy);
            }
            Ok(Some(reason)) => ui::info(format_args!(
                "{}",
                ui::dim(&format!("  skipping {branch} ({})", reason.describe()))
            )),
            Err(error) => {
                ui::error(&format!(
                    "cannot verify the project policy of `{}@{branch}`: {error:#}",
                    checkout.slug()
                ));
                return Ok(ExitCode::Precondition);
            }
        }
    }
    let mut branches = publishable;
    if let Some(unchanged) =
        acknowledged_unchanged(&args, &repo, &client, &branches, asked_explicitly)
    {
        if !unchanged.is_empty() {
            ui::info(format_args!(
                "{}",
                ui::dim(&format!(
                    "  {} unchanged since their acknowledged publication: {}",
                    unchanged.len(),
                    unchanged.join(", ")
                ))
            ));
        }
        branches.retain(|branch| !unchanged.contains(branch));
        if branches.is_empty() {
            reading.finish();
            if !args.dry_run {
                crate::domain::privacy::service::schedule_sync(client.base());
            }
            ui::info(format_args!("{}: already up to date.", checkout.slug()));
            return Ok(ExitCode::Ok);
        }
    }
    if !args.dry_run {
        crate::domain::privacy::service::schedule_sync(client.base());
        // Dictionary sync runs detached, so push is where its refusals become visible.
        if let Some(refusal) = crate::domain::privacy::sync::recorded_refusal(client.base()) {
            let (said, next) = refusal.describe(chrono::Utc::now().timestamp());
            ui::warning(&said);
            ui::hint(&next);
        }
    }
    if branches.is_empty() {
        ui::info("nothing to publish yet — no turns have been settled.");
        if !ui::quiet() {
            ui::hint("`agit commit` records the conversation so far, then push");
        }
        return Ok(ExitCode::Ok);
    }

    let (publication_client, publication_account) = if let Some(destination) = &args.to_hub {
        let hub = identity::normalize_hub(destination)?;
        ensure!(
            hub != identity::normalize_hub(client.base())?,
            "--to-hub must name a different Hub"
        );
        let credential = credentials::load_checked(&hub)?.context(format!(
            "no saved credentials for {hub}; sign in first with `AGIT_HUB_URL={hub} agit login`"
        ))?;
        let account = credential.username.clone();
        (
            crate::hub::Client::for_credential(&hub, &credential),
            account,
        )
    } else {
        (client, me.clone())
    };

    reading.finish();
    publish_in_groups(
        &args,
        &publication_client,
        &publication_account,
        &checkout,
        &repo,
        &branches,
        asked_explicitly,
    )
}

/// Selected branches whose local tip is the source of the publication the Hub last acknowledged
/// for them, decided from local records alone; `None` when this push must ask the Hub about all.
///
/// A receipt is saved only after the Hub acknowledged every ref of a publication, and an
/// acknowledged commit stays published: the Hub refuses rewriting it. So a branch whose tip still
/// equals its receipt's source has nothing to send, unless a tag changed since: the publication
/// carries every tag the branch reaches, so the tags must also be as they were when it was frozen.
/// What local records cannot show is a branch removed or replaced on the Hub since; `--verify`
/// asks the Hub about every branch.
///
/// `main` carries no receipt: it is the shared file line, published alongside session branches.
/// While it differs from what the destination last listed for it, every branch takes the full
/// path, so a session push still carries an advanced `main`.
///
/// A push that names its branches while the destination has refs set aside as withdrawn takes the
/// full path too: naming a branch is what asks the Hub again for those refs, and a set-aside tag
/// can be in the listing a receipt acknowledged, so the shortcut would report the branch up to
/// date without asking. A push that names no branch, or an automatic one, never asks again: it
/// leaves set-aside refs out on either path, so it keeps the shortcut. Whatever decides that a
/// push asks again has to decide here too.
fn acknowledged_unchanged(
    args: &Args,
    repo: &Repo,
    client: &crate::hub::Client,
    branches: &[String],
    named: bool,
) -> Option<Vec<String>> {
    use crate::domain::privacy_receipt::{
        PublicationReceipt, acknowledged_tags, outbox::Entry, tag_listing,
    };
    if args.verify
        || args.dry_run
        || args.audit
        || args.show_preview
        || args.to.is_some()
        || args.to_hub.is_some()
        || std::env::var_os(crate::domain::privacy_receipt::SUPERVISOR_RESULT_ENV).is_some()
    {
        return None;
    }
    let identity = identity::read(repo).ok()??;
    if identity.hub != identity::normalize_hub(client.base()).ok()? {
        return None;
    }
    if named
        && std::env::var_os(super::auto_push::AUTOMATIC_ENV).is_none()
        && !crate::domain::privacy_git::withheld_refs(repo, &identity)
            .ok()?
            .is_empty()
    {
        return None;
    }
    // One listing answers every tip: a lookup per branch costs a Git process each.
    let tips: std::collections::BTreeMap<String, String> = repo
        .git_opt(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/remotes/origin/main",
            "refs/tags",
        ])?
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once(' ')?;
            Some((name.to_owned(), oid.to_owned()))
        })
        .collect();
    let tags = tag_listing(
        tips.iter()
            .filter(|(name, _)| name.starts_with("refs/tags/"))
            .map(|(name, oid)| (name.as_str(), oid.as_str())),
    );
    let tip = |reference: &str| tips.get(reference).cloned();
    if let Some(main) = tip("refs/heads/main")
        && tip("refs/remotes/origin/main").as_deref() != Some(main.as_str())
    {
        return None;
    }
    let pending = Entry::scan(repo).ok()?;
    Some(
        branches
            .iter()
            .filter(|branch| branch.as_str() != "main")
            .filter(|branch| {
                let Ok(Some(receipt)) = PublicationReceipt::load(repo, branch) else {
                    return false;
                };
                receipt.destination == identity
                    && tip(&format!("refs/heads/{branch}")).as_deref()
                        == Some(receipt.source.as_str())
                    && acknowledged_tags(repo, branch, &receipt.source)
                        .is_ok_and(|acknowledged| acknowledged.as_deref() == Some(tags.as_str()))
                    && !pending.iter().any(|entry| entry.awaits(&receipt))
            })
            .cloned()
            .collect(),
    )
}

/// Branches one publication carries. A publication's fixed cost (destination checks, the ref
/// advertisement, snapshot planning) is paid once per publication, so many branches share one.
/// The plan's head limit also counts `main` and branches a selection carries, so a group stays
/// well below it.
const BRANCHES_PER_PUBLICATION: usize = 48;

/// Publish `branches` in as few publications as the head limit allows.
///
/// Without `--keep-going` the first failing group ends the push, as one publication of the whole
/// selection would: a refusal of what the person selected never becomes a partial publication
/// they did not ask for. With it, a failing group is published again one branch at a time, so a
/// branch that cannot be published (a pending privacy check, a refused upload) holds back only
/// itself. Publishing is idempotent: a branch its group already delivered is reported up to date
/// the second time. `named` says the person named the branches, which is what asks the Hub
/// again for a branch it set aside as withdrawn; every group carries it.
fn publish_in_groups(
    args: &Args,
    client: &crate::hub::Client,
    account: &str,
    checkout: &super::clone::Checkout,
    repo: &Repo,
    branches: &[String],
    named: bool,
) -> CmdResult {
    let publish = |group: &[String]| {
        audited_push::run(
            args,
            client.clone(),
            account,
            checkout.clone(),
            repo.clone(),
            group,
            named,
        )
    };
    if branches.len() == 1 {
        return publish(branches);
    }
    let mut failed: Vec<(String, String, ExitCode)> = Vec::new();
    for group in branches.chunks(BRANCHES_PER_PUBLICATION) {
        let outcome = publish(group);
        let failure = match &outcome {
            Ok(ExitCode::Ok) => continue,
            Ok(code) => (format!("exit {}", code.as_i32()), *code),
            Err(error) => (format!("{error:#}"), ExitCode::Precondition),
        };
        if !args.keep_going {
            return outcome;
        }
        if group.len() == 1 {
            failed.push((group[0].clone(), failure.0, failure.1));
            continue;
        }
        ui::warning(&format!(
            "publishing {} branches together failed ({}); publishing them one at a time",
            group.len(),
            failure.0
        ));
        for branch in group {
            match publish(std::slice::from_ref(branch)) {
                Ok(ExitCode::Ok) => {}
                Ok(code) => failed.push((branch.clone(), format!("exit {}", code.as_i32()), code)),
                Err(error) => {
                    ui::error(&format!("{}@{branch}: {error:#}", checkout.slug()));
                    failed.push((branch.clone(), format!("{error:#}"), ExitCode::Precondition));
                }
            }
        }
    }
    let Some((_, _, code)) = failed.first().cloned() else {
        return Ok(ExitCode::Ok);
    };
    ui::error(&format!(
        "{} of {} branches were not published: {}",
        failed.len(),
        branches.len(),
        failed
            .iter()
            .map(|(branch, _, _)| branch.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    ui::hint("the other branches were published; each listed branch says why above");
    Ok(code)
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
/// [`has_settled_turns`] for each of `branches`, reading every branch's metadata in one Git
/// process. A branch the batch cannot answer is judged on its own.
fn settled_turns(repo: &Repo, branches: &[String]) -> std::collections::BTreeMap<String, bool> {
    let batched = (|| -> crate::Result<std::collections::BTreeMap<String, bool>> {
        let mut input = String::new();
        for branch in branches {
            anyhow::ensure!(
                !branch.contains(['\n', '\r']),
                "branch name cannot be listed"
            );
            input.push_str(&format!("refs/heads/{branch}:{}\n", meta::FILE));
        }
        let mut file = tempfile::tempfile()?;
        use std::io::{Seek as _, Write as _};
        file.write_all(input.as_bytes())?;
        file.rewind()?;
        let output = repo.git_with_stdin_file(&["cat-file", "--batch"], file)?;
        let mut rest = output.as_bytes();
        let mut verdicts = std::collections::BTreeMap::new();
        for branch in branches {
            let end = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(rest.len());
            let header = std::str::from_utf8(&rest[..end])?;
            rest = rest.get(end + 1..).unwrap_or_default();
            if header.ends_with(" missing") {
                // No metadata is left to the server, as for a single branch.
                verdicts.insert(branch.clone(), true);
                continue;
            }
            let size: usize = header
                .rsplit(' ')
                .next()
                .context("malformed metadata batch")?
                .parse()?;
            // The output arrives with trailing whitespace trimmed, which can shorten only the
            // last body; a body cut anywhere else fails to parse below.
            let body = rest.get(..size).unwrap_or(rest);
            rest = rest.get(size + 1..).unwrap_or_default();
            let settled = meta::parse_strict(std::str::from_utf8(body)?, branch)
                .map(|m| !m.is_session_line() || meta::is_bare_id(&m.session))
                .unwrap_or(true);
            verdicts.insert(branch.clone(), settled);
        }
        Ok(verdicts)
    })();
    batched.unwrap_or_else(|_| {
        branches
            .iter()
            .map(|branch| (branch.clone(), has_settled_turns(repo, branch)))
            .collect()
    })
}

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
            to: None,
            to_hub: None,
            agent: None,
            branch: vec![],
            all: false,
            keep_going: false,
            verify: false,
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
