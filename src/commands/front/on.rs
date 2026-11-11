//! `agit on` and `agit off`: whether a folder's sessions save and upload by themselves.

use super::{Chosen, Refusal};
use crate::{ExitCode, commands::CmdResult, ui};
use anyhow::Result;
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct OnArgs {
    /// The folder to turn on (default: the current one).
    pub dir: Option<PathBuf>,
    /// Save to this repository instead of the one chosen for the folder.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,
    /// Allow the repository named with --repo to be public: every session here becomes readable
    /// by anyone.
    #[arg(long, requires = "repo")]
    pub public: bool,
}

#[derive(ClapArgs)]
pub struct OffArgs {
    /// The folder to turn off (default: the current one).
    pub dir: Option<PathBuf>,
}

pub fn on(args: OnArgs) -> CmdResult {
    super::finish(crate::commands::setup::presence::after(turn_on(args)))
}

pub fn off(args: OffArgs) -> CmdResult {
    super::finish(turn_off(args))
}

fn current_or(dir: Option<PathBuf>) -> Result<PathBuf> {
    Ok(match dir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    })
}

fn turn_on(args: OnArgs) -> CmdResult {
    let dir = current_or(args.dir)?;
    if args.repo.is_none() {
        let lifted = turn_on_bound_folders(&dir, super::held_sessions)?;
        if !lifted.is_empty() {
            return report_bound_on(&lifted);
        }
    }
    let root = super::folder(&dir)?;
    let mut again = String::from("agit on");
    if let Some(repo) = &args.repo {
        again.push_str(&format!(" --repo {repo}"));
    }
    if args.public {
        again.push_str(" --public");
    }
    let client = super::sign_in(&again)?;
    let switching = args
        .repo
        .as_deref()
        .is_some_and(|repo| super::bound(&root).is_some_and(|bound| bound != repo));
    let enabled = enable(&client, &root, args.repo.as_deref(), args.public, |slug| {
        if std::env::var_os("AGIT_YES").is_some() || !ui::prompt::may_prompt() {
            return Ok(true);
        }
        let question = format!(
            "Save and upload {}sessions in {} to {slug}{}?",
            if switching { "new " } else { "" },
            ui::tilde(&root),
            if args.public {
                " (public: anyone can read them)"
            } else {
                ""
            }
        );
        Ok(ui::prompt::confirm(&question, true)? == Some(true))
    })?;
    // An explicit `agit on` brings a policy that never looked at OpenCode's store forward, once.
    let look = crate::commands::project::look_for_earlier_opencode(&root).unwrap_or(None);
    let code = report(&root, &enabled)?;
    if !crate::commands::json::is_capturing()
        && let Some(line) = look
            .as_ref()
            .and_then(crate::commands::project::OpencodeLook::line)
    {
        println!("{line}");
    }
    Ok(code)
}

/// A folder that uploads automatically, and how it came to.
pub(crate) struct Enabled {
    pub repository: String,
    /// The repository earlier sessions stay in, when this call moved the folder elsewhere.
    pub previous: Option<String>,
    /// `None` when the folder was already on and the Hub was not asked again.
    pub visibility: Option<String>,
    pub chosen: Chosen,
    pub created: bool,
    pub already: bool,
    /// Why the team's repository was not used, and how to move there.
    pub note: Option<String>,
    /// Teams this call gave access to the organization repository it created.
    pub granted: Vec<String>,
    /// Where an owner gives teams access, for an organization repository this call created.
    pub grant_page: Option<String>,
    /// The hold on a folder that was already on that this call ended: `push_auto_off` or
    /// `public_needs_consent`.
    pub resumed: Option<&'static str>,
    /// What still holds the uploads of a folder that was already on.
    pub held: Option<crate::commands::project::Held>,
}

