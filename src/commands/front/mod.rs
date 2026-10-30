//! The front door: commands that infer what the advanced commands take as arguments.
//!
//! A person names nothing here. The folder is the Git top level of the working directory, or the
//! directory itself; its destination comes from an existing binding, `--repo`, the Hub
//! repositories linked to the folder's Git origin, or the account's own repository named after
//! the folder; sessions are found by the folder they ran in. Every choice made for the person is
//! printed, and every refusal ends with one next step.
//!
//! The front door composes the advanced commands rather than reimplementing them, so a folder
//! turned on here is an ordinary project binding and its sessions are ordinary session branches.

pub mod on;
pub mod overview;

use crate::{
    ExitCode, adapter,
    domain::{link, repo::Repo, store::Store},
    infra::{config, credentials},
    ui,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A refusal that names the next step. The message and the step are plain words: the front door
/// never asks a person to know how sessions are stored.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub message: String,
    pub next: String,
    pub code: ExitCode,
}

impl Refusal {
    pub(crate) fn new(message: impl Into<String>, next: impl Into<String>, code: ExitCode) -> Self {
        Self {
            message: message.into(),
            next: next.into(),
            code,
        }
    }

    pub(crate) fn report(&self) -> ExitCode {
        // A cancel or a "no" at a question is an outcome, not a failure: it reads as a plain line.
        if self.code == ExitCode::Ok {
            ui::progress(&self.message);
        } else {
            ui::error(&self.message);
        }
        ui::hint(&self.next);
        self.code
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; {}", self.message, self.next)
    }
}

impl std::error::Error for Refusal {}

/// The category of a child command's exit status.
pub(crate) fn exit_code(code: i32) -> ExitCode {
    [
        ExitCode::Ok,
        ExitCode::Failure,
        ExitCode::Usage,
        ExitCode::Ref,
        ExitCode::Precondition,
        ExitCode::Auth,
        ExitCode::Network,
        ExitCode::Policy,
        ExitCode::Interactive,
    ]
    .into_iter()
    .find(|c| c.as_i32() == code)
    .unwrap_or(ExitCode::Failure)
}

/// Prints a refusal raised anywhere below a front-door command; other errors propagate.
pub(crate) fn finish(result: super::CmdResult) -> super::CmdResult {
    match result {
        Err(error) if error.is::<Refusal>() => Ok(error
            .downcast_ref::<Refusal>()
            .expect("checked above")
            .report()),
        other => other,
    }
}

/// The folder a directory belongs to: its Git top level, or the directory itself.
///
/// A home directory or a filesystem root would gather every unrelated conversation on the machine
/// into one destination, so neither is a folder.
pub(crate) fn folder(dir: &Path) -> Result<PathBuf> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} does not exist", dir.display()))?;
    let root = git_top_level(&dir).unwrap_or(dir);
    let home = config::user_home().and_then(|h| h.canonicalize().ok());
    if root.parent().is_none() || home.as_ref() == Some(&root) {
        return Err(Refusal::new(
            format!(
                "{} holds everything on this computer, not one project",
                ui::tilde(&root)
            ),
            "run this inside a project folder, for example `cd ~/code/my-app` first",
            ExitCode::Usage,
        )
        .into());
    }
    Ok(root)
}

fn git_top_level(dir: &Path) -> Option<PathBuf> {
    let out = crate::infra::git_runtime::command()
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !top.is_empty())
        .then(|| PathBuf::from(top).canonicalize().ok())
        .flatten()
}

/// The folder's Git `origin`, without user information: it is sent to the Hub, and a URL a person
/// configured with a token in it must not carry the token there.
pub(crate) fn origin(root: &Path) -> Option<String> {
    let out = crate::infra::git_runtime::command()
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !url.is_empty()).then(|| without_user_info(&url))
}

