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
    super::finish(turn_on(args))
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
    let root = super::folder(&current_or(args.dir)?)?;
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
    report(&root, &enabled)
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
    if let Err(error) = bind(root, &slug) {
        if let Some(previous) = previous {
            previous.restore(root, &slug)?;
        }
        return Err(error);
    }
    let (granted, grant_page) = if created {
        grant_teams(client, &destination)
    } else {
        (Vec::new(), None)
    };
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
    })
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
    let earlier = crate::commands::project::earlier_count(root);
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
                "previous_repository": enabled.previous,
                "note": enabled.note,
                "teams_granted": enabled.granted,
                "grant_access_page": enabled.grant_page,
                "earlier_sessions_not_uploaded": earlier,
                "runtimes": runtimes.iter().map(|(r, state)| serde_json::json!({"runtime": r, "state": state.name(), "says": state.describe()})).collect::<Vec<_>>(),
                "next": (earlier > 0).then(|| serde_json::json!({
                    "command": "agit save --all",
                    "why": "upload the sessions that ran here before",
                })),
            })
        );
        return Ok(ExitCode::Ok);
    }
    let folder = ui::tilde(root);
    if enabled.already {
        ui::success(&format!("{folder} is already on"));
    } else {
        ui::success(&format!("{folder} is on"));
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
    println!(
        "  sessions here upload to {}{visibility}{how}",
        ui::bold(enabled.repository.as_str())
    );
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
    if earlier > 0 {
        println!(
            "  {} earlier session{} here {} not uploaded; to upload {} too: {}",
            earlier,
            if earlier == 1 { "" } else { "s" },
            if earlier == 1 { "is" } else { "are" },
            if earlier == 1 { "it" } else { "them" },
            ui::accent("agit save --all")
        );
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
                "this Codex has no hooks; its sessions here upload when you run `agit save`".into()
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
            codex_state(crate::commands::setup::codex_hooks(root))
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

fn turn_off(args: OffArgs) -> CmdResult {
    let root = super::folder(&current_or(args.dir)?)?;
    let folder = ui::tilde(&root);
    match crate::commands::project::uploads_automatically(&root) {
        Some(true) => {
            let root_arg = root.to_string_lossy().into_owned();
            run_child(&["project", "unbind", &root_arg], &root)?;
            if crate::commands::json::is_capturing() {
                println!("{}", serde_json::json!({"folder": root, "on": false}));
            } else {
                ui::success(&format!("{folder} is off"));
                println!(
                    "  new sessions here are no longer saved or uploaded; nothing was deleted"
                );
                println!("  to turn it back on: {}", ui::accent("agit on"));
            }
            Ok(ExitCode::Ok)
        }
        _ => {
            if crate::commands::json::is_capturing() {
                println!("{}", serde_json::json!({"folder": root, "on": false}));
            } else {
                println!("{folder} is already off");
            }
            Ok(ExitCode::Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeState, codex_state};
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