/// Turns `root` on: chooses the destination, refuses a public one the person did not name with
/// `public`, asks `confirm` with the destination, then creates, links and enrolls. A folder that is
/// already on is left as it is.
pub(crate) fn enable(
    client: &crate::hub::Client,
    root: &Path,
    repo: Option<&str>,
    public: bool,
    confirm: impl FnOnce(&str) -> Result<bool>,
) -> Result<Enabled> {
    let _setup = crate::commands::project::setup_lock(root)?;
    let origin = super::origin(root);
    let destination = super::destination(client, root, origin.as_deref(), repo)?;
    let slug = destination.slug();
    if destination.replaces.is_none()
        && super::bound(root).as_deref() == Some(slug.as_str())
        && crate::commands::project::uploads_automatically(root) == Some(true)
    {
        let (resumed, held) = resume_held(root)?;
        return Ok(Enabled {
            repository: slug,
            previous: None,
            visibility: None,
            chosen: destination.chosen,
            created: false,
            already: true,
            note: None,
            granted: Vec::new(),
            grant_page: None,
            resumed,
            held,
        });
    }
    // Nothing is created on the Hub for a folder that no agent here can upload from by itself.
    let claude = crate::adapter::get("claude-code").is_ok_and(|adapter| adapter.available());
    let codex = crate::adapter::get("codex").is_ok_and(|adapter| adapter.available())
        && crate::commands::setup::codex_hooks_enabled();
    if !claude && !codex {
        return Err(Refusal::new(
            "no agent on this computer can upload its sessions by itself",
            "install Claude Code, or Codex with its hooks feature enabled, then run this again",
            ExitCode::Precondition,
        )
        .into());
    }
    if super::visibility(client, &destination)?.as_deref() == Some("public")
        && !(public && destination.chosen == Chosen::Given)
    {
        return Err(public_refusal(root, &destination).into());
    }
    if !destination.asked && !confirm(&slug)? {
        return Err(Refusal::new(
            "nothing changed",
            "to save somewhere else, run `agit on --repo OWNER/NAME`",
            ExitCode::Ok,
        )
        .into());
    }
    upload_unpublished(root, &destination)?;
    let (visibility, created) = super::ensure_on_hub(client, &destination, origin.as_deref())?;
    // New sessions go to the named repository; sessions already saved stay where they are. A
    // move that does not complete leaves the folder routed as it was.
    let previous = match &destination.replaces {
        Some(_) => {
            let previous = crate::commands::project::unbind_for_move(root)?;
            if let Err(error) = crate::domain::workspace::bind(root, &slug, true) {
                previous.restore(root, &slug)?;
                return Err(error);
            }
            Some(previous)
        }
        None => None,
    };
    // Naming the destination with `--repo` is the person's own choice to upload into it, so it
    // replaces an off they recorded for it on this computer; a destination the folder chose keeps
    // that off, and the bind below leaves its automatic upload off. A bind that fails puts both
    // back, the off and the checkout's own setting the bind may already have written, so a
    // refused `agit on` turns nothing on: the checkout's setting outranks the record.
    let (owner, name) = crate::commands::parse_slug(&slug)?;
    let checkout = crate::infra::config::repo_dir(&owner, &name)?;
    let setting_before = crate::domain::repo::Repo::open(&checkout)
        .map(|repo| repo.auto_push_override())
        .transpose()?
        .flatten();
    let replaced = repo.is_some() && replace_recorded_off(&slug)?;
    if let Err(error) = bind(root, &slug) {
        if let Some(previous) = previous {
            previous.restore(root, &slug)?;
        }
        if replaced {
            undo_replaced_off(&owner, &name, &checkout, setting_before)?;
        }
        return Err(error);
    }
    let (granted, grant_page) = if created {
        grant_teams(client, &destination)
    } else {
        (Vec::new(), None)
    };
    // Once on, nothing is left to ask about.
    crate::commands::project::forget_automatic_upload_offer(root);
    Ok(Enabled {
        repository: slug,
        previous: destination.replaces,
        visibility: Some(visibility),
        chosen: destination.chosen,
        created,
        already: false,
        note: destination.note,
        granted,
        grant_page,
        resumed: None,
        held: None,
    })
}

/// For a folder that is on with its uploads held, ends the hold `agit on` can end: its
/// repository's automatic upload switched off on this computer, and a public repository's switch
/// on, which is asked or refused as `agit config` asks it. The repository's own switch moves only
/// for a person at a terminal or with `--yes`, from any session; otherwise it is named. It moves
/// as every choice that turns automatic upload on does: the checkout's own setting, then the off
/// recorded for the destination, which would otherwise outrank it; with no checkout here yet, the
/// recorded off alone. What only the person's default for repositories without a setting keeps
/// off, with no checkout to hold one, stays ([`crate::commands::project::on_switches_back`]).
/// Returns the code of the hold this ended, and what still holds the folder.
fn resume_held(
    root: &Path,
) -> Result<(Option<&'static str>, Option<crate::commands::project::Held>)> {
    use crate::commands::project::Saving;
    let Saving::Held {
        repository, held, ..
    } = crate::commands::project::saving(root)
    else {
        return Ok((None, None));
    };
    let attended = std::env::var_os("AGIT_YES").is_some() || ui::prompt::may_prompt();
    if !matches!(held.code, "push_auto_off" | "public_needs_consent")
        || (held.code == "push_auto_off" && !attended)
    {
        return Ok((None, Some(held)));
    }
    let (owner, name) = crate::commands::parse_slug(&repository)?;
    match crate::domain::repo::Repo::open(crate::infra::config::repo_dir(&owner, &name)?) {
        Some(checkout) => {
            crate::commands::push::enroll_public_destination(
                &checkout,
                &repository,
                "agit on",
                None,
            )?;
            if held.code == "push_auto_off" {
                checkout.set_auto_push(Some(true))?;
                crate::domain::repo::automatic_upload_off::set(&owner, &name, false)?;
            }
        }
        None if held.code == "push_auto_off" => {
            crate::domain::repo::automatic_upload_off::set(&owner, &name, false)?;
        }
        None => return Ok((None, Some(held))),
    }
    let still = match crate::commands::project::saving(root) {
        Saving::Held { held, .. } => Some(held),
        _ => None,
    };
    Ok((Some(held.code), still))
}