fn without_user_info(url: &str) -> String {
    let Some(i) = url.find("://") else {
        return url.to_string();
    };
    let (scheme, rest) = url.split_at(i + 3);
    match rest.find('@') {
        Some(at) if !rest[..at].contains('/') => format!("{scheme}{}", &rest[at + 1..]),
        _ => url.to_string(),
    }
}

/// The repository name a folder suggests: the origin's last path segment, else the folder's own
/// name, reduced to the characters repository names allow. `None` when nothing usable remains.
pub(crate) fn suggested_name(root: &Path, origin: Option<&str>) -> Option<String> {
    let from_origin = origin.and_then(|url| {
        url.trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .map(|last| last.trim_end_matches(".git").to_string())
    });
    let from_folder = root.file_name().map(|n| n.to_string_lossy().to_string());
    [from_origin, from_folder]
        .into_iter()
        .flatten()
        .map(|raw| slug(&raw, 64))
        .find(|name| !name.is_empty() && crate::domain::repo::valid_name(name).is_ok())
}

/// Lowercase ASCII words joined by `-`, at most `max` characters, cut at a word boundary.
pub(crate) fn slug(text: &str, max: usize) -> String {
    let mut out = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let extra = if out.is_empty() {
            word.len()
        } else {
            word.len() + 1
        };
        if out.len() + extra > max {
            if out.is_empty() {
                out.push_str(&word[..max]);
            }
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word);
    }
    out
}

/// How a destination was chosen; the result names it so the person can tell where sessions go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Chosen {
    /// The folder is already bound to it.
    Bound,
    /// Named with `--repo`.
    Given,
    /// The Hub links it to the folder's Git origin, and the account may write to it.
    Linked,
    /// The team's repository for this project: in the Hub organization named like the owner of
    /// the folder's Git origin, named after the project.
    Team,
    /// A new repository for the project in the organization the folder's Git origin belongs to,
    /// picked at a terminal by an owner of that organization.
    Org,
    /// The account's own repository named after the folder.
    Own,
}

#[derive(Debug, Clone)]
pub(crate) struct Destination {
    pub owner: String,
    pub name: String,
    pub chosen: Chosen,
    /// The repository the folder saves to now, when `--repo` names another one.
    pub replaces: Option<String>,
    /// Said to the person with the result: why the team's repository was not used.
    pub note: Option<String>,
    /// The person already picked this destination at a prompt.
    pub asked: bool,
}

