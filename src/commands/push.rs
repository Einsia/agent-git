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
pub(crate) use audited_push::published_visibility;
mod consent;
pub(crate) use audited_push::{
    automatic_publication_consent, automatic_publication_consent_with_client, publication_consent,
    publication_consent_with_client,
};
pub(crate) use consent::{
    ASKED_ENV, AutoConsent, PublicAnswer, answer_public_switch_on, confirms_public,
    enroll_public_destination, record_unattended, unattended,
};
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

#[derive(ClapArgs, Clone)]
pub struct Args {
    /// Target: `owner/repo@branch` (or legacy bare repo / `-b branch`). Omittable when the context resolves.
    ///
    /// The `owner/name` form is what the “several agents named X” error asks for — a bare
    /// name is ambiguous the moment this machine holds both your `payments` and a read-only
    /// checkout of somebody else’s.
    #[arg(value_name = "owner/repo@branch")]
    pub agent: Option<String>,

    /// Publish to another repository the account can write to (`owner/repo`); the checkout keeps
    /// its own repository as the default destination. For a device-local RC repository, select its
    /// retained Hub publication target.
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

    /// When the branch has diverged from the Hub's copy, publish this checkout's line as a new
    /// branch beside it (`<branch>-local`, or the next free name) and leave the Hub's as it is.
    #[arg(long, conflicts_with_all = ["all", "dry_run", "to", "to_hub", "audit"])]
    pub keep_both: bool,
}

/// `--keep-both` on a branch the Hub's copy hasn't moved away from: there is nothing to keep
/// beside it, and nothing was sent.
#[derive(Debug)]
pub(crate) struct NotDiverged {
    pub(crate) target: String,
}

impl std::fmt::Display for NotDiverged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        let branch = self.target.split_once('@').map_or("", |(_, branch)| branch);
        write!(
            f,
            "{branch} hasn't diverged from the Hub, so there is nothing to keep beside it; `agit push {}` pushes it",
            quote_arg(&self.target)
        )
    }
}

impl std::error::Error for NotDiverged {}

/// Whether a push refused as diverged may go on to keep both: one branch of this checkout published
/// to its own repository, by a person or with `--keep-both`. A preview publishes nothing, so it
/// never forks, and a push to another repository or Hub would find "this checkout's line" under
/// the destination's name, which is another checkout's.
fn offers_keep_both(args: &Args, automatic: bool) -> bool {
    !automatic
        && !args.all
        && args.branch.len() <= 1
        && !args.dry_run
        && !args.audit
        && args.to.is_none()
        && args.to_hub.is_none()
}

/// A push refused as diverged, kept both ways: with `--keep-both`, or when the person at the
/// terminal says yes. This checkout's line is forked as the branch the refusal named and that
/// branch is pushed; the Hub's branch stays as it is. `-y` and `AGIT_YES` never answer the
/// question: publishing a branch nobody named takes the flag or a typed yes.
fn keep_both(
    args: Args,
    refusal: &anyhow::Error,
    diverged: &crate::domain::privacy_git::sessions::Diverged,
) -> CmdResult {
    let repository = &diverged.repository;
    let kept = format!("{repository}@{}", diverged.keep_both);
    let client = crate::hub::Client::from_env_with_timeout(std::time::Duration::from_secs(20));
    let readers = readers_of(&client, repository)
        .unwrap_or_else(|| super::project::visibility_pointer(repository));
    if !args.keep_both {
        super::fix::register_terminal_error(refusal);
        ui::error(&super::terminal_error_message(refusal));
        let code = super::terminal_error_code(refusal, ExitCode::Failure);
        if std::env::var_os("AGIT_YES").is_some() || !ui::prompt::may_prompt() {
            return Ok(code);
        }
        let asked = ui::prompt::confirm(
            &format!(
                "Publish this checkout's line as {kept} ({readers}), and leave {} as the Hub has it?",
                diverged.branch
            ),
            false,
        )?;
        if asked != Some(true) {
            return Ok(code);
        }
    }
    let base = match super::fork::resolve_base(
        &format!("{repository}@{}", diverged.branch),
        &std::env::current_dir()?,
    )? {
        Some(base) => base,
        None => return Ok(ExitCode::Ref),
    };
    if super::fork::fork_branch(
        &base,
        &format!("{repository}@{}", diverged.branch),
        &diverged.keep_both,
    )?
    .is_none()
    {
        return Ok(ExitCode::Precondition);
    }
    // With --keep-both nothing asked, so the branch it creates is named before the push's own
    // confirmation asks to publish it.
    if args.keep_both {
        ui::info(format_args!(
            "keeping both: this checkout's line goes up as {kept} ({readers}), and {} stays as the Hub has it",
            diverged.branch
        ));
    }
    let pushed = publish(Args {
        agent: Some(kept.clone()),
        branch: Vec::new(),
        keep_both: false,
        ..args
    })?;
    if pushed == ExitCode::Ok {
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        println!(
            "published this checkout's line as {kept} ({readers}); {} stays as the Hub has it. To continue from your line: `agit resume {}`",
            diverged.branch,
            quote_arg(&kept)
        );
    }
    Ok(pushed)
}