/// After creating a repository in an organization the account owns, asks once per team the
/// account belongs to whether that team may read and upload there; the default, and the answer
/// without a terminal, is no. The folder is already on, so a failed grant is reported rather than
/// undoing it. Returns the teams granted and, when the Hub has the organization's teams, the page
/// where more can be given access.
fn grant_teams(
    client: &crate::hub::Client,
    destination: &super::Destination,
) -> (Vec<String>, Option<String>) {
    let me = client.credential_username().unwrap_or_default();
    if destination.owner == me
        || !client
            .get_org(&destination.owner)
            .is_ok_and(|org| org.role == "owner")
    {
        return (Vec::new(), None);
    }
    let slug = destination.slug();
    let page = Some(super::settings_url(client, &slug));
    let Ok(teams) = client.org_teams(&destination.owner) else {
        return (Vec::new(), page);
    };
    let mut granted = Vec::new();
    if std::env::var_os("AGIT_YES").is_some() || !ui::prompt::may_prompt() {
        return (granted, page);
    }
    for team in teams
        .iter()
        .filter(|team| team.members.iter().any(|member| member.username == me))
    {
        let question = format!("Let team {} read and upload to {slug}?", team.name);
        if ui::prompt::confirm(&question, false).ok().flatten() == Some(true) {
            match client.grant_team(&destination.owner, &team.name, &destination.name) {
                Ok(_) => granted.push(team.name.clone()),
                Err(error) => ui::warning(&format!(
                    "team {} was not given access: {error:#}",
                    team.name
                )),
            }
        }
    }
    (granted, page)
}

/// A repository made with `agit init` and never uploaded has no Hub identity yet. Its first upload
/// creates the destination, or takes over an empty one, and records that identity, so it comes
/// before anything else touches the Hub; the upload itself refuses a destination with other
/// history.
fn upload_unpublished(root: &Path, destination: &super::Destination) -> Result<()> {
    let Some(repo) = crate::infra::config::repo_dir(&destination.owner, &destination.name)
        .ok()
        .and_then(crate::domain::repo::Repo::open)
    else {
        return Ok(());
    };
    if crate::hub::identity::read(&repo)?.is_some() {
        return Ok(());
    }
    run_child(
        &["push", &destination.slug(), "-b", "main", "--private"],
        root,
    )
}

fn public_refusal(root: &Path, destination: &super::Destination) -> Refusal {
    let slug = destination.slug();
    let (message, alternative) = match destination.chosen {
        Chosen::Bound => (
            format!(
                "{} is set up to save to {slug}, which is public",
                ui::tilde(root)
            ),
            format!(
                "`agit on --repo {}/{}-private`",
                destination.owner, destination.name
            ),
        ),
        _ => (
            format!("{slug} is public, so everything uploaded there can be read by anyone"),
            format!(
                "a private one: `agit on --repo {}/{}-private`",
                destination.owner, destination.name
            ),
        ),
    };
    Refusal::new(
        message,
        format!(
            "upload to {alternative}, or to {slug} knowingly: `agit on --repo {slug} --public`"
        ),
        ExitCode::Precondition,
    )
}

/// Drops the off this computer recorded for `slug` before its checkout existed, and says so.
/// Puts back the off a refused `agit on --repo` replaced, and the checkout's own setting as it was
/// before. The off goes back first and on its own: it needs no Git configuration write, so a held
/// configuration lock, which may be why the bind failed, can't keep it from returning. The setting
/// is written only when the bind changed it.
fn undo_replaced_off(
    owner: &str,
    name: &str,
    checkout: &Path,
    setting_before: Option<bool>,
) -> Result<()> {
    let off = crate::domain::repo::automatic_upload_off::set(owner, name, true);
    let setting = match crate::domain::repo::Repo::open(checkout) {
        Some(repo) if repo.auto_push_override().ok().flatten() != setting_before => {
            repo.set_auto_push(setting_before)
        }
        _ => Ok(()),
    };
    off?;
    setting
}

/// Drops the off recorded for `slug` on this computer, saying so; whether there was one.
fn replace_recorded_off(slug: &str) -> Result<bool> {
    use crate::domain::repo::automatic_upload_off;
    let (owner, name) = crate::commands::parse_slug(slug)?;
    if automatic_upload_off::recorded(&owner, &name) != automatic_upload_off::Recorded::Off {
        return Ok(false);
    }
    automatic_upload_off::set(&owner, &name, false)?;
    ui::progress(format_args!(
        "automatic upload into {slug} was turned off on this computer; naming it with --repo turns it on"
    ));
    Ok(true)
}

/// Enrolls the folder as a project that uploads automatically, through `agit project bind`, so
/// its checks (account, write access, destination identity, hook capability) all apply.
fn bind(root: &Path, repository: &str) -> Result<()> {
    let root_arg = root.to_string_lossy().into_owned();
    run_child(
        &[
            "project",
            "bind",
            &root_arg,
            "--repo",
            repository,
            "--history",
            "none",
            "--auto-upload",
        ],
        root,
    )
}

/// Runs an advanced command with every confirmation answered, and turns its failure into a
/// refusal that carries the command's own explanation.
pub(super) fn run_child(arguments: &[&str], cwd: &Path) -> Result<()> {
    let output = crate::infra::background::command(std::env::current_exe()?)
        .args(["--quiet", "--json", "--yes"])
        .args(arguments)
        .current_dir(cwd)
        .env_remove("AGIT_SESSION")
        .stdin(std::process::Stdio::null())
        .output()?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    if output.status.success() && value["ok"] == true {
        return Ok(());
    }
    let errors: Vec<String> = value["diagnostics"]["stderr"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["level"] == "error")
        .filter_map(|d| d["message"].as_str().map(str::to_owned))
        .collect();
    Err(Refusal::new(
        if errors.is_empty() {
            format!("`agit {}` failed", arguments.join(" "))
        } else {
            errors.join("; ")
        },
        "run the same command again; if it keeps failing, `agit doctor` checks this computer's setup",
        super::exit_code(output.status.code().unwrap_or(1)),
    )
    .into())
}