impl Destination {
    pub(crate) fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// The destination already bound to `root`, without network access.
pub(crate) fn bound(root: &Path) -> Option<String> {
    super::project::policy_repository(root)
        .or_else(|| crate::domain::workspace::read(root).map(|binding| binding.repo))
}

/// Chooses where a folder's sessions go. See the module documentation for the order.
pub(crate) fn destination(
    client: &crate::hub::Client,
    root: &Path,
    origin: Option<&str>,
    given: Option<&str>,
) -> Result<Destination> {
    let me = client
        .credential_username()
        .context("the signed-in account is unavailable")?;
    let parse = |slug: &str, chosen| -> Result<Destination> {
        let (owner, name) = super::parse_slug(slug)?;
        Ok(Destination {
            owner,
            name,
            chosen,
            replaces: None,
            note: None,
            asked: false,
        })
    };
    if let Some(given) = given {
        let mut given = parse(given, Chosen::Given)?;
        given.replaces = bound(root).filter(|existing| *existing != given.slug());
        return Ok(given);
    }
    if let Some(existing) = bound(root) {
        return parse(&existing, Chosen::Bound);
    }
    // Repositories the Hub links to the folder's code, split by whether the account can upload.
    // Every future turn goes to the chosen destination, so a public one is never chosen for the
    // person; naming it with `--repo` and `--public` is the only way to upload there.
    let mut linked = TeamRepositories::default();
    if let Some(origin) = origin {
        for agent in super::remote_request(client.agents_for_repo(origin))?
            .into_iter()
            .filter(|agent| !agent.is_public())
        {
            let writable = agent.owner == me
                || matches!(
                    super::remote_request(push_access(
                        client,
                        &agent.owner,
                        &agent.name,
                        &agent.agent_id
                    ))?,
                    crate::hub::PushAccess::Writable
                );
            if writable {
                linked.writable.push(agent.slug());
            } else {
                linked.read_only.push(agent.slug());
            }
        }
    }
    // A project whose code belongs to an organization the account is in keeps its sessions with
    // the team, ahead of personal repositories linked to the same code: an existing repository
    // is joined, one the account cannot upload to is named with how to get access, a missing one
    // is offered to an owner, and a personal repository says why it was used and how to move.
    if let Some(org) = origin.and_then(origin_owner).filter(|owner| *owner != me)
        && let Some(role) = org_role(client, &org)?
        && let Some(name) = suggested_name(root, origin)
    {
        let prefix = format!("{org}/");
        let mut team = team_repositories(client, &org, &name)?;
        let (linked_team, others): (Vec<String>, Vec<String>) = linked
            .writable
            .into_iter()
            .partition(|slug| slug.starts_with(&prefix));
        team.writable.extend(linked_team);
        team.read_only.extend(
            linked
                .read_only
                .into_iter()
                .filter(|slug| slug.starts_with(&prefix)),
        );
        for list in [&mut team.writable, &mut team.read_only] {
            list.sort();
            list.dedup();
        }
        if let Some(slug) = one_of(team.writable)? {
            return parse(&slug, Chosen::Team);
        }
        // A personal repository already linked to this code is the personal choice; a second
        // one is never made beside it.
        let own = match others.as_slice() {
            [mine] => parse(mine, Chosen::Linked)?,
            _ => own(client, &me, root, origin)?,
        };
        if let Some(blocked) = team.read_only.first() {
            return blocked_team(client, &org, blocked, own);
        }
        let shared = format!("{org}/{name}-agents");
        if role == "owner" {
            return org_or_own(&org, &shared, own);
        }
        return Ok(Destination {
            note: Some(format!(
                "no repository of {org} for this project is open to you; an owner of {org} can create {shared} or give your team access to one, and then `agit on --repo {shared}` moves this folder there"
            )),
            ..own
        });
    }
    // The repository of the code's owner wins over personal ones linked to the same code, also
    // for an account that collaborates on it without being a member of the organization.
    let owner = origin
        .and_then(origin_owner)
        .map(|owner| format!("{owner}/"));
    let (team, others): (Vec<String>, Vec<String>) =
        linked.writable.into_iter().partition(|slug| {
            owner
                .as_deref()
                .is_some_and(|owner| slug.starts_with(owner))
        });
    if !team.is_empty() {
        if let Some(slug) = one_of(team)? {
            return parse(&slug, Chosen::Team);
        }
    } else if let Some(slug) = one_of(others)? {
        return parse(&slug, Chosen::Linked);
    }
    own(client, &me, root, origin)
}

/// The account's own private repository named after the folder.
fn own(
    client: &crate::hub::Client,
    me: &str,
    root: &Path,
    origin: Option<&str>,
) -> Result<Destination> {
    let Some(name) = suggested_name(root, origin) else {
        return Err(Refusal::new(
            format!("cannot make a repository name from {}", ui::tilde(root)),
            "name one, for example `agit on --repo YOUR-NAME/my-project`",
            ExitCode::Usage,
        )
        .into());
    };
    // A public repository of the same name is never the default; the next free private-looking
    // name is used instead.
    let mut candidate = name.clone();
    for n in 1.. {
        match client.get_agent(me, &candidate) {
            Ok(remote) if remote.is_public() => {
                candidate = if n == 1 {
                    format!("{name}-sessions")
                } else {
                    format!("{name}-sessions-{n}")
                };
            }
            Ok(_) => break,
            Err(error) if is_not_found(&error) => break,
            Err(error) => return super::remote_request(Err(error)),
        }
    }
    Ok(Destination {
        owner: me.to_owned(),
        name: candidate,
        chosen: Chosen::Own,
        replaces: None,
        note: None,
        asked: false,
    })
}

/// The Hub page where an owner gives teams and people access to a repository.
pub(crate) fn settings_url(client: &crate::hub::Client, slug: &str) -> String {
    format!("{}/@{slug}/settings", client.base().trim_end_matches('/'))
}

/// The team's repository exists but the account cannot upload to it. A personal repository is
/// used only when the person picks it at a terminal; elsewhere the refusal names the owner's
/// one-click fix and the personal alternative.
fn blocked_team(
    client: &crate::hub::Client,
    org: &str,
    blocked: &str,
    own: Destination,
) -> Result<Destination> {
    let why = format!("{blocked} holds this project's sessions, but you cannot upload there yet");
    let ask = format!(
        "ask an owner of {org} to give your team access: {}",
        settings_url(client, blocked)
    );
    if ui::prompt::may_prompt() {
        println!("{why}.\n{ask}.");
        let mine = format!("your own private repository {}, for now", own.slug());
        return match ui::prompt::select(
            "Save this folder's sessions to",
            &[mine.as_str(), "nowhere yet"],
        )? {
            Some(0) => Ok(Destination {
                note: Some(format!(
                    "{why}; once you can, `agit on --repo {blocked}` moves this folder there"
                )),
                asked: true,
                ..own
            }),
            _ => Err(Refusal::new("nothing changed", ask, ExitCode::Ok).into()),
        };
    }
    Err(Refusal::new(
        why,
        format!(
            "{ask}; or save to your own for now: `agit on --repo {}`",
            own.slug()
        ),
        ExitCode::Precondition,
    )
    .into())
}

/// An owner of the organization picks between a new repository there and their own one. Without
/// a terminal nothing is picked for them: the refusal names both.
fn org_or_own(org: &str, shared: &str, own: Destination) -> Result<Destination> {
    let team =
        format!("{shared} (new; private to {org}: its owners and granted teams can read it)");
    let mine = format!(
        "{} (yours; private: only you and people you add can read it)",
        own.slug()
    );
    if ui::prompt::may_prompt() {
        let question =
            format!("{org} has no repository for this project yet. Save its sessions to");
        return match ui::prompt::select(&question, &[team.as_str(), mine.as_str()])? {
            Some(0) => {
                let (owner, name) = super::parse_slug(shared)?;
                Ok(Destination {
                    owner,
                    name,
                    chosen: Chosen::Org,
                    replaces: None,
                    note: None,
                    asked: true,
                })
            }
            Some(_) => Ok(Destination { asked: true, ..own }),
            None => Err(Refusal::new(
                "nothing changed",
                format!(
                    "choose with `agit on --repo {shared}` or `agit on --repo {}`",
                    own.slug()
                ),
                ExitCode::Ok,
            )
            .into()),
        };
    }
    Err(Refusal::new(
        format!("choose where this project's sessions go: {team}, or {mine}"),
        format!(
            "`agit on --repo {shared}` or `agit on --repo {}`",
            own.slug()
        ),
        ExitCode::Interactive,
    )
    .into())
}

/// The account's role in organization `name`, or `None` when the Hub shows it no such
/// organization.
fn org_role(client: &crate::hub::Client, name: &str) -> Result<Option<String>> {
    match client.get_org(name) {
        Ok(org) => Ok(Some(org.role)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => super::remote_request(Err(error)),
    }
}

/// The one candidate, a choice at a terminal among several, or a refusal that lists them.
fn one_of(candidates: Vec<String>) -> Result<Option<String>> {
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.into_iter().next()),
        _ => {
            let labels: Vec<&str> = candidates.iter().map(String::as_str).collect();
            if ui::prompt::may_prompt()
                && let Some(i) = ui::prompt::select(
                    "Several places on the Hub already hold this project's sessions. Save to",
                    &labels,
                )?
            {
                return Ok(Some(candidates[i].clone()));
            }
            Err(Refusal::new(
                format!(
                    "several places on the Hub already hold this project's sessions: {}",
                    candidates.join(", ")
                ),
                format!("choose one, for example `agit on --repo {}`", candidates[0]),
                ExitCode::Interactive,
            )
            .into())
        }
    }
}