/// Who can read `repository`, as the Hub says it now; `None` when it doesn't answer.
fn readers_of(client: &crate::hub::Client, repository: &str) -> Option<String> {
    let (owner, name) = repository.split_once('/')?;
    let remote = client.get_agent(owner, name).ok()?;
    let account = client.credential_username().unwrap_or_default();
    let (org, members_read) = super::project::owner_audience(client, owner, &account);
    super::project::readers(repository, Some(&remote.visibility), org, members_read)
}

/// Publication rejects inherited Git routing before storage preparation or native review.
pub fn check_audit_environment() -> crate::Result<()> {
    audited_push::check_environment(std::env::vars_os())
}

/// The entry point of `agit __push-automatic`, which background spawners run through
/// [`super::auto_push::automatic_push`]: the one place that marks a publication automatic.
pub fn run_automatic(args: Args) -> CmdResult {
    super::auto_push::mark_automatic();
    // Before sign-in or any request: with automatic upload off, this child asks the Hub nothing.
    if let Some((repository, _)) = args.agent.as_deref().and_then(|a| a.split_once('@')) {
        let (owner, name) = super::parse_slug(repository)?;
        if let Some(repo) = Repo::open(crate::infra::config::repo_dir(&owner, &name)?) {
            ensure!(
                super::auto_push::publication_allowed(&repo, repository)?,
                "{}",
                super::auto_push::DISABLED_FOR_REPOSITORY
            );
        }
    }
    run(args)
}

/// A push of `owner/repo@branch` that the privacy check holds back sends nothing and leaves a note
/// `agit status` shows, with what ends the wait, until the Hub acknowledges that target's branch:
/// an automatic upload has nobody watching it. Its exit is a policy refusal, which is how an
/// upload's failure is classed as a privacy check that did not complete.
pub fn run(args: Args) -> CmdResult {
    use crate::domain::privacy::{pending, service};
    let target = args.agent.clone().filter(|target| target.contains('@'));
    if args.keep_both && args.branch.len() > 1 {
        ui::error("--keep-both keeps one branch beside the Hub's; name one");
        return Ok(ExitCode::Usage);
    }
    let again = args.clone();
    let result = publish(args);
    if let Err(error) = &result
        && let Some(diverged) = error.chain().find_map(|cause| {
            cause.downcast_ref::<crate::domain::privacy_git::sessions::Diverged>()
        })
        && offers_keep_both(&again, super::auto_push::is_automatic())
    {
        return keep_both(again, error, diverged);
    }
    if let Err(error) = &result
        && let Some(not) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<NotDiverged>())
    {
        // Named as the person named it: the publication may carry the file line along with it.
        let named = NotDiverged {
            target: target.clone().unwrap_or_else(|| not.target.clone()),
        };
        ui::error(&named.to_string());
        return Ok(ExitCode::Precondition);
    }
    let (Some(target), Err(error)) = (target, &result) else {
        return result;
    };
    let Some(waiting) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<service::Pending>())
    else {
        return result;
    };
    let reason = service::reason_name(waiting.reason);
    pending::upload::record(&target, reason);
    super::fix::register_terminal_error(error);
    ui::error(&super::terminal_error_message(error));
    for hint in pending::steps(reason).unwrap_or_else(|| vec!["the next upload tries again".into()])
    {
        ui::hint(&hint);
    }
    Ok(ExitCode::Policy)
}

/// The records a push reads its branch holds from. Records that can't be read hold every branch
/// of the push, so it is refused with why; no records at all hold nothing.
fn hold_records(
    store: crate::Result<Option<crate::domain::store::Store>>,
) -> Result<Option<crate::domain::store::Store>, String> {
    store.map_err(|error| {
        format!(
            "nothing was uploaded: agit couldn't read its own records of which branches may hold \
             another conversation's turns ({error:#})"
        )
    })
}