fn report(root: &Path, enabled: &Enabled) -> CmdResult {
    // A git worktree its main checkout covers follows that folder's setting; a main checkout
    // names the worktrees it covers, each with the sessions from before it covered them.
    let through = crate::commands::project::worktree_of(root);
    let worktrees = match &through {
        Some(_) => Vec::new(),
        None => crate::commands::project::covered_worktrees(root),
    };
    let sessions = if through.is_some() || !worktrees.is_empty() {
        super::sessions_shown_in(root).0
    } else {
        Vec::new()
    };
    let earlier = match &through {
        Some(_) => super::left_out_in(root, &sessions),
        None => crate::commands::project::earlier_count(root),
    };
    let worktrees: Vec<(std::path::PathBuf, usize)> = worktrees
        .into_iter()
        .map(|worktree| {
            let count = super::left_out_in(&worktree, &sessions);
            (worktree, count)
        })
        .collect();
    let waiting = earlier + worktrees.iter().map(|(_, count)| count).sum::<usize>();
    let runtimes = runtimes(root);
    if crate::commands::json::is_capturing() {
        println!(
            "{}",
            serde_json::json!({
                "folder": root,
                "repository": enabled.repository.as_str(),
                "visibility": enabled.visibility,
                "chosen": enabled.chosen,
                "created": enabled.created,
                "already_on": enabled.already,
                "resumed": enabled.resumed,
                "held": enabled.held.as_ref().map(crate::commands::project::Held::json),
                "through": through,
                "worktrees": worktrees.iter().map(|(folder, count)| serde_json::json!({
                    "folder": folder,
                    "earlier_sessions_not_uploaded": count,
                })).collect::<Vec<_>>(),
                "previous_repository": enabled.previous,
                "note": enabled.note,
                "teams_granted": enabled.granted,
                "grant_access_page": enabled.grant_page,
                "earlier_sessions_not_uploaded": earlier,
                "runtimes": runtimes.iter().map(|(r, state)| serde_json::json!({"runtime": r, "state": state.name(), "says": state.describe()})).chain(crate::commands::project::opencode_not_captured(root).map(|said| serde_json::json!({"runtime": "opencode", "state": "not_captured", "says": said}))).collect::<Vec<_>>(),
                // Uploading the sessions from before is the person's choice: the agent asks.
                "next": (waiting > 0).then(|| serde_json::json!({
                    "ask_user": super::earlier_note(waiting, "here", super::Reader::Agent),
                })),
            })
        );
        return Ok(ExitCode::Ok);
    }
    let folder = ui::tilde(root);
    let held_line = |held: &crate::commands::project::Held| {
        if held.code == "push_auto_off"
            && crate::commands::project::on_switches_back(&enabled.repository)
        {
            format!(
                "{folder} is on, but automatic upload to {} is off on this computer, so sessions here are saved and not uploaded; {} switches it back on{}",
                enabled.repository,
                ui::accent("agit on --yes"),
                match super::Reader::of_this_process() {
                    super::Reader::Person => "",
                    super::Reader::Agent => " only when the user asks to",
                }
            )
        } else {
            format!("{folder} is on, but its uploads are held: {}", held.text)
        }
    };
    match (&through, enabled.already) {
        (Some(main), _) => {
            ui::success(&format!(
                "{folder} is already on through {} (git worktree)",
                ui::tilde(main)
            ));
            if let Some(held) = &enabled.held {
                ui::warning(&held_line(held));
            }
        }
        (None, true) => match (enabled.resumed, &enabled.held) {
            (_, Some(held)) => ui::warning(&held_line(held)),
            (Some("push_auto_off"), None) => ui::success(&format!(
                "{folder} was on, and automatic upload to {} was off on this computer; it is back on",
                enabled.repository
            )),
            (Some(_), None) => ui::success(&format!(
                "{folder} is on, and automatic upload into {} is switched on",
                enabled.repository
            )),
            (None, None) => ui::success(&format!("{folder} is already on")),
        },
        (None, false) => ui::success(&format!("{folder} is on")),
    }
    let how = match (enabled.chosen, enabled.created) {
        (Chosen::Org, true) => " (new; its owners and granted teams can read it)",
        (_, true) => " (new)",
        (Chosen::Linked, false) => " (already holds this project's sessions on the Hub)",
        (Chosen::Team, false) => " (your team's repository for this project)",
        _ => "",
    };
    let visibility = enabled
        .visibility
        .as_deref()
        .map(|v| format!(", {v}"))
        .unwrap_or_default();
    // A held folder uploads nothing, so it names its repository without promising an upload.
    if enabled.held.is_some() {
        println!(
            "  its repository: {}{visibility}{how}",
            ui::bold(enabled.repository.as_str())
        );
    } else {
        println!(
            "  sessions here upload to {}{visibility}{how}",
            ui::bold(enabled.repository.as_str())
        );
    }
    for (worktree, _) in &worktrees {
        println!("  also covers {} (git worktree)", ui::tilde(worktree));
    }
    if let Some(previous) = &enabled.previous {
        println!(
            "  sessions saved before stay in {previous}; teammates can read them through an invite link: `agit repo invite {previous}`"
        );
    }
    if let Some(note) = &enabled.note {
        println!("  {note}");
    }
    for team in &enabled.granted {
        println!("  team {team} can read and upload there");
    }
    if let (Some(page), true) = (&enabled.grant_page, enabled.granted.is_empty()) {
        println!("  to give a team access: {page}");
    }
    for (runtime, state) in &runtimes {
        println!("  {}: {}", super::runtime_name(runtime), state.describe());
    }
    if let Some(said) = crate::commands::project::opencode_not_captured(root) {
        println!("  {said}");
    }
    let here =
        std::iter::once(("here".to_owned(), earlier)).chain(worktrees.iter().map(
            |(worktree, count)| (format!("in {} (git worktree)", ui::tilde(worktree)), *count),
        ));
    let reader = super::Reader::of_this_process();
    for (place, count) in here.filter(|(_, count)| *count > 0) {
        println!("  {}", super::earlier_note(count, &place, reader));
    }
    println!("  to stop: {}", ui::accent("agit off"));
    Ok(ExitCode::Ok)
}