/// The organization's private repositories for the project, whose names are the project's name
/// or start with it (`acme/widget`, `acme/widget-agents`), split by whether the account can upload
/// to them. A team that made its repository on the website, without linking the code origin, is
/// found this way; joining it links the origin for the next teammate.
#[derive(Default)]
struct TeamRepositories {
    writable: Vec<String>,
    read_only: Vec<String>,
}

fn team_repositories(
    client: &crate::hub::Client,
    org: &str,
    name: &str,
) -> Result<TeamRepositories> {
    let prefix = format!("{name}-");
    let mut team = TeamRepositories {
        writable: Vec::new(),
        read_only: Vec::new(),
    };
    for agent in super::remote_request(client.agents_owned_by(org))? {
        if agent.is_public() || !(agent.name == name || agent.name.starts_with(&prefix)) {
            continue;
        }
        match super::remote_request(push_access(
            client,
            &agent.owner,
            &agent.name,
            &agent.agent_id,
        ))? {
            crate::hub::PushAccess::Writable => team.writable.push(agent.slug()),
            _ => team.read_only.push(agent.slug()),
        }
    }
    Ok(team)
}

/// The owner segment of a Git origin (`git@host:acme/widget.git`, `https://host/acme/widget`),
/// lowercased the way Hub namespaces are.
fn origin_owner(origin: &str) -> Option<String> {
    let path = match origin.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => origin.split_once(':')?.1,
    };
    let mut segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    segments.pop()?;
    let owner = segments.pop()?.to_ascii_lowercase();
    (!owner.is_empty()).then_some(owner)
}

