//! Explicit project enrollment composes discovery, import and registered session publication.

use super::CmdResult;
use crate::{
    ExitCode, adapter,
    domain::{link, repo::Repo, store::Store, workspace},
    hub::identity::{self, RemoteIdentity},
    infra::{config, credentials},
    ui,
};
use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
pub enum Action {
    /// Enroll a directory and its subdirectories into an existing repository.
    Bind {
        path: PathBuf,
        #[arg(long, value_name = "OWNER/REPO")]
        repo: String,
        /// Import existing conversations, or enroll only conversations created afterward.
        #[arg(long, value_enum)]
        history: History,
        /// Install hooks and enable this repository's existing automatic publication policy.
        #[arg(long)]
        auto_upload: bool,
        /// List candidates without changing bindings, preferences or remote state.
        #[arg(long)]
        dry_run: bool,
        /// Move a directory routed to another repository, replacing its binding and project policy.
        #[arg(long)]
        rebind: bool,
    },
    /// Import and publish eligible sessions again; existing claims are reused.
    Sync {
        path: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect the local project policy and the last sync result, without network access.
    Status { path: PathBuf },
    /// Stop project hook capture; retain local and remote session history.
    Unbind { path: PathBuf },
    /// Stop uploading a conversation from this computer; its local history stays.
    Drop {
        /// The conversation's branch; without it, the conversation this command runs in.
        #[arg(value_name = "OWNER/REPO@BRANCH")]
        target: Option<String>,
    },
    /// Publish the project's hook-reported sessions; the hook that starts it does not wait.
    #[command(hide = true)]
    Capture { path: PathBuf },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum History {
    All,
    None,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    root: PathBuf,
    repository: String,
    identity: RemoteIdentity,
    account: String,
    account_id: Option<String>,
    enabled: bool,
    auto_upload: bool,
    history: History,
    excluded: BTreeSet<String>,
    last_result: Option<serde_json::Value>,
    /// The repository's own push.auto override as it stood before bind turned automatic push
    /// on; absent when bind did not turn it on. Unbind puts it back, so a binding never leaves
    /// every managed session of the repository uploading after the folder is unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    push_auto_before: Option<PushAutoBefore>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PushAutoBefore {
    Unset,
    Off,
    On,
}

impl From<Option<bool>> for PushAutoBefore {
    fn from(value: Option<bool>) -> Self {
        match value {
            None => Self::Unset,
            Some(false) => Self::Off,
            Some(true) => Self::On,
        }
    }
}

impl From<PushAutoBefore> for Option<bool> {
    fn from(value: PushAutoBefore) -> Self {
        match value {
            PushAutoBefore::Unset => None,
            PushAutoBefore::Off => Some(false),
            PushAutoBefore::On => Some(true),
        }
    }
}

fn auto_push_scope(repository: &str) -> String {
    format!(
        "turns on automatic push for the whole repository {repository}: every managed session there uploads after each turn"
    )
}

/// The folders that still hold automatic push on for the same repository as `project`. Their
/// records all name the override from before the first of them was bound.
fn auto_push_holders(project: &Project) -> Result<Vec<Project>> {
    Ok(policies()?
        .into_iter()
        .filter(|other| {
            other.root != project.root
                && other.enabled
                && other.push_auto_before.is_some()
                && other.repository == project.repository
                && other.identity == project.identity
        })
        .collect())
}

/// Turns on automatic push for the whole repository, recording the override from before any
/// folder turned it on: this folder's own earlier record, another bound folder's, or the
/// current one. A folder that recorded the override another folder set would turn automatic
/// push back on when it is unbound last.
fn enable_auto_push(project: &mut Project, repo: &Repo) -> Result<()> {
    if project.push_auto_before.is_none() {
        project.push_auto_before = Some(
            match auto_push_holders(project)?
                .iter()
                .find_map(|other| other.push_auto_before)
            {
                Some(before) => before,
                None => repo.auto_push_override()?.into(),
            },
        );
    }
    repo.set_auto_push(Some(true))
}

/// Restores the push.auto override bind replaced once no other bound folder holds automatic
/// push on, and reports in words whether it stays on. The record is dropped only after this
/// succeeds, so a failed unbind can be repeated.
fn restore_auto_push(project: &mut Project) -> Result<()> {
    restore_auto_push_for(project)?;
    if project.push_auto_before.take().is_some() {
        save(project)?;
    }
    Ok(())
}

fn restore_auto_push_for(project: &Project) -> Result<()> {
    let repository = &project.repository;
    // A repository that is not cloned here pushes nothing.
    let Ok(repo) = checkout(project) else {
        return Ok(());
    };
    // Another clone at the same path keeps its own preference; this folder's record is not
    // about it.
    if identity::read(&repo)?.as_ref() != Some(&project.identity) {
        println!(
            "automatic push for {repository} is unchanged: the local clone is not the repository this folder was bound to"
        );
        return Ok(());
    }
    let Some(before) = project.push_auto_before else {
        if repo.auto_push_enabled()? {
            println!(
                "automatic push stays on for {repository}: it was not turned on by this folder's binding"
            );
        }
        return Ok(());
    };
    if let Some(other) = auto_push_holders(project)?.first() {
        println!(
            "automatic push stays on for {repository}: {} still uploads into it",
            other.root.display()
        );
        return Ok(());
    }
    repo.set_auto_push(before.into())?;
    if repo.auto_push_enabled()? {
        println!(
            "automatic push stays on for {repository}: it was on before this folder was bound"
        );
    } else {
        println!("automatic push is off for {repository}, as it was before this folder was bound");
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Candidate {
    runtime: String,
    session_id: String,
    cwd: PathBuf,
}

fn key(runtime: &str, id: &str) -> String {
    format!("{runtime}:{id}")
}
fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..24].into()
}
fn home() -> Result<PathBuf> {
    Ok(config::agit_home()?.join("projects"))
}
fn state_path(root: &Path) -> Result<PathBuf> {
    Ok(home()?.join(format!("{}.json", hash(&root.to_string_lossy()))))
}
/// Who can see the repository, as the Hub last reported it, kept beside the policy so a policy
/// file stays readable by every version.
fn audience_path(root: &Path) -> Result<PathBuf> {
    Ok(state_path(root)?.with_extension("audience"))
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Audience {
    visibility: Option<String>,
    /// Whether the owner is an organization; `None` until the Hub has said.
    org: Option<bool>,
}

fn recorded_audience(root: &Path) -> Audience {
    audience_path(root)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record_audience(root: &Path, audience: &Audience) -> Result<()> {
    config::create_state_dir(&home()?)?;
    let mut file = tempfile::NamedTempFile::new_in(home()?)?;
    file.write_all(&serde_json::to_vec(audience)?)?;
    file.persist(audience_path(root)?).map_err(|e| e.error)?;
    Ok(())
}

/// Whether `owner` is an organization, from the account's point of view: its own namespace is
/// not, an organization the Hub shows it is, and anything else is unknown. The Hub answers "not
/// found" for an organization the account is not a member of, so that answer settles nothing.
fn owner_is_org(client: &crate::hub::Client, owner: &str, account: &str) -> Option<bool> {
    if owner == account {
        return Some(false);
    }
    client.get_org(owner).ok().map(|_| true)
}

fn root(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .context("project directory must exist")?;
    ensure!(
        path.is_dir() && path.parent().is_some(),
        "select a project directory, not a filesystem root"
    );
    if let Some(user_home) = config::user_home() {
        ensure!(
            user_home.canonicalize().ok().as_ref() != Some(&path),
            "select a project directory, not your home directory"
        );
    }
    Ok(path)
}

fn read(root: &Path) -> Result<Option<Project>> {
    match fs::read(state_path(root)?) {
        Ok(bytes) => {
            let project: Project =
                serde_json::from_slice(&bytes).context("invalid project policy")?;
            ensure!(project.root == root, "project policy directory mismatch");
            Ok(Some(project))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save(project: &Project) -> Result<()> {
    config::create_state_dir(&home()?)?;
    let mut file = tempfile::NamedTempFile::new_in(home()?)?;
    file.write_all(&serde_json::to_vec_pretty(project)?)?;
    file.as_file().sync_all()?;
    file.persist(state_path(&project.root)?)
        .map_err(|e| e.error)?;
    Ok(())
}

fn lock(root: &Path) -> Result<fs::File> {
    lock_at(
        state_path(root)?.with_extension("lock"),
        "the project operation lock",
    )
}

/// Serializes the folders of one repository while they read each other's push.auto records,
/// change the repository's push.auto and save their own policy. Without it, a folder bound
/// while another is between turning push.auto on and saving its record would record that on
/// as the value from before, and the last unbind would leave automatic push on.
fn repository_lock(repository: &str) -> Result<fs::File> {
    lock_at(
        home()?.join(format!("{}.repository-lock", hash(repository))),
        "the repository's automatic push lock",
    )
}

fn lock_at(path: PathBuf, what: &str) -> Result<fs::File> {
    config::create_state_dir(&home()?)?;
    let file = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    crate::infra::local_state::lock_patiently(&file, &path, what, true)?;
    Ok(file)
}

fn nearest(path: &Path) -> Result<Option<Project>> {
    for ancestor in path.ancestors() {
        if let Some(project) = read(ancestor)? {
            return Ok(Some(project));
        }
    }
    Ok(None)
}

fn includes(project: &Project, path: &Path) -> Result<bool> {
    if !path.starts_with(&project.root) {
        return Ok(false);
    }
    for ancestor in path.ancestors() {
        if ancestor == project.root {
            return Ok(true);
        }
        if read(ancestor)?.is_some() || workspace::read(ancestor).is_some() {
            return Ok(false);
        }
    }
    Ok(false)
}

fn candidates(project: &Project) -> Result<Vec<Candidate>> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for runtime in ["codex", "claude-code"] {
        let adapter = adapter::get(runtime)?;
        for session in adapter.all_session_choices()? {
            let cwd = session
                .cwd
                .or_else(|| crate::tui::screens::selector::recorded_cwd(&session.path));
            let Some(cwd) = cwd.and_then(|p| PathBuf::from(p).canonicalize().ok()) else {
                continue;
            };
            if includes(project, &cwd)? && seen.insert(key(runtime, &session.id)) {
                result.push(Candidate {
                    runtime: runtime.into(),
                    session_id: session.id,
                    cwd,
                });
            }
        }
    }
    Ok(result)
}

fn checkout(project: &Project) -> Result<Repo> {
    let (owner, name) = super::parse_slug(&project.repository)?;
    Repo::open(config::repo_dir(&owner, &name)?).context("project repository is not cloned locally")
}

fn verify(project: &Project) -> Result<Repo> {
    ensure!(
        identity::normalize_hub(&config::hub_url())? == project.identity.hub,
        "project belongs to another Hub"
    );
    let credential = credentials::load_checked(&project.identity.hub)?
        .context("sign in before syncing this project")?;
    ensure!(
        credential.username == project.account && credential.account_id == project.account_id,
        "project belongs to another signed-in account; bind again explicitly"
    );
    let repo = checkout(project)?;
    ensure!(
        identity::require_current(&repo, &project.identity.hub)? == project.identity,
        "project repository identity changed"
    );
    ensure!(
        !crate::rc::local_repository::publication::is_device_local(&repo)?,
        "RC repositories remain owned by their supervisor"
    );
    Ok(repo)
}

fn child(arguments: &[String], cwd: &Path, automatic: bool) -> Result<serde_json::Value> {
    let mut command = crate::infra::background::command(std::env::current_exe()?);
    command
        .args(["--quiet", "--json"])
        .args(arguments)
        .current_dir(cwd)
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_YES")
        .stdin(Stdio::null());
    if automatic {
        super::auto_push::configure(&mut command);
    }
    let output = command.output()?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("child command did not return a CLI result")?;
    ensure!(
        output.status.success() && value["ok"] == true,
        "{} failed: {}",
        arguments.first().map_or("command", String::as_str),
        value["diagnostics"]["stderr"]
    );
    Ok(value)
}

/// A project can publish only its own claims; foreign claims retain their source authority.
#[derive(Debug, PartialEq, Eq)]
enum Placement {
    /// The project repository holds, or will hold, the session's claim on this branch.
    Claim(String),
    /// Another repository holds the claim; the value is its `owner/repo@branch`.
    Foreign(String),
}

fn placement(project: &Project, candidate: &Candidate) -> Result<Placement> {
    crate::domain::merge_archive::RuntimeLinkKey {
        runtime: candidate.runtime.clone(),
        session_id: candidate.session_id.clone(),
    }
    .validate()?;
    let store = Store::at(config::store_root()?);
    if let Some(link) = link::get(&store, &candidate.runtime, &candidate.session_id) {
        ensure!(
            link.is_active() && link.merge_archive.is_none(),
            "session has an inactive or archived claim"
        );
        if let Some(branch) = &link.branch {
            let (Some(owner), Some(agent)) = (link.owner.as_deref(), link.agent.as_deref()) else {
                anyhow::bail!("session has an incomplete claim");
            };
            let source = format!("{owner}/{agent}");
            if source != project.repository {
                return Ok(Placement::Foreign(format!("{source}@{branch}")));
            }
            ensure!(
                link.native_binding.is_none(),
                "session has a protected claim"
            );
            return Ok(Placement::Claim(branch.clone()));
        }
        ensure!(
            link.native_binding.is_none(),
            "session has a protected claim"
        );
    }
    Ok(Placement::Claim(capture_branch(
        &candidate.runtime,
        &candidate.session_id,
    )))
}

/// The branch a project opens for a session it captures.
fn capture_branch(runtime: &str, id: &str) -> String {
    format!("project-{runtime}-{}", hash(&key(runtime, id)))
}

/// Resolve under the project lock so a moved claim cannot authorize another repository.
fn publication(project: &Project, candidate: &Candidate) -> Result<String> {
    match placement(project, candidate)? {
        Placement::Claim(branch) => Ok(branch),
        Placement::Foreign(source) => anyhow::bail!(
            "session is claimed by {source}; cross-repository publication requires preserved source authority; publish from its source repository"
        ),
    }
}

fn sync_one(project: &Project, candidate: &Candidate) -> Result<String> {
    verify(project)?;
    let branch = publication(project, candidate)?;
    let target = format!("{}@{branch}", project.repository);
    child(
        &[
            "import".into(),
            candidate.session_id.clone(),
            "--from".into(),
            candidate.runtime.clone(),
            "--into".into(),
            target.clone(),
            "--independent".into(),
        ],
        &candidate.cwd,
        true,
    )?;
    let store = Store::at(config::store_root()?);
    let link = link::get(&store, &candidate.runtime, &candidate.session_id)
        .context("import did not retain its claim")?;
    ensure!(
        link.cwd
            .as_deref()
            .and_then(|p| Path::new(p).canonicalize().ok())
            .as_ref()
            == Some(&candidate.cwd),
        "imported session directory differs from the selected source"
    );
    verify(project)?;
    let mut args = vec!["push".into(), target];
    if !project.auto_upload {
        args.push("--yes".into());
    }
    child(&args, &candidate.cwd, project.auto_upload)?;
    Ok(branch)
}

fn sync(project: &mut Project) -> CmdResult {
    let mut rows = Vec::new();
    let mut failed = false;
    for candidate in candidates(project)? {
        if project
            .excluded
            .contains(&key(&candidate.runtime, &candidate.session_id))
        {
            continue;
        }
        let outcome = match sync_one(project, &candidate) {
            Ok(branch) => {
                serde_json::json!({"branch": branch, "status": "pushed"})
            }
            Err(error) => {
                failed = true;
                serde_json::json!({"status": "failed", "error": format!("{error:#}")})
            }
        };
        rows.push(serde_json::json!({
            "runtime": candidate.runtime, "session_id": candidate.session_id,
            "outcome": outcome,
        }));
    }
    project.last_result = Some(serde_json::json!({"at":chrono::Utc::now(),"sessions":rows}));
    save(project)?;
    println!(
        "{}",
        serde_json::json!({"project":project.root,"repository":project.repository,"result":project.last_result})
    );
    Ok(if failed {
        ExitCode::Failure
    } else {
        ExitCode::Ok
    })
}

fn confirm(message: &str) -> Result<()> {
    if std::env::var_os("AGIT_YES").is_some() {
        return Ok(());
    }
    if !ui::prompt::may_prompt() || ui::prompt::confirm(message, false)? != Some(true) {
        return Err(super::InteractionRequired(format!(
            "{message}; review with --dry-run, then repeat with --yes"
        ))
        .into());
    }
    Ok(())
}

pub fn run(args: Args) -> CmdResult {
    match args.action {
        Action::Bind {
            path,
            repo,
            history,
            auto_upload,
            dry_run,
            rebind,
        } => bind(&path, &repo, history, auto_upload, dry_run, rebind),
        Action::Status { path } => {
            let root = root(&path)?;
            let project = read(&root)?.context("directory has no project policy")?;
            println!("{}", serde_json::to_string(&project)?);
            Ok(ExitCode::Ok)
        }
        Action::Unbind { path } => {
            let root = root(&path)?;
            let _guard = lock(&root)?;
            let mut project = read(&root)?.context("directory has no project policy")?;
            let _repository = repository_lock(&project.repository)?;
            project.enabled = false;
            save(&project)?;
            println!("Project capture stopped. Session history is unchanged.");
            restore_auto_push(&mut project)?;
            Ok(ExitCode::Ok)
        }
        Action::Drop { target } => drop_command(target.as_deref()),
        Action::Sync { path, dry_run } => {
            let root = root(&path)?;
            let mut project = read(&root)?.context("bind this directory first")?;
            let rows = candidates(&project)?;
            if dry_run {
                println!(
                    "{}",
                    serde_json::json!({"project":project.root,"candidates":rows,"excluded":project.excluded})
                );
                return Ok(ExitCode::Ok);
            }
            ensure!(
                project.enabled,
                "project is paused; bind again explicitly to resume"
            );
            confirm(
                "Import and publish this project's eligible sessions using its recorded policy?",
            )?;
            let _guard = lock(&root)?;
            project = read(&root)?.context("project policy disappeared")?;
            ensure!(project.enabled, "project is paused");
            sync(&mut project)
        }
        Action::Capture { path } => {
            drain(&root(&path)?)?;
            Ok(ExitCode::Ok)
        }
    }
}

/// A directory routes to one repository: its workspace binding and any existing project policy
/// must already name the requested one, unless `rebind` replaces them. Returns the existing
/// policy for that repository; a policy for another repository carries nothing over.
fn assignable(root: &Path, repository: &str, rebind: bool) -> Result<Option<Project>> {
    if let Some(binding) = workspace::read(root) {
        ensure!(
            rebind || binding.repo == repository,
            "directory is bound to {}; add `--rebind` to this command to move it to {repository}",
            binding.repo
        );
    }
    let Some(existing) = read(root)? else {
        return Ok(None);
    };
    if existing.repository == repository {
        return Ok(Some(existing));
    }
    ensure!(
        rebind,
        "project is already assigned to {}; add `--rebind` to this command to move it to {repository}",
        existing.repository
    );
    Ok(None)
}

fn bind(
    path: &Path,
    repository: &str,
    history: History,
    automatic: bool,
    dry_run: bool,
    rebind: bool,
) -> CmdResult {
    let root = root(path)?;
    let (owner, name) = super::parse_slug(repository)?;
    super::canonical_owner(&owner)?;
    crate::domain::repo::valid_name(&name)?;
    ensure!(
        !repository.contains('@'),
        "project binding requires OWNER/REPO, not a session branch"
    );
    let client = crate::hub::Client::from_env();
    let mut project = Project {
        root: root.clone(),
        repository: repository.into(),
        identity: RemoteIdentity {
            hub: identity::normalize_hub(client.base())?,
            agent_id: String::new(),
        },
        account: String::new(),
        account_id: None,
        enabled: true,
        auto_upload: automatic,
        history,
        excluded: BTreeSet::new(),
        last_result: None,
        push_auto_before: None,
    };
    let rows = candidates(&project)?;
    // A dry run answers whether the bind would go through, so it refuses what the bind refuses.
    assignable(&root, repository, rebind)?;
    if dry_run {
        println!(
            "{}",
            serde_json::json!({
                "directory": root, "repository": repository, "history": history,
                "repository_auto_upload": automatic, "repository_auto_push": automatic,
                "repository_auto_push_scope": automatic.then(|| auto_push_scope(repository)),
                "candidates": rows,
            })
        );
        return Ok(ExitCode::Ok);
    }
    ensure!(
        !crate::rc::harness::settlement_is_delegated(),
        "configure project capture in a device terminal outside the RC supervisor"
    );
    let _guard = lock(&root)?;
    if let Some(existing) = assignable(&root, repository, rebind)? {
        project.last_result = existing.last_result;
        // The override from before the first bind stays the one unbind restores; push.auto as
        // this bind finds it may be the value an earlier bind set.
        project.push_auto_before = existing.push_auto_before;
        if history == History::None {
            project.excluded = existing.excluded;
        }
    }
    if history == History::None {
        // Earlier sessions stay out whether or not another repository claims them; only a
        // session already claimed in this repository keeps publishing here.
        let store = Store::at(config::store_root()?);
        project.excluded.extend(
            rows.iter()
                .filter(|s| {
                    link::get(&store, &s.runtime, &s.session_id).is_none_or(|link| {
                        link.branch.is_none()
                            || format!(
                                "{}/{}",
                                link.owner.as_deref().unwrap_or_default(),
                                link.agent.as_deref().unwrap_or_default()
                            ) != repository
                    })
                })
                .map(|s| key(&s.runtime, &s.session_id)),
        );
    }
    let client = super::require_login()?;
    let remote = super::remote_request(client.get_agent(&owner, &name))?;
    project.identity = RemoteIdentity::new(client.base(), &remote.agent_id)?;
    ensure!(
        matches!(
            super::remote_request(client.push_access(&owner, &name, &remote.agent_id))?,
            crate::hub::PushAccess::Writable
        ),
        "project repository does not grant write access"
    );
    let me = super::remote_request(client.me())?;
    project.account = me.username;
    project.account_id = me.account_id;
    if automatic && remote.visibility == "public" {
        super::push::require_terminal_for_public(
            repository,
            &format!(
                "agit project bind {} --repo {repository} --history {} --auto-upload",
                root.display(),
                if history == History::All {
                    "all"
                } else {
                    "none"
                }
            ),
        )?;
    }
    let audience = Audience {
        visibility: Some(remote.visibility.clone()),
        org: owner_is_org(&client, &owner, &project.account),
    };
    println!(
        "Project: {} -> {repository} ({})",
        root.display(),
        remote.visibility
    );
    println!(
        "History: {history:?}; candidates: {}; automatic upload: {automatic}",
        rows.len()
    );
    if automatic {
        println!(
            "Automatic upload {}. Each publication validates its destination, session owners and snapshot privacy.",
            auto_push_scope(repository)
        );
    }
    confirm(
        "Bind this directory, accept independent histories for unclaimed sessions, and publish the selected project content?",
    )?;
    child(
        &["clone".into(), repository.into(), "--no-bind".into()],
        &root,
        false,
    )?;
    let repo = verify(&project)?;
    let mut repository_guard = None;
    if automatic {
        let runtimes = super::setup::project_hook_runtimes()?;
        ensure!(
            super::config::get("commit.auto").as_deref() != Some("false"),
            "commit.auto is disabled; enable it explicitly before automatic project capture"
        );
        let consent = super::push::publication_consent(&repo, repository, &project.identity)?;
        println!(
            "Automatic publication destination: {}",
            serde_json::to_string(&consent)?
        );
        confirm(&format!(
            "Confirm this account and destination? This {}",
            auto_push_scope(repository)
        ))?;
        for runtime in runtimes {
            child(
                &[
                    "setup".into(),
                    "--runtime".into(),
                    runtime.into(),
                    "--hooks".into(),
                ],
                &root,
                false,
            )?;
        }
        consent.save(&repo)?;
        repository_guard = Some(repository_lock(repository)?);
        enable_auto_push(&mut project, &repo)?;
    }
    workspace::bind(&root, repository, rebind)?;
    save(&project)?;
    drop(repository_guard);
    record_audience(&root, &audience)?;
    if history == History::All {
        sync(&mut project)
    } else {
        println!("{}", serde_json::to_string(&project)?);
        Ok(ExitCode::Ok)
    }
}

/// Exact hook identity remains authoritative; a project policy never overrides another claim.
pub(crate) fn capture(runtime: &str, session: &str, cwd: Option<&str>) -> Result<bool> {
    let Some(cwd) = cwd.and_then(|p| Path::new(p).canonicalize().ok()) else {
        return Ok(false);
    };
    let Some(project) = nearest(&cwd)? else {
        return Ok(false);
    };
    if !matches!(runtime, "codex" | "claude-code") || !includes(&project, &cwd)? {
        return Ok(false);
    }
    let candidate = Candidate {
        runtime: runtime.into(),
        session_id: session.into(),
        cwd,
    };
    // A session claimed elsewhere is left to its source repository.
    if !matches!(placement(&project, &candidate), Ok(Placement::Claim(_))) {
        return Ok(false);
    }
    if !project.enabled
        || !project.auto_upload
        || project
            .excluded
            .contains(&key(&candidate.runtime, &candidate.session_id))
    {
        return Ok(true);
    }
    enqueue(&project.root, &candidate)?;
    // The probe lock is released before spawning, so the new worker can take it.
    let idle = capture_worker(&project.root)?.is_some();
    if idle && spawn_capture(&project.root).is_err() {
        drain(&project.root)?;
    }
    Ok(true)
}

/// Import, inspection and upload outlast a runtime's hook budget for a long session, and a
/// runtime that ends its hook on timeout would end the upload with it. The hook therefore hands
/// the project to a detached worker that holds no pipe of the hook, so the turn ends at once.
fn spawn_capture(root: &Path) -> Result<()> {
    let mut command = crate::infra::background::command(std::env::current_exe()?);
    command
        .args(["--quiet", "project", "capture"])
        .arg(root)
        .current_dir(root)
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_YES")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn()?;
    Ok(())
}

fn pending_dir(root: &Path) -> Result<PathBuf> {
    Ok(state_path(root)?.with_extension("pending"))
}

/// A session's marker is keyed by the session, so Stops that arrive while a worker is busy
/// coalesce into one later capture of the session's latest content.
fn enqueue(root: &Path, candidate: &Candidate) -> Result<()> {
    let pending = pending_dir(root)?;
    config::create_state_dir(&pending)?;
    let mut file = tempfile::NamedTempFile::new_in(&pending)?;
    file.write_all(&serde_json::to_vec(candidate)?)?;
    file.persist(pending.join(hash(&key(&candidate.runtime, &candidate.session_id))))
        .map_err(|e| e.error)?;
    Ok(())
}

/// The queued markers; a temporary file of an interrupted enqueue is not one.
fn markers(root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(pending_dir(root)?) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut markers = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.len() == 24 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            markers.push(path);
        }
    }
    Ok(markers)
}

/// Removes one marker before its capture, so a Stop during that capture queues the session again.
fn take_pending(root: &Path) -> Result<Option<Candidate>> {
    for path in markers(root)? {
        let bytes = fs::read(&path)?;
        fs::remove_file(&path)?;
        if let Ok(candidate) = serde_json::from_slice(&bytes) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// At most one worker captures a project. `None` means another worker holds it and will see
/// any marker written before it lets go.
fn capture_worker(root: &Path) -> Result<Option<fs::File>> {
    config::create_state_dir(&home()?)?;
    let file = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_path(root)?.with_extension("capture"))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(e) if crate::infra::local_state::is_contended(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Publishes queued sessions one at a time. After letting the worker lock go it looks once more,
/// so a marker written while it was finishing is not left for the next turn.
fn drain(root: &Path) -> Result<()> {
    loop {
        let Some(worker) = capture_worker(root)? else {
            return Ok(());
        };
        while let Some(candidate) = take_pending(root)? {
            let Some(project) = read(root)? else {
                return Ok(());
            };
            // A failure is recorded in the project's last result; the remaining sessions go on.
            let _ = capture_candidate(project, candidate);
        }
        drop(worker);
        if markers(root)?.is_empty() {
            return Ok(());
        }
    }
}

fn capture_candidate(mut project: Project, candidate: Candidate) -> Result<bool> {
    // Automatic capture leaves foreign claims to their source repository's settlement.
    if !matches!(placement(&project, &candidate), Ok(Placement::Claim(_))) {
        return Ok(false);
    }
    let session_key = key(&candidate.runtime, &candidate.session_id);
    if !project.enabled || !project.auto_upload || project.excluded.contains(&session_key) {
        return Ok(true);
    }
    let _guard = lock(&project.root)?;
    project = read(&project.root)?.context("project policy disappeared")?;
    if !project.enabled || !project.auto_upload || project.excluded.contains(&session_key) {
        return Ok(true);
    }
    // A queued capture outlives its Stop: a directory bound on its own in between belongs to
    // that binding's history and upload choices, not to this project's.
    if !includes(&project, &candidate.cwd)? {
        return Ok(false);
    }
    let result = sync_one(&project, &candidate);
    project.last_result = Some(
        serde_json::json!({"at":chrono::Utc::now(),"session_id":candidate.session_id,"ok":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:#}"))}),
    );
    save(&project)?;
    result?;
    Ok(true)
}

// ── publication withheld by project policy ─────────────────────────────

/// Branches withheld from publication, kept apart from the policies so that a dropped claim stays
/// withheld after its link, and even its project, is gone.
const WITHHELD: &str = "withheld";
/// Present once this machine's existing claims have been judged by the capture rule.
const RECHECKED: &str = "claims-rechecked";

/// Why a project keeps a branch from being published, whatever claim the branch carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Withholding {
    /// The project captured a conversation that started outside its folder.
    StartedOutside,
    /// The project cannot find where the captured conversation started.
    UnknownStart,
    /// The user excluded the session from the project.
    Excluded,
    /// The user dropped the claim.
    Dropped,
}

impl Withholding {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::StartedOutside => "the conversation started outside the project folder",
            Self::UnknownStart => "agit cannot verify where the conversation started",
            Self::Excluded => "the conversation is excluded from the project",
            Self::Dropped => "the conversation was dropped from the project",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Withheld {
    repository: String,
    branch: String,
    session: Option<String>,
    reason: Withholding,
    announced: bool,
}

/// Every project policy on this machine. An unreadable policy fails the caller, since it may be
/// the one that withholds a branch.
fn policies() -> Result<Vec<Project>> {
    let entries = match fs::read_dir(home()?) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut projects = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let is_policy = path.extension().is_some_and(|e| e == "json")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| {
                    stem.len() == 24 && stem.bytes().all(|b| b.is_ascii_hexdigit())
                });
        if is_policy {
            projects.push(
                serde_json::from_slice(&fs::read(&path)?)
                    .with_context(|| format!("invalid project policy {}", path.display()))?,
            );
        }
    }
    Ok(projects)
}

fn withheld_records() -> Result<Vec<Withheld>> {
    match fs::read(home()?.join(WITHHELD)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid withheld branch list"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Read-modify-write of the withheld list under its own lock.
fn update_withheld<T>(change: impl FnOnce(&mut Vec<Withheld>) -> T) -> Result<T> {
    let home = home()?;
    config::create_state_dir(&home)?;
    let lock_path = home.join(WITHHELD).with_extension("lock");
    let guard = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    crate::infra::local_state::lock_patiently(
        &guard,
        &lock_path,
        "the withheld branch lock",
        true,
    )?;
    let mut records = withheld_records()?;
    let result = change(&mut records);
    let mut file = tempfile::NamedTempFile::new_in(&home)?;
    file.write_all(&serde_json::to_vec_pretty(&records)?)?;
    file.as_file().sync_all()?;
    file.persist(home.join(WITHHELD)).map_err(|e| e.error)?;
    Ok(result)
}

/// Where the conversation behind a claim started; `None` when its transcript or its first
/// recorded directory cannot be found.
fn start(claim: &link::Link) -> Option<PathBuf> {
    let transcript = claim.resolve()?;
    let start = crate::tui::screens::selector::recorded_cwd(&transcript)?;
    Path::new(&start).canonicalize().ok()
}

/// Whether a branch carries the name a project capture gives a session. A resumed conversation
/// keeps recording into that branch under a new session id, so the name, not the current claim,
/// says that the branch began as a capture.
fn is_capture_branch(branch: &str) -> bool {
    branch
        .strip_prefix("project-")
        .and_then(|rest| rest.rsplit_once('-'))
        .is_some_and(|(runtime, hash)| {
            !runtime.is_empty() && hash.len() == 24 && hash.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

/// The capture rule applied to one branch of the projects' repository, given every claim that
/// records into it, current or superseded.
///
/// The conversation that opened a capture branch is judged by where it started, and the branch is
/// sound when any project bound to the repository contains that start: the same repository may be
/// bound from several folders, and a conversation that belongs to one of them is not outside
/// because it is outside another. A branch the user named, without a capture's name, is theirs
/// and only an exclusion withholds it.
fn judge(projects: &[Project], branch: &str, claims: &[link::Link]) -> Option<Withholding> {
    let excluded = claims.iter().any(|claim| {
        let session = key(&claim.source, &claim.session_id);
        projects.iter().any(|p| p.excluded.contains(&session))
    });
    if excluded {
        return Some(Withholding::Excluded);
    }
    if !is_capture_branch(branch) {
        return None;
    }
    let opener = claims
        .iter()
        .find(|claim| capture_branch(&claim.source, &claim.session_id) == branch);
    match opener.and_then(start) {
        Some(start)
            if projects
                .iter()
                .any(|p| includes(p, &start).unwrap_or(false)) =>
        {
            None
        }
        Some(_) => Some(Withholding::StartedOutside),
        None => Some(Withholding::UnknownStart),
    }
}

/// Every claim, current or superseded, that records into `owner/agent@branch`.
fn recording_into(all: &[link::Link], owner: &str, agent: &str, branch: &str) -> Vec<link::Link> {
    all.iter()
        .filter(|claim| {
            claim.agent.as_deref() == Some(agent)
                && claim.branch.as_deref() == Some(branch)
                && claim.owner.as_deref().is_none_or(|o| o == owner)
        })
        .cloned()
        .collect()
}

/// Why `slug@branch` must not be published from this machine, if it must not.
///
/// Publication checks the capture rule again instead of trusting the claim: a claim made under a
/// looser rule, or one the user excluded afterwards, would otherwise upload a conversation the
/// project never had the right to capture.
pub(crate) fn withheld(slug: &str, branch: &str) -> Result<Option<Withholding>> {
    if let Some(record) = withheld_records()?
        .into_iter()
        .find(|r| r.repository == slug && r.branch == branch)
    {
        return Ok(Some(record.reason));
    }
    let projects: Vec<_> = policies()?
        .into_iter()
        .filter(|p| p.repository == slug)
        .collect();
    if projects.is_empty() {
        return Ok(None);
    }
    // An excluded session's capture branch stays withheld after its claim is gone.
    let excluded_branch = projects.iter().flat_map(|p| &p.excluded).any(|session| {
        session
            .split_once(':')
            .is_some_and(|(runtime, id)| capture_branch(runtime, id) == branch)
    });
    if excluded_branch {
        return Ok(Some(Withholding::Excluded));
    }
    let (owner, agent) = super::parse_slug(slug)?;
    let store = Store::at(config::store_root()?);
    let claims = recording_into(&link::list(&store), &owner, &agent, branch);
    Ok(judge(&projects, branch, &claims))
}

/// Whether a project on this machine is bound to `slug`, so `project drop` can act on its branches.
pub(crate) fn binds(slug: &str) -> bool {
    policies().is_ok_and(|projects| projects.iter().any(|p| p.repository == slug))
}

/// Withholds the branch for good, then releases its current claims: the branch keeps its local
/// history, and no session records into it any more.
fn release(
    store: &Store,
    slug: &str,
    branch: &str,
    claims: &[link::Link],
    reason: Withholding,
    announced: bool,
) -> Result<()> {
    let session = claims
        .iter()
        .find(|c| c.is_active())
        .map(|c| key(&c.source, &c.session_id));
    update_withheld(|records| {
        if !records
            .iter()
            .any(|r| r.repository == slug && r.branch == branch)
        {
            records.push(Withheld {
                repository: slug.into(),
                branch: branch.into(),
                session: session.clone(),
                reason,
                announced,
            });
        }
    })?;
    let (owner, agent) = super::parse_slug(slug)?;
    for claim in claims.iter().filter(|c| c.is_active()) {
        let _guard = link::lock(store, &claim.source, &claim.session_id)?;
        if link::get(store, &claim.source, &claim.session_id)
            .is_some_and(|current| link::claims_branch(&current, &owner, &agent, branch))
        {
            fs::remove_file(link::link_path(store, &claim.source, &claim.session_id))?;
        }
    }
    Ok(())
}

/// Judges this machine's existing claims once, so a claim made under a looser capture rule is
/// dropped without the user having to know a command, and records what it dropped for
/// [`announce_dropped`]. A failure leaves the judgement for the next command; publication checks
/// each branch on its own meanwhile.
pub fn recheck_claims(announce: bool) {
    let _ = recheck();
    if announce {
        let _ = announce_dropped();
    }
}

fn recheck() -> Result<()> {
    let home = home()?;
    if !home.is_dir() || home.join(RECHECKED).exists() {
        return Ok(());
    }
    let projects = policies()?;
    let store = Store::at(config::store_root()?);
    let all = link::list(&store);
    let branches: BTreeSet<_> = all
        .iter()
        .filter(|c| c.is_active())
        .filter_map(|c| Some((c.owner.clone()?, c.agent.clone()?, c.branch.clone()?)))
        .collect();
    for (owner, agent, branch) in branches {
        let slug = format!("{owner}/{agent}");
        let bound: Vec<_> = projects
            .iter()
            .filter(|p| p.repository == slug)
            .cloned()
            .collect();
        if bound.is_empty() {
            continue;
        }
        let claims = recording_into(&all, &owner, &agent, &branch);
        // A start that cannot be found is withheld at publication but not dropped: the claim
        // may be sound, and only the user can tell.
        match judge(&bound, &branch, &claims) {
            None | Some(Withholding::UnknownStart) => {}
            Some(reason) => release(&store, &slug, &branch, &claims, reason, false)?,
        }
    }
    fs::write(home.join(RECHECKED), b"1\n")?;
    Ok(())
}

/// Says once which claims [`recheck`] dropped.
fn announce_dropped() -> Result<()> {
    if !withheld_records()?.iter().any(|r| !r.announced) {
        return Ok(());
    }
    let dropped = update_withheld(|records| {
        records
            .iter_mut()
            .filter(|r| !r.announced)
            .map(|r| {
                r.announced = true;
                format!(
                    "{}@{}: {}; its history: `agit log {}@{}`",
                    r.repository,
                    r.branch,
                    r.reason.describe(),
                    r.repository,
                    r.branch
                )
            })
            .collect::<Vec<_>>()
    })?;
    if dropped.is_empty() {
        return Ok(());
    }
    ui::warning(&format!(
        "these conversations will not be uploaded from this computer; their local history stays:\n  {}",
        dropped.join("\n  ")
    ));
    ui::hint("a copy already pushed to the Hub stays there until you remove it on the Hub");
    Ok(())
}

/// Keeps a project from capturing the session into a new branch.
fn exclude(root: &Path, session: &str) -> Result<()> {
    let _guard = lock(root)?;
    if let Some(mut current) = read(root)?
        && current.excluded.insert(session.into())
    {
        save(&current)?;
    }
    Ok(())
}

/// The branch of the conversation this command runs in. A conversation recorded into no branch
/// is only excluded from the project its start belongs to, and `None` is returned.
fn this_conversation() -> Result<Option<String>> {
    let sessions = super::context::runtime_sessions();
    let [session] = sessions.as_slice() else {
        anyhow::bail!(
            "{}; name the conversation as OWNER/REPO@BRANCH",
            if sessions.is_empty() {
                "this command does not run inside a conversation"
            } else {
                "the environment names several conversations"
            }
        );
    };
    if session.target.is_some() {
        return Ok(session.target.clone());
    }
    let start = link::Link::new(session.runtime, &session.session_id, None)
        .resolve()
        .and_then(|transcript| crate::tui::screens::selector::recorded_cwd(&transcript))
        .and_then(|start| Path::new(&start).canonicalize().ok());
    if let Some(start) = start
        && let Some(project) = nearest(&start)?
        && includes(&project, &start)?
    {
        exclude(&project.root, &key(session.runtime, &session.session_id))?;
    }
    Ok(None)
}

fn drop_command(target: Option<&str>) -> CmdResult {
    let target = match target {
        Some(target) => target.to_owned(),
        None => match this_conversation()? {
            Some(target) => target,
            None => {
                println!(
                    "This conversation will not be uploaded from this computer; its local history stays."
                );
                return Ok(ExitCode::Ok);
            }
        },
    };
    let (repository, branch) = target
        .split_once('@')
        .filter(|(_, branch)| !branch.is_empty())
        .context("name the conversation as OWNER/REPO@BRANCH")?;
    let (owner, agent) = super::parse_slug(repository)?;
    let projects: Vec<_> = policies()?
        .into_iter()
        .filter(|p| p.repository == repository)
        .collect();
    let store = Store::at(config::store_root()?);
    let claims = link::active_for_branch(&store, &owner, &agent, branch);
    // A conversation imported by hand records into its branch without any project, and dropping
    // it must still stop that; only a target that neither a project nor a conversation names is
    // a mistake.
    ensure!(
        !projects.is_empty() || !claims.is_empty(),
        "no project on this machine is bound to {repository}, and no conversation here records into {target}"
    );
    for claim in &claims {
        for project in &projects {
            exclude(&project.root, &key(&claim.source, &claim.session_id))?;
        }
    }
    release(
        &store,
        repository,
        branch,
        &claims,
        Withholding::Dropped,
        true,
    )?;
    println!("{target} will not be uploaded from this computer; its local history stays.");
    ui::hint("a copy already pushed to the Hub stays there until you remove it on the Hub");
    Ok(ExitCode::Ok)
}

/// Where a project uploads a conversation by itself, and who can see that repository.
pub(crate) struct AutomaticUpload {
    pub(crate) repository: String,
    /// `None` when neither the Hub nor an earlier answer from it says.
    pub(crate) visibility: Option<String>,
    /// Whether the owner is an organization, when known.
    pub(crate) org: Option<bool>,
}

/// The repository a project uploads this conversation to by itself, judged by the folder the
/// conversation started in. `None` when no project there uploads automatically, the conversation
/// is excluded, or it records into another repository. Who can see it is the Hub's answer within
/// `wait`, recorded for later, or else the last recorded answer; the requests never renew
/// credentials, so a caller may run them inside a hook.
pub(crate) fn automatic_upload(
    runtime: &str,
    session: &str,
    origin: &Path,
    wait: std::time::Duration,
) -> Option<AutomaticUpload> {
    let project = nearest(origin).ok()??;
    if !project.enabled
        || !project.auto_upload
        || !includes(&project, origin).ok()?
        || project.excluded.contains(&key(runtime, session))
    {
        return None;
    }
    let claimed_elsewhere = Store::open()
        .ok()
        .flatten()
        .and_then(|store| link::get(&store, runtime, session))
        .is_some_and(|link| {
            link.branch.is_some()
                && format!(
                    "{}/{}",
                    link.owner.as_deref().unwrap_or_default(),
                    link.agent.as_deref().unwrap_or_default()
                ) != project.repository
        });
    if claimed_elsewhere {
        return None;
    }
    let recorded = recorded_audience(&project.root);
    let answered = crate::hub::Client::from_env_with_live_access(wait)
        .filter(|client| {
            identity::normalize_hub(client.base()).ok().as_deref()
                == Some(project.identity.hub.as_str())
        })
        .and_then(|client| {
            let (owner, name) = super::parse_slug(&project.repository).ok()?;
            let remote = client.get_agent(&owner, &name).ok()?;
            (remote.agent_id == project.identity.agent_id).then(|| Audience {
                visibility: Some(remote.visibility),
                org: recorded
                    .org
                    .or_else(|| owner_is_org(&client, &owner, &project.account)),
            })
        });
    let audience = match answered {
        Some(audience) => {
            let _ = record_audience(&project.root, &audience);
            audience
        }
        None => recorded,
    };
    Some(AutomaticUpload {
        repository: project.repository,
        visibility: audience.visibility.filter(|v| !v.is_empty()),
        org: audience.org,
    })
}

pub(crate) fn registration_scope(cwd: &Path) -> bool {
    nearest(cwd)
        .ok()
        .flatten()
        .is_some_and(|p| p.enabled && p.auto_upload && includes(&p, cwd).unwrap_or(false))
}

pub(crate) fn blocks_auto_push(repo: &Repo, slug: &str, branch: &str) -> Result<bool> {
    if withheld(slug, branch)?.is_some() {
        return Ok(true);
    }
    let meta = crate::domain::meta::read_at_ref_result(repo, &format!("refs/heads/{branch}"))?
        .context("cannot verify project policy without branch metadata")?;
    if meta.is_file_line() {
        return Ok(false);
    }
    let hydrated = crate::domain::privacy::service::transform(
        Some(repo.root()),
        &meta.cwd,
        crate::domain::privacy::projector::Mode::HydrateText,
    );
    if crate::domain::privacy::projector::tokens(&hydrated.content)
        .next()
        .is_some()
    {
        return Ok(false);
    }
    let cwd = hydrated.content;
    let cwd = Path::new(&cwd)
        .canonicalize()
        .context("cannot resolve the session directory to verify project policy")?;
    let Some(project) = nearest(&cwd)? else {
        return Ok(false);
    };
    Ok(project.repository == slug
        && includes(&project, &cwd)?
        && (!project.enabled || !project.auto_upload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::meta::{self, Meta};

    fn isolated(name: &str) -> bool {
        const CHILD: &str = "AGIT_PROJECT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("commands::project::tests::{name}"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("AGIT_HOME", home.path())
            // Candidate discovery never reads the developer's own transcripts.
            .env("CLAUDE_CONFIG_DIR", home.path().join("claude"))
            .env("CODEX_HOME", home.path().join("codex"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    fn project(root: &Path) -> Project {
        Project {
            root: root.canonicalize().unwrap(),
            repository: "alice/app".into(),
            identity: RemoteIdentity::new(
                "https://example.test",
                "bbbbbbbb-0000-4000-8000-000000000002",
            )
            .unwrap(),
            account: "alice".into(),
            account_id: None,
            enabled: true,
            auto_upload: true,
            history: History::All,
            excluded: BTreeSet::new(),
            last_result: None,
            push_auto_before: None,
        }
    }

    /// A conversation is announced as uploading only where the project would upload it: started
    /// inside an automatic project, not excluded, not recording into another repository. The
    /// visibility comes from the Hub or its last recorded answer, never a default; this policy's
    /// Hub is not the configured one, so no request is sent.
    #[test]
    fn only_a_conversation_the_project_uploads_is_announced() {
        if !isolated("only_a_conversation_the_project_uploads_is_announced") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        let inside = policy.root.join("src");
        fs::create_dir_all(&inside).unwrap();
        policy.excluded.insert(key("codex", "excluded"));
        save(&policy).unwrap();
        let wait = std::time::Duration::from_millis(1);
        let announced = |id: &str, origin: &Path| {
            automatic_upload("codex", id, origin, wait).map(|upload| upload.repository)
        };
        assert_eq!(announced("fresh", &inside).as_deref(), Some("alice/app"));
        assert!(announced("excluded", &inside).is_none());
        assert!(announced("fresh", policy.root.parent().unwrap()).is_none());
        let store = Store::at(config::store_root().unwrap());
        let mut claim = link::Link::new("codex", "elsewhere", Some(&inside));
        claim.owner = Some("bob".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("work".into());
        link::write(&store, &claim).unwrap();
        assert!(announced("elsewhere", &inside).is_none());

        let audience =
            || automatic_upload("codex", "fresh", &inside, wait).map(|u| (u.visibility, u.org));
        assert_eq!(audience(), Some((None, None)));
        record_audience(
            &policy.root,
            &Audience {
                visibility: Some("private".into()),
                org: Some(true),
            },
        )
        .unwrap();
        assert_eq!(audience(), Some((Some("private".into()), Some(true))));

        policy.auto_upload = false;
        save(&policy).unwrap();
        assert!(announced("fresh", &inside).is_none());
    }

    /// A foreign claim cannot become a new import or silently change publication authority.
    #[test]
    fn project_publication_retains_claims_and_refuses_foreign_authority() {
        if !isolated("project_publication_retains_claims_and_refuses_foreign_authority") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let project = project(work.path());
        let store = Store::at(config::store_root().unwrap());
        let candidate = |id: &str| Candidate {
            runtime: "codex".into(),
            session_id: id.into(),
            cwd: project.root.clone(),
        };
        let claim = |id: &str, owner: &str, agent: &str, branch: &str| {
            let mut claim = link::Link::new("codex", id, Some(&project.root));
            claim.owner = Some(owner.into());
            claim.agent = Some(agent.into());
            claim.branch = Some(branch.into());
            link::write(&store, &claim).unwrap();
        };
        let rc = "cccccccc-0000-4000-8000-000000000003";
        claim(rc, "desktop-machine", "project-local", "rc-branch");
        assert_eq!(
            placement(&project, &candidate(rc)).unwrap(),
            Placement::Foreign("desktop-machine/project-local@rc-branch".into())
        );
        let own = "dddddddd-0000-4000-8000-000000000004";
        claim(own, "alice", "app", "kept");
        assert_eq!(
            placement(&project, &candidate(own)).unwrap(),
            Placement::Claim("kept".into())
        );
        let fresh = "eeeeeeee-0000-4000-8000-000000000005";
        assert!(matches!(
            placement(&project, &candidate(fresh)).unwrap(),
            Placement::Claim(branch) if branch.starts_with("project-codex-")
        ));
        // A claim can move while capture waits for the project lock.
        assert!(publication(&project, &candidate(fresh)).is_ok());
        claim(fresh, "desktop-machine", "project-local", "moved");
        let path = link::link_path(&store, "codex", fresh);
        let before = fs::read(&path).unwrap();
        let error = publication(&project, &candidate(fresh)).unwrap_err();
        assert!(error.to_string().contains("preserved source authority"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    /// A Claude Code session is a candidate for its directory when its first directory-bearing
    /// record runs past the preview window, including when that record is too long to buffer and
    /// a later record names the same directory. Discovery that parses only the preview window
    /// drops both sessions, and discovery that stops at an unbuffered record drops the second;
    /// either way binding with history or syncing never imports them.
    #[test]
    fn claude_session_opening_with_a_long_prompt_is_a_candidate() {
        if !isolated("claude_session_opening_with_a_long_prompt_is_a_candidate") {
            return;
        }
        use crate::tui::screens::selector::{CWD_RECORD_BYTES, PREVIEW_BYTES};
        let work = tempfile::tempdir().unwrap();
        let project = project(work.path());
        let native = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap())
            .join("projects")
            .join(adapter::claude_code::slug_for(&project.root));
        fs::create_dir_all(&native).unwrap();
        let write = |id: &str, pasted: usize| {
            let queued = serde_json::json!({"type": "queue-operation",
                "operation": "enqueue", "sessionId": id});
            let prompt = serde_json::json!({"type": "user", "isSidechain": false,
                "sessionId": id, "cwd": project.root,
                "message": {"role": "user", "content": [{"type": "image",
                    "source": {"type": "base64", "data": "A".repeat(pasted)}}]}});
            let attachment = serde_json::json!({"type": "attachment", "isSidechain": false,
                "sessionId": id, "cwd": project.root});
            fs::write(
                native.join(format!("{id}.jsonl")),
                format!("{queued}\n{prompt}\n{attachment}\n"),
            )
            .unwrap();
        };
        let beyond_preview = "aaaaaaaa-0000-4000-8000-000000000011";
        write(beyond_preview, 2 * PREVIEW_BYTES as usize);
        let beyond_buffer = "bbbbbbbb-0000-4000-8000-000000000012";
        write(beyond_buffer, CWD_RECORD_BYTES + 1);
        let mut found: Vec<_> = candidates(&project)
            .unwrap()
            .into_iter()
            .map(|candidate| (candidate.runtime, candidate.session_id, candidate.cwd))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [beyond_preview, beyond_buffer].map(|id| (
                "claude-code".to_owned(),
                id.to_owned(),
                project.root.clone()
            ))
        );
    }

    /// A dry run refuses what the bind refuses: a directory its workspace binding or an existing
    /// project policy already routes to another repository, unless `--rebind` moves it. A dry run
    /// that lists candidates before these checks reports a bind that cannot happen, and a
    /// `--rebind` that still honors either route leaves the directory with no way to move.
    #[test]
    fn dry_run_refuses_a_directory_routed_to_another_repository() {
        if !isolated("dry_run_refuses_a_directory_routed_to_another_repository") {
            return;
        }
        let bound = tempfile::tempdir().unwrap();
        workspace::bind(bound.path(), "alice/other", false).unwrap();
        let error = bind(bound.path(), "alice/app", History::None, false, true, false).unwrap_err();
        assert!(
            format!("{error:#}").contains("add `--rebind` to this command to move it to alice/app"),
            "{error:#}"
        );
        assert!(bind(bound.path(), "alice/app", History::None, false, true, true).is_ok());

        let enrolled = tempfile::tempdir().unwrap();
        save(&project(enrolled.path())).unwrap();
        let error = bind(
            enrolled.path(),
            "alice/other",
            History::None,
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("already assigned to alice/app"),
            "{error:#}"
        );
        assert!(
            bind(
                enrolled.path(),
                "alice/app",
                History::None,
                false,
                true,
                false
            )
            .is_ok()
        );
        assert!(
            bind(
                enrolled.path(),
                "alice/other",
                History::None,
                false,
                true,
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn capture_rechecks_history_exclusions_after_locking() {
        if !isolated("capture_rechecks_history_exclusions_after_locking") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let stale = project(work.path());
        let candidate = Candidate {
            runtime: "codex".into(),
            session_id: "cccccccc-0000-4000-8000-000000000003".into(),
            cwd: stale.root.clone(),
        };
        let mut latest = stale.clone();
        latest.history = History::None;
        latest
            .excluded
            .insert(key(&candidate.runtime, &candidate.session_id));
        let guard = lock(&latest.root).unwrap();
        save(&latest).unwrap();
        let worker = std::thread::spawn(move || capture_candidate(stale, candidate));
        drop(guard);
        assert!(worker.join().unwrap().unwrap());
        assert!(read(&latest.root).unwrap().unwrap().last_result.is_none());
    }

    /// Stops that arrive while a worker holds the project coalesce into one marker per session,
    /// and another drainer leaves them to that worker. A worker per Stop would pile up waiting
    /// processes that each import, inspect and upload the same session again.
    #[test]
    fn busy_capture_worker_coalesces_stops_into_one_worker() {
        if !isolated("busy_capture_worker_coalesces_stops_into_one_worker") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let project = project(work.path());
        save(&project).unwrap();
        let candidate = |id: &str| Candidate {
            runtime: "codex".into(),
            session_id: id.into(),
            cwd: project.root.clone(),
        };
        let worker = capture_worker(&project.root).unwrap().unwrap();
        for _ in 0..3 {
            enqueue(
                &project.root,
                &candidate("cccccccc-0000-4000-8000-000000000003"),
            )
            .unwrap();
        }
        enqueue(
            &project.root,
            &candidate("dddddddd-0000-4000-8000-000000000004"),
        )
        .unwrap();
        assert_eq!(markers(&project.root).unwrap().len(), 2);
        assert!(capture_worker(&project.root).unwrap().is_none());
        drain(&project.root).unwrap();
        assert_eq!(markers(&project.root).unwrap().len(), 2);
        drop(worker);
        drain(&project.root).unwrap();
        assert!(markers(&project.root).unwrap().is_empty());
        assert!(read(&project.root).unwrap().unwrap().last_result.is_some());
    }

    /// A capture queued before its directory was bound on its own is skipped, so the parent
    /// project does not import or publish a session the nested binding now owns.
    #[test]
    fn queued_capture_respects_a_nested_binding_made_afterwards() {
        if !isolated("queued_capture_respects_a_nested_binding_made_afterwards") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let parent = project(work.path());
        save(&parent).unwrap();
        let nested = parent.root.join("nested");
        fs::create_dir(&nested).unwrap();
        enqueue(
            &parent.root,
            &Candidate {
                runtime: "codex".into(),
                session_id: "cccccccc-0000-4000-8000-000000000003".into(),
                cwd: nested.clone(),
            },
        )
        .unwrap();
        let mut child = project(&nested);
        child.repository = "alice/nested".into();
        child.auto_upload = false;
        save(&child).unwrap();
        drain(&parent.root).unwrap();
        assert!(markers(&parent.root).unwrap().is_empty());
        assert!(read(&parent.root).unwrap().unwrap().last_result.is_none());
    }

    #[test]
    fn paused_project_blocks_its_registered_session_directory() {
        if !isolated("paused_project_blocks_its_registered_session_directory") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("private-project-value");
        fs::create_dir(&work).unwrap();
        let project = project(&work);
        save(&project).unwrap();
        let repo = Repo::init(&temp.path().join("repo")).unwrap();
        repo.git(&["checkout", "-b", "work"]).unwrap();
        let mut metadata = Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "codex".into(),
            project.root.to_string_lossy().into_owned(),
        );
        meta::write(repo.root(), &metadata).unwrap();
        repo.add_all().unwrap();
        repo.commit("Protected project directory").unwrap();
        assert!(!blocks_auto_push(&repo, "alice/app", "work").unwrap());
        run(Args {
            action: Action::Unbind { path: work },
        })
        .unwrap();
        assert!(blocks_auto_push(&repo, "alice/app", "work").unwrap());

        metadata.cwd = temp.path().join("missing").to_string_lossy().into_owned();
        meta::write(repo.root(), &metadata).unwrap();
        repo.add_all().unwrap();
        repo.commit("Unavailable project directory").unwrap();
        assert!(blocks_auto_push(&repo, "alice/app", "work").is_err());
    }

    /// Unbind leaves the repository's push.auto as it was before the first folder was bound,
    /// whatever that was, when two folders bind the same repository and are unbound in the
    /// order they were bound; the first unbind leaves automatic push on for the other folder.
    /// An unbind that kept bind's override would leave every managed session of the repository
    /// uploading; one that cleared it would turn off automatic push chosen before binding; a
    /// second folder that recorded the override the first one set would turn it back on.
    /// A record is never written into another clone at the same path.
    #[test]
    fn unbind_restores_push_auto_from_before_the_bind() {
        if !isolated("unbind_restores_push_auto_from_before_the_bind") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = config::repo_dir("alice", "app").unwrap();
        let unbind = |work: &Path| {
            run(Args {
                action: Action::Unbind { path: work.into() },
            })
            .unwrap();
        };
        for (index, (user, before)) in [
            (None, None),
            (None, Some(false)),
            (None, Some(true)),
            (Some("true"), None),
        ]
        .into_iter()
        .enumerate()
        {
            config::set_global("push.auto", user).unwrap();
            let _ = fs::remove_dir_all(&dir);
            let repo = Repo::init(&dir).unwrap();
            repo.set_auto_push(before).unwrap();
            let folders: Vec<Project> = ["a", "b"]
                .iter()
                .map(|name| {
                    let work = temp.path().join(format!("{index}{name}"));
                    fs::create_dir(&work).unwrap();
                    let mut project = project(&work);
                    identity::pin(&repo, &project.identity).unwrap();
                    enable_auto_push(&mut project, &repo).unwrap();
                    save(&project).unwrap();
                    project
                })
                .collect();
            unbind(&folders[0].root);
            assert!(repo.auto_push_enabled().unwrap());
            unbind(&folders[1].root);
            assert_eq!(repo.auto_push_override().unwrap(), before);
            for folder in &folders {
                assert_eq!(read(&folder.root).unwrap().unwrap().push_auto_before, None);
            }
        }

        config::set_global("push.auto", None).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        let work = temp.path().join("recloned");
        fs::create_dir(&work).unwrap();
        let mut project = project(&work);
        project.push_auto_before = Some(PushAutoBefore::Off);
        save(&project).unwrap();
        let other = Repo::init(&dir).unwrap();
        identity::pin(
            &other,
            &RemoteIdentity::new(
                "https://example.test",
                "cccccccc-0000-4000-8000-000000000003",
            )
            .unwrap(),
        )
        .unwrap();
        other.set_auto_push(Some(true)).unwrap();
        unbind(&work);
        assert_eq!(other.auto_push_override().unwrap(), Some(true));
    }

    /// A Claude Code transcript that starts in `start`, and the project capture claim on it.
    fn captured(project: &Project, id: &str, start: &Path) -> link::Link {
        let native = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap())
            .join("projects")
            .join(adapter::claude_code::slug_for(start));
        fs::create_dir_all(&native).unwrap();
        let record = |cwd: &Path| serde_json::json!({"type": "user", "sessionId": id, "cwd": cwd});
        fs::write(
            native.join(format!("{id}.jsonl")),
            format!("{}\n{}\n", record(start), record(&project.root)),
        )
        .unwrap();
        // A capture records the hook's directory, so the claim itself names the project.
        let mut claim = link::Link::new("claude-code", id, Some(&project.root));
        claim.owner = Some("alice".into());
        claim.agent = Some("app".into());
        claim.branch = Some(capture_branch("claude-code", id));
        link::write(&Store::at(config::store_root().unwrap()), &claim).unwrap();
        claim
    }

    /// Publication judges a project capture branch by where its conversation started and by the
    /// project's exclusions, not by the claim: a branch captured from a conversation started
    /// outside the folder, or a session excluded later, is refused by push and by automatic push
    /// while push.auto is on, and so is such a branch after a resume moved it to a new session id.
    /// A gate that trusted the claim would upload these; one that judged a conversation by every
    /// folder bound to the repository would refuse a conversation of the other folder; one that
    /// withheld every capture branch would stop the project's own sessions.
    #[test]
    fn publication_rechecks_the_capture_rule_whatever_the_claim_says() {
        if !isolated("publication_rechecks_the_capture_rule_whatever_the_claim_says") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut project = project(work.path());
        save(&project).unwrap();
        let inside = captured(
            &project,
            "aaaaaaaa-0000-4000-8000-000000000021",
            &project.root,
        );
        let strayed = captured(
            &project,
            "bbbbbbbb-0000-4000-8000-000000000022",
            &outside.path().canonicalize().unwrap(),
        );
        let excluded = captured(
            &project,
            "cccccccc-0000-4000-8000-000000000023",
            &project.root,
        );
        project
            .excluded
            .insert(key(&excluded.source, &excluded.session_id));
        save(&project).unwrap();
        let second = tempfile::tempdir().unwrap();
        let second = super::tests::project(second.path());
        save(&second).unwrap();
        let elsewhere = captured(
            &second,
            "dddddddd-0000-4000-8000-000000000024",
            &second.root,
        );
        let resumed = captured(
            &project,
            "eeeeeeee-0000-4000-8000-000000000025",
            &outside.path().canonicalize().unwrap(),
        );
        let store = Store::at(config::store_root().unwrap());
        let mut successor = resumed.clone();
        successor.session_id = "ffffffff-0000-4000-8000-000000000026".into();
        link::write(&store, &successor).unwrap();
        let mut superseded = resumed.clone();
        superseded.superseded_by = Some(successor.instance());
        link::write(&store, &superseded).unwrap();
        let branch = |claim: &link::Link| claim.branch.clone().unwrap();
        assert_eq!(withheld("alice/app", &branch(&inside)).unwrap(), None);
        assert_eq!(withheld("alice/app", &branch(&elsewhere)).unwrap(), None);
        assert_eq!(
            withheld("alice/app", &branch(&resumed)).unwrap(),
            Some(Withholding::StartedOutside)
        );
        assert_eq!(
            withheld("alice/app", &branch(&strayed)).unwrap(),
            Some(Withholding::StartedOutside)
        );
        assert_eq!(
            withheld("alice/app", &branch(&excluded)).unwrap(),
            Some(Withholding::Excluded)
        );
        let repo = Repo::init(&work.path().join("repo")).unwrap();
        assert!(blocks_auto_push(&repo, "alice/app", &branch(&strayed)).unwrap());
    }

    /// Existing claims are judged without a command: a claim captured from a conversation started
    /// outside the folder is dropped and stays withheld once its link is gone, the drop is said
    /// once, and sound claims are kept, including one of another folder bound to the repository. `project drop` releases a claim the user names the same
    /// way and excludes its session, so the project does not capture it again.
    #[test]
    fn existing_claims_are_rechecked_once_and_can_be_dropped() {
        if !isolated("existing_claims_are_rechecked_once_and_can_be_dropped") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let project = project(work.path());
        save(&project).unwrap();
        let store = Store::at(config::store_root().unwrap());
        let inside = captured(
            &project,
            "aaaaaaaa-0000-4000-8000-000000000031",
            &project.root,
        );
        let strayed = captured(
            &project,
            "bbbbbbbb-0000-4000-8000-000000000032",
            &outside.path().canonicalize().unwrap(),
        );
        let second = tempfile::tempdir().unwrap();
        let second = super::tests::project(second.path());
        save(&second).unwrap();
        let elsewhere = captured(
            &second,
            "cccccccc-0000-4000-8000-000000000033",
            &second.root,
        );
        recheck_claims(false);
        assert!(link::get(&store, &elsewhere.source, &elsewhere.session_id).is_some());
        assert!(link::get(&store, &strayed.source, &strayed.session_id).is_none());
        assert!(link::get(&store, &inside.source, &inside.session_id).is_some());
        assert_eq!(
            withheld("alice/app", strayed.branch.as_deref().unwrap()).unwrap(),
            Some(Withholding::StartedOutside)
        );
        let unannounced = || {
            withheld_records()
                .unwrap()
                .iter()
                .filter(|r| !r.announced)
                .count()
        };
        assert_eq!(unannounced(), 1);
        recheck_claims(true);
        assert_eq!(unannounced(), 0);

        let target = format!("alice/app@{}", inside.branch.as_deref().unwrap());
        run(Args {
            action: Action::Drop {
                target: Some(target),
            },
        })
        .unwrap();
        assert!(link::get(&store, &inside.source, &inside.session_id).is_none());
        assert_eq!(
            withheld("alice/app", inside.branch.as_deref().unwrap()).unwrap(),
            Some(Withholding::Dropped)
        );
        assert!(
            read(&project.root)
                .unwrap()
                .unwrap()
                .excluded
                .contains(&key(&inside.source, &inside.session_id))
        );
    }
}