#[derive(Debug, Clone)]
enum RuntimeState {
    /// Each finished turn saves and uploads.
    Automatic,
    /// Codex runs the hooks only after the person trusts them in Codex, and it did not say
    /// whether it does.
    NeedsTrust,
    /// Codex will not run one of AgentGit's hooks here, or lists none for one of their events:
    /// the line names it, what that costs and how to fix it.
    Inactive(String),
    /// Codex lists none of AgentGit's hooks, so nothing from Codex saves.
    Missing,
    /// This Codex reports no hooks; its sessions upload when caught up.
    CatchUp,
}

impl RuntimeState {
    /// The state's name in `--json`, which only gains values: `inactive` and `missing` are new.
    fn name(&self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::NeedsTrust => "needs_trust",
            Self::Inactive(_) => "inactive",
            Self::Missing => "missing",
            Self::CatchUp => "catch_up",
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Automatic => "every finished turn saves and uploads".into(),
            Self::NeedsTrust => {
                "every finished turn saves and uploads once you trust AgentGit's hooks: in Codex, open /hooks and trust both".into()
            }
            Self::Inactive(problem) => problem.clone(),
            Self::Missing => {
                "nothing is saved yet: Codex lists none of AgentGit's hooks; to add them: `agit setup --runtime codex --hooks`".into()
            }
            Self::CatchUp => {
                "this Codex has no hooks, so nothing from it is saved by itself; to upload one of its sessions here, name it: `agit save SESSION-ID`".into()
            }
        }
    }
}

/// What each agent here does with the folder's conversations. Codex is asked whether it runs
/// AgentGit's hooks in `root`: a folder can be on while Codex holds the hooks for review, and then
/// nothing from Codex saves, so its line says so rather than that every turn saves.
fn runtimes(root: &Path) -> Vec<(&'static str, RuntimeState)> {
    let mut runtimes = Vec::new();
    if crate::adapter::get("claude-code").is_ok_and(|a| a.available()) {
        runtimes.push(("claude-code", RuntimeState::Automatic));
    }
    if crate::adapter::get("codex").is_ok_and(|a| a.available()) {
        let state = if crate::commands::setup::codex_hooks_enabled() {
            codex_state(
                crate::commands::setup::codex_hooks(root)
                    .and_then(crate::commands::setup::CodexHooksAnswer::listed),
            )
        } else {
            RuntimeState::CatchUp
        };
        runtimes.push(("codex", state));
    }
    runtimes
}

/// Codex's state from what it said about AgentGit's hooks, `None` when it did not answer.
fn codex_state(hooks: Option<crate::commands::setup::CodexHooks>) -> RuntimeState {
    match hooks {
        None => RuntimeState::NeedsTrust,
        Some(hooks) if hooks.listed == 0 => RuntimeState::Missing,
        Some(hooks) => match hooks.problem() {
            Some(problem) => RuntimeState::Inactive(problem),
            None => RuntimeState::Automatic,
        },
    }
}

/// Turns back on the folders holding `dir` that the website binds as projects of this computer
/// and `agit off` turned off. The conversations each holds now stay out of its uploads until the
/// person saves them (`rc::folder_cutoff`), as they did when it was first turned on: only those
/// started from now on upload. `held` lists a folder's conversations (`runtime:session-id`).
/// Returns each folder and the repository it uploads to.
fn turn_on_bound_folders(
    dir: &Path,
    held: impl Fn(&Path) -> Result<Vec<String>>,
) -> Result<Vec<(PathBuf, Option<String>)>> {
    let mut lifted = Vec::new();
    for (folder, repo) in crate::rc::local_repository::covering(dir)? {
        if !repo.uploads_turned_off()? {
            continue;
        }
        crate::rc::folder_cutoff::record(&folder, held(&folder)?)?;
        repo.turn_uploads_off(false)?;
        let destination = crate::rc::local_repository::publication::Destination::load(&repo)?;
        if destination.is_some() {
            repo.set_auto_push(Some(true))?;
        }
        lifted.push((
            folder,
            destination.map(|destination| destination.repository),
        ));
    }
    Ok(lifted)
}

fn report_bound_on(lifted: &[(PathBuf, Option<String>)]) -> CmdResult {
    for (folder, repository) in lifted {
        if crate::commands::json::is_capturing() {
            println!(
                "{}",
                serde_json::json!({"folder": folder, "on": true, "repository": repository})
            );
            continue;
        }
        ui::success(&format!("{} is on", ui::tilde(folder)));
        match repository {
            Some(repository) => println!(
                "  sessions started from now on here upload to {repository}; the ones already here stay on this computer until you save them (`agit save`)"
            ),
            None => println!(
                "  sessions started from now on here upload once the website binds this folder again; the ones already here stay on this computer until you save them (`agit save`)"
            ),
        }
    }
    Ok(ExitCode::Ok)
}