/// Makes sure the destination exists on the Hub and is linked to the folder's origin. Returns
/// its visibility and whether this call created it. Creation is private: nothing the front door
/// creates is readable by anyone else until the person shares it.
pub(crate) fn ensure_on_hub(
    client: &crate::hub::Client,
    destination: &Destination,
    origin: Option<&str>,
) -> Result<(String, bool)> {
    let me = client.credential_username();
    let existing = match client.get_agent(&destination.owner, &destination.name) {
        Ok(remote) => Some(remote),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return super::remote_request(Err(error)),
    };
    if let Some(remote) = &existing {
        let writable = me.as_deref() == Some(remote.owner.as_str())
            || matches!(
                super::remote_request(push_access(
                    client,
                    &remote.owner,
                    &remote.name,
                    &remote.agent_id
                ))?,
                crate::hub::PushAccess::Writable
            );
        if !writable {
            return Err(Refusal::new(
                format!("you can read {} but not save to it", destination.slug()),
                format!(
                    "ask its owner to give you write access, or save to your own: `agit on --repo {}/{}`",
                    me.as_deref().unwrap_or("YOUR-NAME"),
                    destination.name
                ),
                ExitCode::Precondition,
            )
            .into());
        }
        if origin.is_none() {
            return Ok((remote.visibility.clone(), false));
        }
    }
    // Creating an existing repository answers with it and links the origins it is sent, so this
    // one request both creates and links.
    let request = crate::hub::PublishRequest {
        name: destination.name.clone(),
        owner: (me.as_deref() != Some(destination.owner.as_str()))
            .then(|| destination.owner.clone()),
        public: false,
        repo_origins: origin.into_iter().map(str::to_string).collect(),
    };
    let response = match (&existing, client.publish(&request)) {
        (_, Ok(response)) => response,
        // Linking an existing repository to more code is for its owners; a teammate who may
        // upload there joins it as it is.
        (Some(remote), Err(_)) => return Ok((remote.visibility.clone(), false)),
        (None, Err(error)) => return super::remote_request(Err(error)),
    };
    anyhow::ensure!(
        response.owner == destination.owner && response.name == destination.name,
        "the Hub answered with {}/{} instead of {}",
        response.owner,
        response.name,
        destination.slug()
    );
    Ok(match existing {
        Some(remote) => (remote.visibility, false),
        None => (
            super::remote_request(client.get_agent(&destination.owner, &destination.name))?
                .visibility,
            true,
        ),
    })
}