fn publish(mut args: Args) -> CmdResult {
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
        && (crate::commands::auto_push::is_automatic()
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
        if let Some(step) = super::migration::interrupted_step(&error) {
            ui::hint(step);
        }
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
    let folder_session = super::status::folder_saved_session(&cwd)
        .filter(|(slug, _)| *slug == checkout.slug())
        .map(|(_, branch)| branch);
    let branches = match plan_branches(
        &explicit_branches,
        args.all,
        ctx_branch.as_deref(),
        &heads,
        folder_session
            .as_deref()
            .map(|branch| (checkout.slug(), branch)),
    ) {
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
    // A branch an unconfirmed Codex link named may hold another conversation's turns: it is
    // never published, automatically or on request.
    let store = match hold_records(crate::domain::store::Store::open()) {
        Ok(store) => store,
        Err(refusal) => {
            ui::error(&refusal);
            return Ok(ExitCode::Policy);
        }
    };
    let mut held = Vec::new();
    let mut open = Vec::new();
    for branch in branches {
        match store
            .as_ref()
            .and_then(|store| crate::domain::link::launch::held(store, &checkout.slug(), &branch))
        {
            Some(hold) => held.push((branch, hold)),
            None => open.push(branch),
        }
    }
    let branches = open;
    for (branch, hold) in &held {
        if asked_explicitly {
            ui::error(&format!(
                "`{}@{branch}` will not be uploaded from this computer.",
                checkout.slug()
            ));
            ui::hint(&crate::domain::link::launch::explain(hold));
            ui::hint("its local history stays; nothing was sent to the Hub");
            return Ok(ExitCode::Policy);
        }
        ui::info(format_args!(
            "{}",
            ui::dim(&format!(
                "  skipping {branch} (it may hold another conversation's turns)"
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
                // An exclusion says which one, and how to let the session in where naming it does.
                let exclusion = if reason == super::project::Withholding::Excluded {
                    super::project::exclusion(&checkout.slug(), &branch)
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let why = match &exclusion {
                    Some(super::project::Exclusion::Earlier { folder, session }) => format!(
                        "it ran before agit was on in {}. To upload it: `agit save {session}` in that folder",
                        ui::tilde(folder)
                    ),
                    Some(super::project::Exclusion::Dropped { folder, session }) => format!(
                        "you stopped uploading it from {} (`agit project drop`). To upload it anyway: `agit save {session}` in that folder",
                        ui::tilde(folder)
                    ),
                    Some(super::project::Exclusion::Delegated) => {
                        "it is a delegated session, and the conversation that started it keeps those out".to_owned()
                    }
                    None => reason.describe().to_owned(),
                };
                ui::error(&format!(
                    "`{}@{branch}` {WITHHELD_HERE}: {why}.",
                    checkout.slug(),
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
    // A session its folder left out goes only where a push names its branch.
    if !asked_explicitly {
        publishable.retain(|branch| {
            let left_out =
                super::project::folder_refuses_upload(&repo, &checkout.slug(), branch, false)
                    .unwrap_or(true);
            if left_out {
                ui::info(format_args!(
                    "{}",
                    ui::dim(&format!("  skipping {branch} (its folder left it out)"))
                ));
            }
            !left_out
        });
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
            if asked_explicitly {
                report_nothing_new(
                    &repo,
                    &checkout.slug(),
                    &checkout.slug(),
                    &unchanged,
                    &audited_push::on_the_hub(&unchanged),
                );
            } else {
                ui::info(format_args!("{}: already up to date.", checkout.slug()));
            }
            return Ok(ExitCode::Ok);
        }
    }
    if !args.dry_run {
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

    // Files an older agit filled with placeholders publish as they are; say how to repair them.
    for branch in branches
        .iter()
        .filter(|branch| crate::domain::meta::is_file_line_at(&repo, branch))
    {
        if let Some(hint) = super::repair::hint(&repo, &checkout.slug(), branch) {
            ui::warning(&hint);
        }
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
    let code = publish_in_groups(
        &args,
        &publication_client,
        &publication_account,
        &checkout,
        &repo,
        &branches,
        asked_explicitly,
    )?;
    // A push the person made is where their agent may learn the folder could upload by itself.
    if code == ExitCode::Ok
        && !args.dry_run
        && args.to_hub.is_none()
        && !crate::rc::local_repository::publication::is_device_local(&repo).unwrap_or(true)
        && let Ok(cwd) = std::env::current_dir()
    {
        let destination = args.to.clone().unwrap_or_else(|| checkout.slug());
        super::front::offer_automatic_upload(&publication_client, &cwd, &destination);
    }
    Ok(code)
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
        // Whether the branch diverged is the Hub's answer, never a receipt's.
        || args.keep_both
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
        && !super::auto_push::is_automatic()
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

/// How a push says that a session branch is larger than the Hub accepts. Project capture reads it
/// in a failed child's message to tell the person why, so it is one fixed phrase.
pub(crate) const OVERSIZED: &str = "holds more event entries than the Hub accepts";

/// How a push says that a folder's capture rule keeps a branch back, read by project capture as
/// [`OVERSIZED`] is; the next turn meets it again.
pub(crate) const WITHHELD_HERE: &str = "will not be uploaded from this computer";

/// How an automatic push says that the folder its conversation ran in doesn't upload it by
/// itself, read by the upload's record as [`OVERSIZED`] is.
pub(crate) const FOLDER_KEEPS_IT: &str =
    "is not uploaded automatically: the folder it ran in left it out, or doesn't upload by itself";

/// How a push says that a session branch builds on a session withdrawn on the Hub, read by
/// project capture as [`OVERSIZED`] is.
pub(crate) const BUILDS_ON_WITHDRAWN: &str = "builds on a session withdrawn on the Hub";

/// How an automatic push says that automatic upload into a public repository was never switched
/// on, read by project capture as [`OVERSIZED`] is.
pub(crate) const PUBLIC_NOT_SWITCHED_ON: &str = "is not switched on";

/// How a push says that the Hub refused it for what it carries, so the same push meets the same
/// refusal. Project capture reads it as [`OVERSIZED`] is, and offers no retry for it.
pub(crate) const REFUSED_AGAIN: &str = "the Hub refuses it again until that changes";

/// How a refusal that a newer agit resolves says what to do first, read as [`REFUSED_AGAIN`] is.
/// Only a production build upgrades itself; any other channel is pointed at where its builds
/// come from, so no message names a command that cannot run in this install.
pub(crate) fn upgrade_first() -> &'static str {
    if crate::infra::config::is_production_release() {
        UPGRADE_FIRST
    } else {
        UPGRADE_FIRST_ELSEWHERE
    }
}

const UPGRADE_FIRST: &str = "run `agit upgrade`, then";
const UPGRADE_FIRST_ELSEWHERE: &str = "install a newer agit from the GitLab pipeline that built this one (this channel does not upgrade itself), then";

/// What the Hub says when an encrypted publication carries a Claude Code record its public
/// projection does not support yet.
pub(crate) const UNSUPPORTED_BLOCK: &str =
    "privacy public session contains an unsupported message block";

/// How a push says the Hub refused an encrypted publication for [`UNSUPPORTED_BLOCK`], in
/// privacy's words. A newer agit publishes it; pushing again does not.
pub(crate) const ENCRYPTED_REFUSED: &str = "This session's encrypted publication was refused by the Hub (unsupported Claude Code record); it will publish once agit is updated.";

/// The failure of reading `repository`'s branch list from the Hub, which every publication starts
/// with, named with its cause. One the next try may get past (a timeout, a dropped connection)
/// is a network failure, so a background retry keeps trying; one the Hub refused is not.
pub(crate) fn branch_list_unread(
    repository: &str,
    cause: crate::hub::git::AdvertisementUnread,
) -> anyhow::Error {
    let transient = cause.transient;
    let error = anyhow::Error::new(cause).context(format!(
        "cannot read the Hub's branch list for {repository}"
    ));
    if transient {
        super::remote_request::<()>(Err(error)).unwrap_err()
    } else {
        error
    }
}

/// Whether a failed push's message says that the same push meets the same refusal.
pub(crate) fn refused_for_good(message: &str) -> bool {
    [
        REFUSED_AGAIN,
        UPGRADE_FIRST,
        UPGRADE_FIRST_ELSEWHERE,
        OVERSIZED,
        BUILDS_ON_WITHDRAWN,
        ENCRYPTED_REFUSED,
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

/// Branches one publication carries. A publication's fixed cost (destination checks, the ref
/// advertisement, snapshot planning) is paid once per publication, so many branches share one.
/// The plan's head limit also counts `main` and branches a selection carries, so a group stays
/// well below it.
const BRANCHES_PER_PUBLICATION: usize = 48;

/// Event bytes one publication may make the Hub read. Its view check reads each event object the
/// push carries once, within a budget of two of the largest sessions agit records (the Hub's
/// `materialize::DEFAULT_PAIR_BUDGET_BYTES`); a group of large sessions past it is refused whole.
const VIEW_BYTES_PER_PUBLICATION: u64 = 2 * crate::domain::storage::MAX_MATERIALIZED_BYTES as u64;

/// What `branch`'s head makes the Hub read in a publication's view check, at most: its event
/// objects, and its LOG and VIEW, which hold the whole transcript in the older layout. A branch
/// that cannot be read counts nothing; its publication says why.
fn view_bytes(repo: &Repo, branch: &str) -> u64 {
    use crate::domain::meta;
    let Ok(listing) = repo.git_bytes_result(&[
        "ls-tree",
        "-r",
        "-l",
        "-z",
        &format!("refs/heads/{branch}"),
        "--",
        meta::EVENTS_DIR,
        meta::LOG_FILE,
        meta::VIEW_FILE,
        meta::LEGACY_LOG_FILE,
        meta::LEGACY_VIEW_FILE,
    ]) else {
        return 0;
    };
    listing
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            // `<mode> blob <oid> <size>\t<path>`; the size is right-aligned after the oid.
            let header = std::str::from_utf8(entry.split(|byte| *byte == b'\t').next()?).ok()?;
            let mut fields = header.split_whitespace();
            (fields.nth(1)? == "blob").then_some(())?;
            fields.nth(1)?.parse::<u64>().ok()
        })
        .sum()
}

/// `branches` cut into publications in order: each takes at most [`BRANCHES_PER_PUBLICATION`]
/// branches, and as many as fit together within `budget` by `bytes`. A branch over the budget on
/// its own goes alone.
fn publication_groups(
    branches: &[String],
    bytes: impl Fn(&str) -> u64,
    budget: u64,
) -> Vec<&[String]> {
    let mut groups = Vec::new();
    let (mut start, mut used) = (0, 0u64);
    for (index, branch) in branches.iter().enumerate() {
        let size = bytes(branch);
        if index > start
            && (index - start == BRANCHES_PER_PUBLICATION || used.saturating_add(size) > budget)
        {
            groups.push(&branches[start..index]);
            (start, used) = (index, 0);
        }
        used = used.saturating_add(size);
    }
    if start < branches.len() {
        groups.push(&branches[start..]);
    }
    groups
}

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
    let yes = audited_push::Yes::from_environment();
    // The command read the project policy just before this, and that read covers only the push's
    // first publication. Every later one reads it again: its retry, a branch of a failed group
    // published on its own, and a later group.
    let first = std::cell::Cell::new(true);
    // A repository that appeared where this push meant to create one ends the whole push when
    // nobody can be asked about it: a later group, or a branch published on its own, would find
    // the repository there and could publish into it on answers given for creating it.
    let stopped = std::cell::Cell::new(false);
    let publish = |group: &[String]| {
        let attempt = |attempt| {
            audited_push::run(
                args,
                client.clone(),
                account,
                checkout.clone(),
                repo.clone(),
                group,
                named,
                attempt,
                &yes,
                first.replace(false),
            )
        };
        // Exactly one more attempt, from the start: a refusal of that one is reported as any other.
        match attempt(audited_push::Attempt::First) {
            Err(error) => match error.downcast::<audited_push::RefsMoved>() {
                Ok(moved) => {
                    ui::info(format_args!("{moved}; previewing it again"));
                    attempt(moved.retry())
                }
                Err(error) => match error.downcast::<audited_push::RepositoryExists>() {
                    // Only a person answers for publishing into a repository they did not
                    // create: neither a `--yes` given for the creation nor automatic upload does.
                    Ok(exists)
                        if !crate::commands::auto_push::is_automatic() && ui::prompt::can_ask() =>
                    {
                        ui::info(format_args!("{exists}; asking again"));
                        yes.withdraw();
                        attempt(audited_push::Attempt::First)
                    }
                    Ok(exists) => {
                        stopped.set(true);
                        Err(crate::commands::InteractionRequired(format!(
                            "{exists}; run `agit push` again at a terminal to decide whether to publish into it"
                        ))
                        .into())
                    }
                    Err(error) => Err(error),
                },
            },
            outcome => outcome,
        }
    };
    if branches.len() == 1 {
        return publish(branches);
    }
    let mut failed: Vec<(String, String, ExitCode)> = Vec::new();
    let groups = publication_groups(
        branches,
        |branch| view_bytes(repo, branch),
        VIEW_BYTES_PER_PUBLICATION,
    );
    for group in groups {
        let outcome = publish(group);
        if stopped.get() {
            return outcome;
        }
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
            let outcome = publish(std::slice::from_ref(branch));
            if stopped.get() {
                return outcome;
            }
            match outcome {
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
    if ui::prompt::can_ask() {
        super::repo::tell_public_reach(true);
    }
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
    folder_session: Option<(String, &str)>,
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
            // The one conversation saved from this folder is named, never chosen: a push names
            // what it publishes.
            hints: folder_session
                .filter(|(_, branch)| heads.iter().any(|head| head == branch))
                .map(|(slug, branch)| {
                    format!("the conversation saved from this folder: agit push {slug} -b {branch}")
                })
                .into_iter()
                .chain([
                    "agit push <owner>/<repo> -b <branch>".into(),
                    "or publish all local branches: agit push <owner>/<repo> --all".into(),
                ])
                .collect(),
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

/// What a push that found nothing new on `branches` of the checkout `slug`, published to
/// `repository`, says: for each session branch, which session it is (its first turn's subject, cut short) and when its last turn was,
/// and when that isn't the conversation running in this terminal, that too. A near-identical
/// branch pushed by mistake then doesn't read as an upload. The text is local display only: it
/// goes to the terminal, never into JSON or telemetry.
pub(super) fn report_nothing_new(
    repo: &Repo,
    slug: &str,
    repository: &str,
    branches: &[String],
    held: &str,
) {
    // A script reading the JSON gets no prompt words: they are for a person at the terminal.
    let words = !crate::commands::json::is_capturing();
    let mut said = false;
    for branch in branches {
        let Some(session) = describe_session(repo, branch) else {
            continue;
        };
        said = true;
        match session.first.filter(|_| words) {
            Some(first) => ui::info(format_args!(
                "Nothing new to upload: {branch} is on the Hub as the session \"{first}\" (last turn {}).",
                session.last
            )),
            None => ui::info(format_args!(
                "Nothing new to upload: {branch} is on the Hub (last turn {}).",
                session.last
            )),
        }
    }
    if !said {
        ui::info(format_args!(
            "{repository}: already up to date; {held} on the Hub."
        ));
        return;
    }
    let terminal = super::context::runtime_sessions();
    for branch in branches {
        if let Some(note) =
            super::context::not_this_terminal(&format!("{slug}@{branch}"), &terminal)
        {
            ui::hint(&note);
        }
    }
}

/// A session branch as a push names it.
struct Described {
    /// Its first turn's subject, cut short; `None` when no turn carries one.
    first: Option<String>,
    /// When its last turn was, as the local clock reads it.
    last: String,
}

/// `branch` as a push names it, read from its commits' session records in either storage layout:
/// its own session's turns only, not those of a session it was forked from, and not the commits
/// that record no turn (a milestone, a fork, a shared file). `None` when it holds no turn of its
/// own session.
fn describe_session(repo: &Repo, branch: &str) -> Option<Described> {
    let reference = format!("refs/heads/{branch}");
    let lines = repo.git_opt(&[
        "log",
        "--first-parent",
        "--format=%H%x00%ct%x00%s",
        &reference,
    ])?;
    let commits: Vec<(String, i64, String)> = lines
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\0');
            let oid = fields.next()?.to_owned();
            let at = fields.next()?.parse().ok()?;
            Some((oid, at, fields.next().unwrap_or_default().to_owned()))
        })
        .collect();
    let oids: Vec<String> = commits.iter().map(|(oid, _, _)| oid.clone()).collect();
    let records = meta::at_refs(repo, &oids);
    let session = records
        .iter()
        .flatten()
        .next()
        .filter(|record| !record.is_file_line() && !record.session.is_empty())?
        .session
        .clone();
    // A turn is named by the first commit that carries its ordinal, and dated by the last one that
    // settled it: one turn can settle as several commits, when a background task's notification
    // or a goal's continuation lands after its reply. A milestone, a fork and a file change are
    // recorded as file commits, so they neither name nor date a turn. The subject is the person's
    // own words and says nothing about which kind a commit is.
    let mut openings = std::collections::BTreeMap::new();
    let mut settled = std::collections::BTreeMap::new();
    for ((_, at, subject), record) in commits.iter().zip(&records).rev() {
        let Some(record) = record else { continue };
        if record.session == session
            && record.kind == meta::Kind::Turn
            && let Some(turn) = record.turn
        {
            openings.entry(turn).or_insert((turn, subject.as_str()));
            settled.insert(turn, *at);
        }
    }
    let (_, last) = settled.iter().next_back()?;
    let (_, (turn, opening)) = openings.iter().next()?;
    // A turn with no words has exactly the subject `agit: turn #N`.
    let first = (*opening != format!("agit: turn #{turn}") && !opening.trim().is_empty())
        .then(|| crate::adapter::preview::shorten(opening, 60));
    let at = chrono::DateTime::from_timestamp(*last, 0)?.with_timezone(&chrono::Local);
    Some(Described {
        first,
        last: when(at, chrono::Local::now()),
    })
}

/// `at` as a person reads it from `now`: today or yesterday with the time, else the date.
fn when(at: chrono::DateTime<chrono::Local>, now: chrono::DateTime<chrono::Local>) -> String {
    let days = (now.date_naive() - at.date_naive()).num_days();
    match days {
        0 => format!("today {}", at.format("%H:%M")),
        1 => format!("yesterday {}", at.format("%H:%M")),
        _ => at.format("%Y-%m-%d %H:%M").to_string(),
    }
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

    /// A session branch is named by its own turns, in either storage layout: a fork's opening is
    /// its own first turn, not the session it was forked from; its last turn is the commit that
    /// settled it, not a later milestone or file commit; and a v0 session reads the same way. One
    /// that read its parent's opening, the tip's time, or only the v1 path named the wrong
    /// conversation, said a quiet session was active today, or fell back to "already up to date".
    #[test]
    fn a_session_branch_is_named_by_its_own_turns() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(temp.path()).unwrap();
        repo.git(&["config", "user.name", "t"]).unwrap();
        repo.git(&["config", "user.email", "t@example.com"])
            .unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let day = |d: i64| 1_793_000_000 + d * 86_400;
        let commit = |session: &str,
                      kind: meta::Kind,
                      turn: Option<u32>,
                      layout: meta::LayoutVersion,
                      subject: &str,
                      at: i64| {
            let mut record = meta::Meta::new(session.into(), "claude-code".into(), "/w".into());
            record.kind = kind;
            record.turn = turn;
            record.layout = layout;
            meta::write(repo.root(), &record).unwrap();
            let log = match layout {
                meta::LayoutVersion::V0 => meta::LEGACY_LOG_FILE,
                meta::LayoutVersion::V1 => meta::LOG_FILE,
            };
            std::fs::write(repo.root().join(log), format!("{subject}\n")).unwrap();
            repo.git(&["add", "-A"]).unwrap();
            let date = format!("@{at} +0000");
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.root())
                .args(["commit", "-q", "--allow-empty", "-m", subject])
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .status()
                .unwrap();
        };
        let parent = format!("agit-{}", "a".repeat(40));
        let fork = format!("agit-{}", "b".repeat(40));
        let (v0, v1) = (meta::LayoutVersion::V0, meta::LayoutVersion::V1);
        repo.git(&["checkout", "-q", "-b", "parent"]).unwrap();
        commit(
            &parent,
            meta::Kind::Turn,
            Some(1),
            v1,
            "investigate the old problem",
            day(1),
        );
        commit(
            &parent,
            meta::Kind::Turn,
            Some(2),
            v1,
            "and a second look",
            day(2),
        );
        repo.git(&["checkout", "-q", "-b", "fork"]).unwrap();
        commit(&fork, meta::Kind::Turn, None, v1, "agit: fork fork", day(3));
        commit(
            &fork,
            meta::Kind::Turn,
            Some(1),
            v1,
            "implement the new plan",
            day(4),
        );
        // The same turn settles again when a background task's notification wakes the agent.
        commit(
            &fork,
            meta::Kind::Turn,
            Some(1),
            v1,
            "a background task finished",
            day(6),
        );
        // A milestone is recorded as a file commit, as `agit commit --milestone` writes it.
        commit(
            &fork,
            meta::Kind::File,
            Some(1),
            v1,
            "agit: milestone for turn #1",
            day(9),
        );
        commit(
            &fork,
            meta::Kind::File,
            Some(1),
            v1,
            "agit: shared notes",
            day(10),
        );
        let named = describe_session(&repo, "fork").unwrap();
        assert_eq!(named.first.as_deref(), Some("implement the new plan"));
        let settled = chrono::DateTime::from_timestamp(day(6), 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(named.last, when(settled, chrono::Local::now()));

        repo.git(&["checkout", "-q", "--orphan", "older"]).unwrap();
        repo.git(&["rm", "-rqf", "."]).unwrap();
        commit(
            &parent,
            meta::Kind::Turn,
            Some(1),
            v0,
            "an older layout's opening",
            day(5),
        );
        let named = describe_session(&repo, "older").unwrap();
        assert_eq!(named.first.as_deref(), Some("an older layout's opening"));

        // A person's words can start like agit's own: they still name and date the turn.
        let worded = format!("agit-{}", "c".repeat(40));
        repo.git(&["checkout", "-q", "--orphan", "worded"]).unwrap();
        repo.git(&["rm", "-rqf", "."]).unwrap();
        commit(
            &worded,
            meta::Kind::Turn,
            Some(1),
            v1,
            "agit: tidy the release notes",
            day(7),
        );
        let named = describe_session(&repo, "worded").unwrap();
        assert_eq!(named.first.as_deref(), Some("agit: tidy the release notes"));
        let settled = chrono::DateTime::from_timestamp(day(7), 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(named.last, when(settled, chrono::Local::now()));
    }

    /// A last turn reads as today or yesterday with its time, and anything older by its date.
    #[test]
    fn a_last_turn_reads_as_today_yesterday_or_its_date() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 7, 9, 30, 0)
            .unwrap();
        let at = |d, h, m| {
            chrono::Local
                .with_ymd_and_hms(2026, 11, d, h, m, 0)
                .unwrap()
        };
        assert_eq!(when(at(7, 8, 5), now), "today 08:05");
        assert_eq!(when(at(6, 18, 40), now), "yesterday 18:40");
        assert_eq!(when(at(4, 12, 0), now), "2026-11-04 12:00");
    }

    /// Hold records that can't be read refuse the push; no records at all hold nothing.
    #[test]
    fn unreadable_hold_records_refuse_the_push() {
        let refused = hold_records(Err(anyhow::anyhow!("permission denied"))).unwrap_err();
        assert!(refused.contains("nothing was uploaded"), "{refused}");
        assert!(refused.contains("permission denied"), "{refused}");
        assert!(matches!(hold_records(Ok(None)), Ok(None)));
    }

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

    /// Keeping both forks this checkout's line and publishes it, so only a push of one branch to
    /// its own repository offers it. A preview that offered it would fork during a dry run and
    /// then call the preview published; a push to another repository or Hub would fork that
    /// destination's checkout instead of this one.
    #[test]
    fn only_a_push_of_one_branch_to_its_own_repository_offers_to_keep_both() {
        let push = || Args {
            to: None,
            to_hub: None,
            agent: Some("alice/app@work".into()),
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
            keep_both: false,
        };
        assert!(super::offers_keep_both(&push(), false));
        assert!(!super::offers_keep_both(&push(), true), "automatic");
        for (what, args) in [
            (
                "dry run",
                Args {
                    dry_run: true,
                    ..push()
                },
            ),
            (
                "audit",
                Args {
                    audit: true,
                    ..push()
                },
            ),
            (
                "another repository",
                Args {
                    to: Some("alice/dest".into()),
                    ..push()
                },
            ),
            (
                "another Hub",
                Args {
                    to_hub: Some("https://hub.example".into()),
                    ..push()
                },
            ),
            (
                "every branch",
                Args {
                    all: true,
                    ..push()
                },
            ),
            (
                "several branches",
                Args {
                    branch: vec!["a".into(), "b".into()],
                    ..push()
                },
            ),
        ] {
            assert!(!super::offers_keep_both(&args, false), "{what}");
        }
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
            keep_both: false,
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
        // The owner takes the hub's lowercase spelling, so the local directory and the remote
        // path use the same one.
        assert_eq!(
            split_publish_target("Einsia/agent-git-dev").unwrap(),
            (Some("einsia".into()), "agent-git-dev".into(), None)
        );
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
        let got = plan_branches(&[], false, Some("refund-fix"), &heads, None).unwrap();
        assert_eq!(got, ["refund-fix"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Repository cardinality cannot substitute for explicit branch selection.
    #[test]
    fn several_branches_without_a_context_are_refused_not_guessed() {
        let (dir, repo) = fixture("ambig");
        let heads = repo.local_branches();
        let err = plan_branches(&[], false, None, &heads, None).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        assert!(
            err.hints.iter().any(|h| h.contains("--all")),
            "{:?}",
            err.hints
        );
        let one = ["solo".to_string()];
        assert!(plan_branches(&[], false, None, &one, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The refusal names the one conversation saved from the folder it ran in, with the exact
    /// push, and still publishes nothing by itself. A folder session that isn't a branch here is
    /// not named.
    #[test]
    fn a_refused_push_names_the_folders_saved_conversation() {
        let heads = ["main".to_string(), "talk".to_string()];
        let err =
            plan_branches(&[], false, None, &heads, Some(("me/app".into(), "talk"))).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        assert_eq!(
            err.hints[0],
            "the conversation saved from this folder: agit push me/app -b talk"
        );
        let err =
            plan_branches(&[], false, None, &heads, Some(("me/app".into(), "gone"))).unwrap_err();
        assert!(
            !err.hints.iter().any(|hint| hint.contains("gone")),
            "{:?}",
            err.hints
        );
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
            None,
        )
        .unwrap();
        assert_eq!(got, ["refund-fix", "main"]);
        let err = plan_branches(&["nope".into()], false, None, &heads, None).unwrap_err();
        assert_eq!(err.code, ExitCode::Ref);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Source tracking refs cannot suppress generation after a privacy policy change.
    #[test]
    fn all_projects_every_branch_despite_source_tracking_refs() {
        let (dir, repo) = fixture("all");
        let heads = repo.local_branches();
        let mut got = plan_branches(&[], true, None, &heads, None).unwrap();
        got.sort();
        assert_eq!(got, ["ghost", "main", "refund-fix"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", "main"])
            .unwrap();
        let mut got = plan_branches(&[], true, None, &heads, None).unwrap();
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

    /// Branches go out in publications of at most [`BRANCHES_PER_PUBLICATION`], and large sessions
    /// are kept apart so that each publication stays within the event bytes the Hub reads for one.
    /// Grouping by count alone puts large sessions together past that budget, and the Hub refuses
    /// the whole group; a branch over the budget on its own still goes, alone.
    #[test]
    fn publications_group_branches_by_count_and_by_event_bytes() {
        const MIB: u64 = 1 << 20;
        let names = |count: usize| -> Vec<String> { (0..count).map(|i| format!("b{i}")).collect() };
        let small = names(50);
        let groups = publication_groups(&small, |_| MIB, 1024 * MIB);
        assert_eq!(
            groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
            [BRANCHES_PER_PUBLICATION, 2]
        );

        let large = names(6);
        let sizes = [600, 600, 100, 900, 2048, 10];
        let size = |branch: &str| sizes[branch[1..].parse::<usize>().unwrap()] * MIB;
        let groups = publication_groups(&large, size, 1024 * MIB);
        let named: Vec<Vec<&str>> = groups
            .iter()
            .map(|group| group.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            named,
            [
                vec!["b0"],
                vec!["b1", "b2"],
                vec!["b3"],
                vec!["b4"],
                vec!["b5"]
            ]
        );
    }

    /// A branch weighs what its head makes the Hub read: every event object, LOG and VIEW. One
    /// that weighed only the files at the top of the snapshot would see a few bytes of event ids
    /// where the Hub reads the whole transcript.
    #[test]
    fn a_branch_weighs_its_event_objects() {
        let (_dir, repo) = fixture("view-bytes");
        repo.git(&["checkout", "--quiet", "-b", "session", "main"])
            .unwrap();
        let text: String = (0..20)
            .map(|turn| {
                format!(
                    "{}\n",
                    serde_json::json!({"type":"user","message":{"role":"user","content":format!("turn {turn} {}", "x".repeat(500))}})
                )
            })
            .collect();
        let log = crate::domain::transcript::wrap_lines(
            &text,
            "claude-code",
            &format!("agit-{}", "a".repeat(40)),
        );
        crate::domain::storage::write_snapshot(repo.root(), &log, &log).unwrap();
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-m", "session"]).unwrap();
        let written: u64 = crate::domain::storage::snapshot_files(&log, &log)
            .unwrap()
            .values()
            .map(|bytes| bytes.len() as u64)
            .sum();
        assert_eq!(view_bytes(&repo, "session"), written);
        assert!(written > text.len() as u64);
        assert_eq!(view_bytes(&repo, "no-such-branch"), 0);
    }
}