/// Turns off the folders holding `dir` that the website binds as projects of this computer: their
/// new sessions no longer upload, whatever binds them again, until `agit on`. Returns the folders
/// turned off by this call.
fn turn_off_bound_folders(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut turned = Vec::new();
    for (folder, repo) in crate::rc::local_repository::covering(dir)? {
        if !repo.uploads_turned_off()? {
            repo.turn_uploads_off(true)?;
            turned.push(folder);
        }
    }
    Ok(turned)
}

fn turn_off(args: OffArgs) -> CmdResult {
    let dir = current_or(args.dir)?;
    let root = super::folder(&dir)?;
    let folder = ui::tilde(&root);
    let bound = turn_off_bound_folders(&dir)?;
    match crate::commands::project::uploads_automatically(&root) {
        Some(true) => {
            // A git worktree its main checkout covers is turned off on its own; the main checkout
            // stays on, and so does its repository's setting.
            let through = crate::commands::project::worktree_of(&root);
            let repository = super::bound(&root);
            let root_arg = root.to_string_lossy().into_owned();
            run_child(&["project", "unbind", &root_arg], &root)?;
            if crate::commands::json::is_capturing() {
                println!(
                    "{}",
                    serde_json::json!({"folder": root, "on": false, "through": through})
                );
            } else {
                ui::success(&format!("{folder} is off"));
                match (&through, &repository) {
                    (Some(main), Some(repository)) => println!(
                        "  new sessions in this git worktree are no longer saved or uploaded; {} still uploads to {repository}",
                        ui::tilde(main)
                    ),
                    _ => println!(
                        "  new sessions here are no longer saved or uploaded; nothing was deleted"
                    ),
                }
                println!("  to turn it back on: {}", ui::accent("agit on"));
            }
            Ok(ExitCode::Ok)
        }
        _ if !bound.is_empty() => {
            for folder in &bound {
                if crate::commands::json::is_capturing() {
                    println!("{}", serde_json::json!({"folder": folder, "on": false}));
                } else {
                    ui::success(&format!("{} is off", ui::tilde(folder)));
                    println!("  new sessions here are no longer uploaded; nothing was deleted");
                    println!("  to turn it back on: {}", ui::accent("agit on"));
                }
            }
            Ok(ExitCode::Ok)
        }
        // In a folder that doesn't upload by itself, off is the person's no to turning it on: it is
        // recorded, so nothing suggests it there again, and nothing else changes.
        _ => {
            crate::commands::project::record_automatic_upload_declined(&root)?;
            if crate::commands::json::is_capturing() {
                println!(
                    "{}",
                    serde_json::json!({"folder": root, "on": false, "declined": true})
                );
            } else {
                println!(
                    "{folder} stays off; agit won't suggest turning it on here again (`agit on` still turns it on)"
                );
            }
            Ok(ExitCode::Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeState, codex_state};

    /// `agit off` in a folder the website binds as a project stops its uploads, whatever turns
    /// its repository's automatic upload back on, and keeps out the conversations it held when it
    /// was turned on. `agit on` lifts that, and keeps out every conversation the folder holds by
    /// then, the ones that ran while it was off included: only those started afterwards upload.
    #[test]
    fn a_website_bound_folder_turned_off_and_on_uploads_only_what_starts_after() {
        if crate::rc::in_isolated_test(
            "commands::front::on::tests::a_website_bound_folder_turned_off_and_on_uploads_only_what_starts_after",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let folder = home.path().join("app");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        let repository =
            crate::rc::local_repository::ensure_repository("project", &folder).unwrap();
        let repo = crate::rc::local_repository::require(
            &crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
                .unwrap(),
        )
        .unwrap();
        repo.set_auto_push(Some(true)).unwrap();
        let uploads = || crate::commands::auto_push::publication_allowed(&repo, "ada/app").unwrap();
        let out =
            |native: &str| crate::rc::folder_cutoff::leaves_out(&folder, "claude-code", native);
        assert!(uploads());
        crate::rc::folder_cutoff::record(&folder, ["claude-code:before".to_owned()]).unwrap();

        assert_eq!(
            super::turn_off_bound_folders(&folder.join(".")).unwrap(),
            vec![folder.clone()]
        );
        assert!(!uploads(), "off stops the uploads");
        repo.set_auto_push(Some(true)).unwrap();
        assert!(
            !uploads(),
            "and a repair of the repository's own setting doesn't lift it"
        );
        assert!(out("before"), "off keeps the record");
        assert!(
            super::turn_off_bound_folders(&folder).unwrap().is_empty(),
            "already off"
        );

        // A conversation runs while the folder is off; then it is turned on.
        let lifted = super::turn_on_bound_folders(&folder, |_| {
            Ok(vec![
                "claude-code:before".to_owned(),
                "claude-code:while-off".to_owned(),
            ])
        })
        .unwrap();
        assert_eq!(lifted, vec![(folder.clone(), None)]);
        assert!(!repo.uploads_turned_off().unwrap());
        assert!(uploads(), "on lifts it");
        assert!(
            out("before") && out("while-off"),
            "what ran before on stays out"
        );
        assert!(!out("after"), "a conversation started after on uploads");
        assert!(
            super::turn_on_bound_folders(&folder, |_| unreachable!())
                .unwrap()
                .is_empty(),
            "a folder that is on is left as it is"
        );
    }

    /// What a folder holds is read from every source the background service can read, not only
    /// this process's Codex home: a conversation in another registered home is held, and would
    /// otherwise count as new once uploads are on. So is one every list leaves out but that can
    /// come back: an archived one, and one with no rollout yet. One of another folder is not. A
    /// source that can't be read refuses, rather than giving a shorter list.
    #[test]
    fn a_folder_holds_the_conversations_of_every_registered_source() {
        if crate::rc::in_isolated_test(
            "commands::front::on::tests::a_folder_holds_the_conversations_of_every_registered_source",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        unsafe {
            std::env::set_var("HOME", root.join("home"));
            std::env::set_var("AGIT_HOME", root.join("agit"));
        }
        // The default Codex home has configuration and no session yet: an empty store, which
        // refuses nothing.
        std::fs::create_dir_all(root.join("home/.codex")).unwrap();
        std::fs::write(root.join("home/.codex/config.toml"), "").unwrap();
        let project = root.join("project");
        let other = root.join("other-codex");
        for directory in [&project, &other] {
            std::fs::create_dir(directory).unwrap();
        }
        let transcript = other.join("thread.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let database = rusqlite::Connection::open(other.join("state_1.sqlite")).unwrap();
        database.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, model TEXT, reasoning_effort TEXT, approval_mode TEXT, sandbox_policy TEXT)").unwrap();
        database.execute("INSERT INTO threads VALUES ('elsewhere',?1,?2,'question','cli',123,0,'m','high','never','{\"type\":\"read-only\"}')", rusqlite::params![transcript.to_str().unwrap(), project.to_str().unwrap()]).unwrap();
        for (id, path, cwd, archived) in [
            ("archived", transcript.to_str(), project.to_str(), 1),
            ("no-rollout", None, project.to_str(), 0),
            ("another-folder", transcript.to_str(), other.to_str(), 1),
        ] {
            database.execute("INSERT INTO threads VALUES (?1,?2,?3,'question','cli',123,?4,'m','high','never','{\"type\":\"read-only\"}')", rusqlite::params![id, path, cwd, archived]).unwrap();
        }
        drop(database);
        crate::rc::runtime_sources::Registry::open()
            .unwrap()
            .register(&other, None, None, None)
            .unwrap();
        let held = super::super::held_sessions(&project).unwrap();
        for id in ["codex:elsewhere", "codex:archived", "codex:no-rollout"] {
            assert!(held.contains(&id.to_owned()), "{id}: {held:?}");
        }
        assert!(
            !held.contains(&"codex:another-folder".to_owned()),
            "{held:?}"
        );

        let refuses = || {
            let refused = super::super::held_sessions(&project).unwrap_err();
            assert!(
                format!("{refused:#}").contains("its uploads were not turned on"),
                "{refused:#}"
            );
        };
        // The same empty Codex home with a `sessions` that can't be read is no longer empty.
        let sessions = root.join("home/.codex/sessions");
        std::fs::write(&sessions, "").unwrap();
        refuses();
        std::fs::remove_file(&sessions).unwrap();

        // A Claude Code store that isn't there is empty; one behind something that isn't a folder
        // can't be read, and refuses.
        let claude = root.join("home/.claude");
        std::fs::write(&claude, "").unwrap();
        refuses();
        std::fs::remove_file(&claude).unwrap();
        // So does a project folder that can be listed but not followed.
        #[cfg(unix)]
        {
            std::fs::create_dir_all(claude.join("projects")).unwrap();
            let looped = claude.join("projects/looped");
            std::os::unix::fs::symlink(&looped, &looped).unwrap();
            refuses();
            std::fs::remove_file(&looped).unwrap();
        }
        super::super::held_sessions(&project).unwrap();

        std::fs::remove_dir_all(&other).unwrap();
        refuses();
    }

    /// A conversation recorded while the folder was off is not published after `agit on`: the
    /// recovery of its queued turns holds it back as the capture does, until the person saves it.
    /// One started after `agit on` publishes.
    #[test]
    fn recovery_publishes_nothing_of_what_ran_while_the_folder_was_off() {
        if crate::rc::in_isolated_test(
            "commands::front::on::tests::recovery_publishes_nothing_of_what_ran_while_the_folder_was_off",
        ) {
            return;
        }
        use crate::rc::supervisor::archive_recovery::{Waiting, publication_target};
        let home = tempfile::tempdir().unwrap();
        let hub = "http://127.0.0.1:9";
        unsafe {
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
            std::env::set_var("AGIT_HUB_URL", hub);
        }
        crate::rc::select_local_authority();
        let folder = home.path().join("app");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        let repository =
            crate::rc::local_repository::ensure_repository("project", &folder).unwrap();
        let lineage =
            crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
                .unwrap();
        let repo = crate::rc::local_repository::require(&lineage).unwrap();
        crate::rc::local_repository::publication::Selection::prepare(
            &repo,
            Some("owner/project"),
            hub,
        )
        .unwrap()
        .unwrap()
        .bind(
            &repo,
            &crate::hub::identity::RemoteIdentity::new(hub, "00000000-0000-0000-0000-000000000002")
                .unwrap(),
        )
        .unwrap();
        repo.set_auto_push(Some(true)).unwrap();
        let recovery = |native: &str| publication_target(&repo, &lineage, "codex", native);

        super::turn_off_bound_folders(&folder).unwrap();
        let waits = |native: &str| {
            recovery(native)
                .unwrap_err()
                .downcast_ref::<Waiting>()
                .copied()
        };
        assert_eq!(waits("while-off"), Some(Waiting::PushDisabled));

        let lifted =
            super::turn_on_bound_folders(&folder, |_| Ok(vec!["codex:while-off".into()])).unwrap();
        assert_eq!(lifted, vec![(folder.clone(), Some("owner/project".into()))]);
        assert_eq!(
            waits("while-off"),
            Some(Waiting::HeldBack),
            "recovery publishes nothing of what ran while off"
        );
        assert_eq!(recovery("after").unwrap().repository, "owner/project");

        crate::rc::folder_cutoff::admit(&folder, "codex", "while-off").unwrap();
        assert_eq!(
            recovery("while-off").unwrap().repository,
            "owner/project",
            "saved by the person, it publishes"
        );
    }

    use crate::commands::setup::CodexHooks;

    /// A folder is on for Codex only in name while Codex holds AgentGit's hooks for review: nothing
    /// from Codex saves until the person approves them. `agit on` then says that nothing is saved
    /// yet and how to approve them, and says every turn saves only when Codex runs an AgentGit
    /// hook for every event. One that judged by the hooks feature alone would call an unapproved
    /// Codex saving (#250, #138); one that trusted a Codex that lists no AgentGit hook would too,
    /// and so would one that trusted every hook listed while the Stop hook was deleted.
    #[test]
    fn agit_on_says_nothing_saves_while_codex_holds_the_hooks_for_review() {
        let held = codex_state(Some(CodexHooks {
            listed: 2,
            inactive: vec![
                ("SessionStart".into(), "not yet approved"),
                ("Stop".into(), "changed since it was approved"),
            ],
            missing: Vec::new(),
        }));
        assert_eq!(held.name(), "inactive");
        let said = held.describe();
        assert!(said.contains("Nothing from Codex is saved"), "{said}");
        assert!(
            said.contains("SessionStart (not yet approved)")
                && said.contains("Stop (changed since it was approved)")
                && said.contains("approve them in its hooks review"),
            "{said}"
        );

        let running = codex_state(Some(CodexHooks {
            listed: 2,
            inactive: Vec::new(),
            missing: Vec::new(),
        }));
        assert!(matches!(running, RuntimeState::Automatic), "{running:?}");
        // The Stop hook was deleted and the approved SessionStart hook kept.
        let no_stop = codex_state(Some(CodexHooks {
            listed: 1,
            inactive: Vec::new(),
            missing: vec!["Stop"],
        }));
        assert_eq!(no_stop.name(), "inactive");
        let said = no_stop.describe();
        assert!(
            said.contains("no Codex turn here is saved")
                && said.contains("`agit setup --runtime codex --hooks`"),
            "{said}"
        );
        let none = codex_state(Some(CodexHooks::default()));
        assert_eq!(none.name(), "missing");
        assert!(
            none.describe()
                .contains("`agit setup --runtime codex --hooks`")
        );
        // Codex that does not answer is not claimed either way.
        assert_eq!(codex_state(None).name(), "needs_trust");
    }
}

#[cfg(test)]
mod undo_tests {
    use super::undo_replaced_off;
    use crate::domain::repo::{Repo, automatic_upload_off};

    /// A refused `agit on --repo` puts the person's off back even when the checkout's Git
    /// configuration can't be written, which may be why the bind failed. Restoring the setting
    /// first and returning on its error would leave the off out, and the destination would
    /// inherit an enabled preference. With the setting unchanged, nothing is written to Git at
    /// all; with it changed, the failed write is reported after the off is back.
    #[test]
    fn the_off_returns_even_when_the_checkouts_setting_cant_be_written() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::infra::config::env_lock();
        let old_home = std::env::var_os("AGIT_HOME");
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        let checkout = home.path().join("repos/alice/app");
        let repo = Repo::init(&checkout).unwrap();
        let lock = checkout.join(".git/config.lock");
        std::fs::write(&lock, "held").unwrap();
        let unchanged = undo_replaced_off("alice", "app", &checkout, None);
        let unchanged_off = automatic_upload_off::recorded("alice", "app");
        automatic_upload_off::set("alice", "app", false).unwrap();
        std::fs::remove_file(&lock).unwrap();
        repo.set_auto_push(Some(true)).unwrap();
        std::fs::write(&lock, "held").unwrap();
        let changed = undo_replaced_off("alice", "app", &checkout, None);
        let changed_off = automatic_upload_off::recorded("alice", "app");
        // The setting the refused bind left behind could not be taken back, and the restored off
        // still keeps the destination from uploading.
        let changed_uploads = (repo.auto_push_enabled(), repo.auto_push_explicitly_off());
        unsafe {
            match old_home {
                Some(value) => std::env::set_var("AGIT_HOME", value),
                None => std::env::remove_var("AGIT_HOME"),
            }
        }
        assert!(unchanged.is_ok(), "{unchanged:?}");
        assert_eq!(unchanged_off, automatic_upload_off::Recorded::Off);
        assert!(changed.is_err());
        assert_eq!(changed_off, automatic_upload_off::Recorded::Off);
        assert_eq!(repo.auto_push_override().unwrap(), Some(true));
        assert!(
            matches!(changed_uploads, (Ok(false), Ok(true))),
            "{changed_uploads:?}"
        );
    }
}