/// The destination's visibility on the Hub, or `None` when it does not exist yet.
pub(crate) fn visibility(
    client: &crate::hub::Client,
    destination: &Destination,
) -> Result<Option<String>> {
    match client.get_agent(&destination.owner, &destination.name) {
        Ok(remote) => Ok(Some(remote.visibility)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => super::remote_request(Err(error)),
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::hub::client::ApiError>()
        .is_some_and(|api| api.status == 404)
}

/// Signs in when needed. At a terminal it runs the ordinary sign-in and waits for it; elsewhere
/// it starts a sign-in, prints the link for the person, and stops with the instruction to run the
/// same command again, which then finishes the approved sign-in by itself.
pub(crate) fn sign_in(again: &str) -> Result<crate::hub::Client> {
    match super::require_login() {
        Ok(client) => return Ok(client),
        Err(error) if !error.is::<super::LoginRequired>() => return Err(error),
        Err(_) => {}
    }
    let hub = config::hub_url();
    if ui::prompt::may_prompt() {
        let code = super::login::run(super::login::Args::default())?;
        if code != ExitCode::Ok {
            return Err(Refusal::new(
                "not signed in",
                format!("sign in with `agit login`, then run `{again}` again"),
                code,
            )
            .into());
        }
        return super::require_login();
    }
    if super::login::is_waiting(&hub) {
        return Err(Refusal::new(
            "the sign-in link has not been approved yet",
            format!(
                "open the link from before and approve it, then run `{again}` again; for a new link run `agit login`"
            ),
            ExitCode::Interactive,
        )
        .into());
    }
    let link = super::login::request_sign_in(&hub)?;
    if super::json::is_capturing() {
        println!(
            "{}",
            serde_json::json!({
                "status": "sign_in_required",
                "sign_in_url": link.url,
                "expires_in": link.expires_in,
                "next": {"human_action": "open the link, sign in and approve", "url": link.url,
                         "then": again},
            })
        );
    } else {
        println!("Sign in to continue: {}", link.url);
    }
    Err(Refusal::new(
        "sign-in needed",
        format!("open the link above and approve, then run `{again}` again"),
        ExitCode::Interactive,
    )
    .into())
}

/// One conversation that ran in a folder.
pub(crate) struct FolderSession {
    pub runtime: &'static str,
    pub id: String,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    /// The runtime's own name for it, or its opening request.
    pub title: Option<String>,
    /// Where it is saved, as `owner/repo` and branch.
    pub saved: Option<(String, String)>,
}

/// Where a saved session stands against the Hub, from local state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Upload {
    /// Saved here, never uploaded.
    NotUploaded,
    /// Uploaded, and nothing newer is waiting.
    Uploaded,
    /// Uploaded before; newer turns are waiting.
    Waiting,
}

impl FolderSession {
    pub(crate) fn upload(&self) -> Option<Upload> {
        let (repository, branch) = self.saved.as_ref()?;
        let (owner, name) = super::parse_slug(repository).ok()?;
        let repo = Repo::open(config::repo_dir(&owner, &name).ok()?)?;
        let local = repo.git_opt(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ]);
        let remote = repo.git_opt(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ]);
        Some(match (local, remote) {
            (_, None) => Upload::NotUploaded,
            (local, remote) if local == remote => Upload::Uploaded,
            _ => Upload::Waiting,
        })
    }

    /// The title, reading the transcript when the runtime's index has none.
    pub(crate) fn title(&mut self) -> String {
        if self.title.is_none() {
            self.title = adapter::get(self.runtime)
                .ok()
                .and_then(|a| a.parse_at(&self.path).ok())
                .and_then(|s| s.gist(60));
        }
        self.title.clone().unwrap_or_else(|| {
            format!("{} session {}", runtime_name(self.runtime), short(&self.id))
        })
    }
}

pub(crate) fn runtime_name(runtime: &str) -> &str {
    match runtime {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Claude Code and Codex conversations that ran in `root` or below it, newest first.
pub(crate) fn sessions_in(root: &Path) -> Result<Vec<FolderSession>> {
    let store = Store::open().ok().flatten();
    let mut sessions = Vec::new();
    for runtime in ["claude-code", "codex"] {
        let adapter = adapter::get(runtime)?;
        if !adapter.available() {
            continue;
        }
        for session in adapter.all_session_choices()? {
            let cwd = session
                .cwd
                .clone()
                .or_else(|| crate::tui::screens::selector::recorded_cwd(&session.path));
            let Some(cwd) = cwd.and_then(|p| PathBuf::from(p).canonicalize().ok()) else {
                continue;
            };
            if !cwd.starts_with(root) {
                continue;
            }
            let saved = store
                .as_ref()
                .and_then(|store| link::get(store, runtime, &session.id))
                .and_then(|link| {
                    Some((
                        format!("{}/{}", link.owner.as_deref()?, link.agent.as_deref()?),
                        link.branch?,
                    ))
                });
            sessions.push(FolderSession {
                runtime,
                id: session.id,
                path: session.path,
                modified: session.mtime,
                title: session.title.or(session.gist),
                saved,
            });
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(sessions)
}

/// The account signed in to the selected Hub, from local state only.
pub(crate) fn signed_in_account() -> Option<String> {
    credentials::load_checked(&config::hub_url())
        .ok()
        .flatten()
        .filter(|c| !c.refresh_expired())
        .map(|c| c.username)
}

/// Whether the account can push to a repository, asked a second time when the first ask fails: a
/// Hub under load can take longer than one request allows to answer the first step of a push, and
/// asking again is safe.
fn push_access(
    client: &crate::hub::Client,
    owner: &str,
    name: &str,
    agent_id: &str,
) -> Result<crate::hub::PushAccess> {
    client
        .push_access(owner, name, agent_id)
        .or_else(|_| client.push_access(owner, name, agent_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The origin is sent to the Hub to find the team's destination. A token a person put into an
    /// HTTPS remote must not travel with it; an SSH user is part of the address and stays.
    #[test]
    fn an_origin_reaches_the_hub_without_credentials() {
        assert_eq!(
            without_user_info("https://alice:ghp_secret@github.com/acme/app.git"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            without_user_info("git@github.com:acme/app.git"),
            "git@github.com:acme/app.git"
        );
    }

    /// Names a person sees are read from what they already named: the origin's repository, then
    /// the folder; requests become a few lowercase words cut at a word boundary. Text without
    /// Latin letters yields nothing, so the caller falls back instead of inventing a name.
    #[test]
    fn names_come_from_the_origin_then_the_folder() {
        let folder = Path::new("/work/My Project");
        assert_eq!(
            suggested_name(folder, Some("git@github.com:acme/todo-app.git")).as_deref(),
            Some("todo-app")
        );
        assert_eq!(
            suggested_name(folder, Some("https://forge.example/team/api/")).as_deref(),
            Some("api")
        );
        assert_eq!(suggested_name(folder, None).as_deref(), Some("my-project"));
        assert_eq!(suggested_name(Path::new("/work/项目"), None), None);
        assert_eq!(
            slug("Add a .gitignore that ignores node_modules, please", 40),
            "add-a-gitignore-that-ignores-node"
        );
        assert_eq!(slug("修复登录跳转", 40), "");
    }
}
