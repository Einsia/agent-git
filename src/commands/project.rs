//! Explicit project enrollment composes discovery, import and registered session publication.

use super::CmdResult;
use crate::{
    ExitCode, adapter,
    domain::{link, meta, repo::Repo, store::Store, workspace},
    hub::identity::{self, RemoteIdentity},
    infra::{config, credentials},
    ui,
};
use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, Write},
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
    /// List every enrolled or bound folder with its state and recent conversations, offline.
    List,
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
        "turns on automatic push on this computer: every managed conversation saved to {repository} (in any folder) uploads after each turn. Teammates' settings and Hub settings are unchanged. Unbinding this folder restores the previous setting once no other bound folder holds automatic push on"
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

/// Turns on automatic push for this computer's local repository, recording the override from
/// before any folder turned it on: this folder's own earlier record, another bound folder's, or the
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
    /// Whether every member of that organization reads its private repositories, as the Hub
    /// last said; `None` until it has, or for a Hub without the base permission.
    #[serde(default)]
    members_read: Option<bool>,
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

/// Whether every member of `owner` reads its private repositories; `None` when the owner is no
/// organization the caller belongs to, or the Hub does not say.
fn members_read(client: &crate::hub::Client, owner: &str) -> Option<bool> {
    let org = client.get_org(owner).ok()?;
    org.base_permission.is_some().then(|| org.members_read())
}

pub(crate) fn root(path: &Path) -> Result<PathBuf> {
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
    /// The project repository holds the session's claim on this branch.
    Claim(String),
    /// No repository claims the session yet; importing it names its branch.
    New,
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
    Ok(Placement::New)
}

/// The branch a newly imported session gets: its opening request in a few words, then a short
/// hash of its identity so two sessions that open alike still get different branches. A request
/// without Latin letters falls back to the runtime and the date.
fn new_branch(project: &Project, candidate: &Candidate) -> String {
    let identity = hash(&key(&candidate.runtime, &candidate.session_id));
    let gist = adapter::get(&candidate.runtime).ok().and_then(|adapter| {
        let path = adapter.resolve(&candidate.session_id, Some(&candidate.cwd))?;
        adapter.parse_at(&path).ok()?.gist(200)
    });
    let words = gist
        .as_deref()
        .map(|gist| super::front::slug(&without_markup(gist), 40))
        .unwrap_or_default();
    let runtime = match candidate.runtime.as_str() {
        "claude-code" => "claude",
        other => other,
    };
    let stem = if words.is_empty() {
        format!("{runtime}-{}", chrono::Local::now().format("%m%d"))
    } else if crate::domain::repo::valid_branch_name(&words).is_err() || words == "main" {
        format!("{runtime}-{words}")
    } else {
        words
    };
    let taken = |branch: &str| {
        checkout(project).is_ok_and(|repo| repo.has_ref(&format!("refs/heads/{branch}")))
    };
    let short = format!("{stem}-{}", &identity[..4]);
    if taken(&short) {
        format!("{stem}-{identity}")
    } else {
        short
    }
}

/// A request's text without the markup a runtime or a paste wraps around it, such as
/// `<pasted_content id="…">` or `[Pasted text #1 +20 lines]`: a branch is named by what the person
/// asked, and its name ends up in links people paste elsewhere.
fn without_markup(text: &str) -> String {
    static MARKUP: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    MARKUP
        .get_or_init(|| {
            regex::Regex::new(
                r"</?[A-Za-z][\w:-]*(?:\s[^<>]*)?/?>|\[(?:Pasted text|Image) #\d+[^\]]*\]",
            )
            .unwrap()
        })
        .replace_all(text, " ")
        .into_owned()
}

/// The branch a project opens for a session it captures.
fn capture_branch(runtime: &str, id: &str) -> String {
    format!("project-{runtime}-{}", hash(&key(runtime, id)))
}

/// Resolve under the project lock so a moved claim cannot authorize another repository. The flag
/// says whether the capture opens the branch.
fn publication(project: &Project, candidate: &Candidate) -> Result<(String, bool)> {
    match placement(project, candidate)? {
        Placement::Claim(branch) => Ok((branch, false)),
        Placement::New => Ok((new_branch(project, candidate), true)),
        Placement::Foreign(source) => anyhow::bail!(
            "session is claimed by {source}; cross-repository publication requires preserved source authority; publish from its source repository"
        ),
    }
}

/// Imports and publishes one session; returns its branch and the head the push started from.
fn sync_one(project: &Project, candidate: &Candidate) -> Result<(String, Option<String>)> {
    verify(project)?;
    let (branch, opens) = publication(project, candidate)?;
    let target = format!("{}@{branch}", project.repository);
    // A capture whose first import settled no turn is marked by the import that settles one.
    let marks = opens
        || captured_records()?.iter().any(|r| {
            r.repository == project.repository
                && r.branch == branch
                && r.runtime == candidate.runtime
                && r.session == candidate.session_id
        });
    let mut import = vec![
        "import".into(),
        candidate.session_id.clone(),
        "--from".into(),
        candidate.runtime.clone(),
        "--into".into(),
        target.clone(),
        "--independent".into(),
    ];
    if marks {
        import.push("--project-capture".into());
    }
    child(&import, &candidate.cwd, true)?;
    // Only a branch the import created is listed: the list must never name a branch the user
    // creates later under the same name. The import that settles the branch's first turn records
    // the same fact in its metadata.
    if opens {
        record_capture(Captured {
            repository: project.repository.clone(),
            branch: branch.clone(),
            runtime: candidate.runtime.clone(),
            session: candidate.session_id.clone(),
        })?;
    }
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
    let repo = verify(project)?;
    // Read before the push: a commit saved while it runs may not be in it, and is not counted as
    // uploaded.
    let head = repo.git_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}^{{commit}}"),
    ]);
    let mut args = vec!["push".into(), target];
    if !project.auto_upload {
        args.push("--yes".into());
    }
    child(&args, &candidate.cwd, project.auto_upload)?;
    // A publication does not move the local view of the Hub's branches; fetching does, so local
    // status and links can tell that this branch is on the Hub.
    let _ = child(
        &["fetch".into(), project.repository.clone()],
        &candidate.cwd,
        false,
    );
    Ok((branch, head))
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
            Ok((branch, head)) => {
                serde_json::json!({"branch": branch, "status": "pushed", "head": head})
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
        Action::List => list(),
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
    let org = owner_is_org(&client, &owner, &project.account);
    let audience = Audience {
        visibility: Some(remote.visibility.clone()),
        org,
        members_read: org
            .unwrap_or(false)
            .then(|| members_read(&client, &owner))
            .flatten(),
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

/// The local files that route a folder to its repository: its project policy, upload records and
/// directory binding, as they are now.
pub(crate) struct Routing {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// The repositories the policy and the directory binding named before the release.
    policy: Option<String>,
    binding: Option<String>,
}

impl Routing {
    fn paths(root: &Path) -> Result<Vec<PathBuf>> {
        let mut paths: Vec<PathBuf> = ["json", "caught", "catch-up"]
            .iter()
            .map(|extension| Ok(state_path(root)?.with_extension(extension)))
            .collect::<Result<_>>()?;
        paths.push(workspace::path_for(root)?);
        Ok(paths)
    }

    /// Puts every file back as it was, removing those that did not exist. A move to `moved_to`
    /// leaves each file absent, naming `moved_to`, or as it was, wherever the move stopped; a file
    /// naming any other repository was written by another command since, and that setup is kept.
    pub(crate) fn restore(self, root: &Path, moved_to: &str) -> Result<()> {
        let _guard = lock(root)?;
        let policy = read(root)?.map(|project| project.repository);
        let binding = workspace::read(root).map(|binding| binding.repo);
        let ours = |now: Option<&str>, before: Option<&str>| {
            now.is_none_or(|repo| repo == moved_to || Some(repo) == before)
        };
        ensure!(
            ours(policy.as_deref(), self.policy.as_deref())
                && ours(binding.as_deref(), self.binding.as_deref()),
            "the folder was set up again by another command meanwhile; its new setup was kept"
        );
        for (path, bytes) in self.files {
            match bytes {
                Some(bytes) => {
                    let parent = path.parent().context("routing file has no directory")?;
                    config::create_state_dir(parent)?;
                    let mut file = tempfile::NamedTempFile::new_in(parent)?;
                    file.write_all(&bytes)?;
                    file.as_file().sync_all()?;
                    file.persist(&path).map_err(|e| e.error)?;
                }
                None => match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }
        Ok(())
    }
}

/// Serializes everything that sets up `root` from the front door, from choosing its destination
/// to enrolling it or putting the previous setup back. It is not the project lock, which the
/// enrolling child process takes itself.
pub(crate) fn setup_lock(root: &Path) -> Result<fs::File> {
    config::create_state_dir(&home()?)?;
    let path = state_path(root)?.with_extension("setup");
    let file = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    crate::infra::local_state::lock_patiently(&file, &path, "the folder setup lock", true)?;
    Ok(file)
}

/// Forgets the project policy at `root` so the folder can be bound to another repository, and
/// returns what it was so a move that fails can put it back. Sessions already saved stay in the
/// repository that holds them.
pub(crate) fn unbind_for_move(root: &Path) -> Result<Routing> {
    let _guard = lock(root)?;
    let mut files = Vec::new();
    for path in Routing::paths(root)? {
        let bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        files.push((path, bytes));
    }
    let policy = read(root)?.map(|project| project.repository);
    let binding = workspace::read(root).map(|binding| binding.repo);
    for (path, _) in files.iter().take(3) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Routing {
        files,
        policy,
        binding,
    })
}

/// The repository of the project policy that covers `path`, paused or not.
pub(crate) fn policy_repository(path: &Path) -> Option<String> {
    let project = nearest(path).ok().flatten()?;
    includes(&project, path).ok()?.then_some(project.repository)
}

/// Whether the project policy covering `path` uploads automatically, without network access.
pub(crate) fn uploads_automatically(path: &Path) -> Option<bool> {
    let project = nearest(path).ok().flatten()?;
    includes(&project, path)
        .ok()?
        .then_some(project.enabled && project.auto_upload)
}

/// The last automatic upload's error, when it failed.
pub(crate) fn last_failure(path: &Path) -> Option<String> {
    let project = nearest(path).ok().flatten()?;
    let last = project.last_result?;
    (last["ok"] == false).then(|| last["error"].as_str().unwrap_or("unknown error").to_owned())
}

/// Sessions under `root` that the policy left out when it was bound without earlier history and
/// that no repository holds yet.
pub(crate) fn excluded_count(root: &Path) -> usize {
    let Some(project) = read(root).ok().flatten() else {
        return 0;
    };
    let store = Store::open().ok().flatten();
    project
        .excluded
        .iter()
        .filter(|session_key| {
            let Some((runtime, id)) = session_key.split_once(':') else {
                return true;
            };
            store
                .as_ref()
                .and_then(|store| link::get(store, runtime, id))
                .is_none_or(|link| link.branch.is_none())
        })
        .count()
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
    let branch = match placement(&project, &candidate) {
        Ok(Placement::Claim(branch)) => Some(branch),
        Ok(Placement::New) => None,
        _ => return Ok(false),
    };
    if !project.enabled
        || !project.auto_upload
        || project
            .excluded
            .contains(&key(&candidate.runtime, &candidate.session_id))
    {
        // Pausing the project stops passive capture only. A session claimed on another branch
        // (`agit run`, `resume --as`, an import) is settled from its link; a paused project
        // still blocks its automatic push. A session no project has claimed has nothing to settle.
        return Ok(branch.is_none_or(|branch| branch == capture_branch(runtime, session)));
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
        if take_catch_up(root)?
            && let Some(project) = read(root)?
        {
            let caught = caught(root);
            for candidate in candidates(&project)? {
                if behind(&project, &candidate, &caught) {
                    enqueue(root, &candidate)?;
                }
            }
        }
        while let Some(candidate) = take_pending(root)? {
            let Some(project) = read(root)? else {
                return Ok(());
            };
            // A failure is recorded in the project's last result; the remaining sessions go on.
            let _ = capture_candidate(project, candidate);
        }
        drop(worker);
        if markers(root)?.is_empty() && !catch_up_requested(root)? {
            return Ok(());
        }
    }
}

fn capture_candidate(mut project: Project, candidate: Candidate) -> Result<bool> {
    // Automatic capture leaves foreign claims to their source repository's settlement.
    if !matches!(
        placement(&project, &candidate),
        Ok(Placement::Claim(_) | Placement::New)
    ) {
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
    let modified = transcript_modified(&candidate);
    let result = sync_one(&project, &candidate);
    if result.is_ok()
        && let Some(modified) = modified
    {
        let _ = record_caught(&project.root, &candidate, modified);
    }
    project.last_result = Some(serde_json::json!({
        "at": chrono::Utc::now(), "session_id": candidate.session_id, "ok": result.is_ok(),
        "error": result.as_ref().err().map(|e| format!("{e:#}")),
        "head": result.as_ref().ok().and_then(|(_, head)| head.as_deref()),
    }));
    save(&project)?;
    result?;
    Ok(true)
}

/// Queues a catch-up of the project covering `path`: every session of it with turns the Hub does
/// not have yet, which a Stop hook that did not run (an untrusted hook, a crash, no network) left
/// behind. The worker does the work; this returns at once.
pub(crate) fn catch_up(path: &Path) -> Result<bool> {
    let Some(project) = nearest(path)? else {
        return Ok(false);
    };
    if !project.enabled || !project.auto_upload || !includes(&project, path)? {
        return Ok(false);
    }
    let marker = state_path(&project.root)?.with_extension("catch-up");
    config::create_state_dir(&home()?)?;
    fs::write(&marker, b"")?;
    if capture_worker(&project.root)?.is_some() {
        let _ = spawn_capture(&project.root);
    }
    Ok(true)
}

fn catch_up_requested(root: &Path) -> Result<bool> {
    Ok(state_path(root)?.with_extension("catch-up").exists())
}

fn take_catch_up(root: &Path) -> Result<bool> {
    match fs::remove_file(state_path(root)?.with_extension("catch-up")) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

// ── publication withheld by project policy ─────────────────────────────

/// Branches withheld from publication, kept apart from the policies so that a dropped claim stays
/// withheld after its link, and even its project, is gone.
const WITHHELD: &str = "withheld";
/// Branches this machine's projects opened for captured sessions.
const CAPTURED: &str = "captured";
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

/// Whether a file in the projects directory is named as a policy is.
fn is_policy(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.len() == 24 && stem.bytes().all(|b| b.is_ascii_hexdigit()))
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
        if is_policy(&path) {
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

/// A branch one of this machine's projects opened for a captured session. The branch's own
/// metadata says the same and travels with it; this list answers without reading the branch.
#[derive(Serialize, Deserialize)]
struct Captured {
    repository: String,
    branch: String,
    runtime: String,
    session: String,
}

fn captured_records() -> Result<Vec<Captured>> {
    match fs::read(home()?.join(CAPTURED)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid captured branch list"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn record_capture(capture: Captured) -> Result<()> {
    update_list(
        CAPTURED,
        "the captured branch lock",
        captured_records,
        |records| {
            records.retain(|r| r.repository != capture.repository || r.branch != capture.branch);
            records.push(capture);
        },
    )
}

/// The conversation whose capture opened `slug@branch`: from this machine's list, else from the
/// branch's own metadata, so a lost list or another machine's capture still names it.
fn capture_origin(slug: &str, branch: &str) -> Result<Option<meta::CaptureOrigin>> {
    if let Some(record) = captured_records()?
        .into_iter()
        .find(|r| r.repository == slug && r.branch == branch)
    {
        return Ok(Some(meta::CaptureOrigin {
            runtime: record.runtime,
            session: record.session,
        }));
    }
    let (owner, agent) = super::parse_slug(slug)?;
    let Some(repo) = Repo::open(config::repo_dir(&owner, &agent)?) else {
        return Ok(None);
    };
    let tip = format!("refs/heads/{branch}");
    if !repo.has_ref(&tip) {
        return Ok(None);
    }
    Ok(meta::read_at_ref_result(&repo, &tip)?.and_then(|m| m.capture))
}

/// Read-modify-write of the withheld list under its own lock.
fn update_withheld<T>(change: impl FnOnce(&mut Vec<Withheld>) -> T) -> Result<T> {
    update_list(
        WITHHELD,
        "the withheld branch lock",
        withheld_records,
        change,
    )
}

/// Read-modify-write of the list in `name` under its own lock.
fn update_list<R: Serialize, T>(
    name: &str,
    what: &str,
    read: fn() -> Result<Vec<R>>,
    change: impl FnOnce(&mut Vec<R>) -> T,
) -> Result<T> {
    let home = home()?;
    config::create_state_dir(&home)?;
    let lock_path = home.join(name).with_extension("lock");
    let guard = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    crate::infra::local_state::lock_patiently(&guard, &lock_path, what, true)?;
    let mut records = read()?;
    let result = change(&mut records);
    let mut file = tempfile::NamedTempFile::new_in(&home)?;
    file.write_all(&serde_json::to_vec_pretty(&records)?)?;
    file.as_file().sync_all()?;
    file.persist(home.join(name)).map_err(|e| e.error)?;
    Ok(result)
}

/// Where the conversation behind a claim started; `None` when its transcript or its first
/// recorded directory cannot be found.
fn start(claim: &link::Link) -> Option<PathBuf> {
    let transcript = claim.resolve()?;
    let start = crate::tui::screens::selector::recorded_cwd(&transcript)?;
    Path::new(&start).canonicalize().ok()
}

/// Whether a branch carries the name a project capture gave a session before captures were
/// named by their request. A resumed conversation keeps recording into that branch under a new
/// session id, so the name, not the current claim, says that the branch began as a capture.
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
/// because it is outside another. A capture is known by `origin`, the conversation recorded as
/// having opened it, or by a capture's legacy name; a branch the user named is theirs and only an
/// exclusion withholds it.
fn judge(
    projects: &[Project],
    branch: &str,
    claims: &[link::Link],
    origin: Option<&meta::CaptureOrigin>,
) -> Option<Withholding> {
    let excluded = |session: String| projects.iter().any(|p| p.excluded.contains(&session));
    if claims
        .iter()
        .any(|claim| excluded(key(&claim.source, &claim.session_id)))
        || origin.is_some_and(|o| excluded(key(&o.runtime, &o.session)))
    {
        return Some(Withholding::Excluded);
    }
    let opener = match origin {
        Some(origin) => claims
            .iter()
            .find(|claim| claim.source == origin.runtime && claim.session_id == origin.session),
        None if is_capture_branch(branch) => claims
            .iter()
            .find(|claim| capture_branch(&claim.source, &claim.session_id) == branch),
        None => return None,
    };
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
    Ok(
        withheld_all(slug, std::slice::from_ref(&branch.to_owned()))?
            .pop()
            .flatten(),
    )
}

/// [`withheld`] for each of `branches`, in order. The records, the project policies and the
/// store's links are read once for all of them: read per branch, a push of many branches pays a
/// full scan of the session store for every one.
pub(crate) fn withheld_all(slug: &str, branches: &[String]) -> Result<Vec<Option<Withholding>>> {
    let records = withheld_records()?;
    let projects: Vec<_> = policies()?
        .into_iter()
        .filter(|p| p.repository == slug)
        .collect();
    let (owner, agent) = super::parse_slug(slug)?;
    let mut links = None;
    let mut verdicts = Vec::with_capacity(branches.len());
    for branch in branches {
        if let Some(record) = records
            .iter()
            .find(|r| r.repository == slug && &r.branch == branch)
        {
            verdicts.push(Some(record.reason));
            continue;
        }
        if projects.is_empty() {
            verdicts.push(None);
            continue;
        }
        // An excluded session's capture branch stays withheld after its claim is gone.
        let excluded_branch = projects.iter().flat_map(|p| &p.excluded).any(|session| {
            session
                .split_once(':')
                .is_some_and(|(runtime, id)| capture_branch(runtime, id) == *branch)
        });
        if excluded_branch {
            verdicts.push(Some(Withholding::Excluded));
            continue;
        }
        let links = match &links {
            Some(links) => links,
            None => links.insert(link::list(&Store::at(config::store_root()?))),
        };
        let claims = recording_into(links, &owner, &agent, branch);
        let origin = capture_origin(slug, branch)?;
        verdicts.push(judge(&projects, branch, &claims, origin.as_ref()));
    }
    Ok(verdicts)
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
        // A branch whose metadata cannot be read is judged by its name here and so is not dropped;
        // publication reads the metadata again and refuses on the error.
        let origin = capture_origin(&slug, &branch).ok().flatten();
        match judge(&bound, &branch, &claims, origin.as_ref()) {
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
    /// Whether every member of that organization reads it, when known.
    pub(crate) members_read: Option<bool>,
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
            (remote.agent_id == project.identity.agent_id).then(|| {
                let org = recorded
                    .org
                    .or_else(|| owner_is_org(&client, &owner, &project.account));
                // Owners change the base permission, so it is asked for every time.
                let members_read = if org == Some(true) {
                    members_read(&client, &owner).or(recorded.members_read)
                } else {
                    None
                };
                Audience {
                    visibility: Some(remote.visibility),
                    org,
                    members_read,
                }
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
        members_read: audience.members_read,
    })
}

fn caught_path(root: &Path) -> Result<PathBuf> {
    Ok(state_path(root)?.with_extension("caught"))
}

/// The transcript modification time each session had when it was last uploaded, so a catch-up
/// reads only transcripts that changed since.
fn caught(root: &Path) -> std::collections::BTreeMap<String, u64> {
    caught_path(root)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record_caught(root: &Path, candidate: &Candidate, modified: u64) -> Result<()> {
    let mut map = caught(root);
    map.insert(key(&candidate.runtime, &candidate.session_id), modified);
    let mut file = tempfile::NamedTempFile::new_in(home()?)?;
    file.write_all(&serde_json::to_vec(&map)?)?;
    file.persist(caught_path(root)?).map_err(|e| e.error)?;
    Ok(())
}

fn transcript_modified(candidate: &Candidate) -> Option<u64> {
    let adapter = adapter::get(&candidate.runtime).ok()?;
    let path = adapter.resolve(&candidate.session_id, Some(&candidate.cwd))?;
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

/// Whether the Hub lacks some of this session: it was never imported, its branch differs from
/// what the Hub was last seen to have, or its transcript changed after its last upload.
fn behind(
    project: &Project,
    candidate: &Candidate,
    caught: &std::collections::BTreeMap<String, u64>,
) -> bool {
    let session_key = key(&candidate.runtime, &candidate.session_id);
    if project.excluded.contains(&session_key) {
        return false;
    }
    match placement(project, candidate) {
        // A session whose first turn has not finished, such as the one whose start asked for this
        // catch-up, has nothing to upload; its Stop uploads it.
        Ok(Placement::New) => adapter::get(&candidate.runtime)
            .ok()
            .and_then(|adapter| {
                let path = adapter.resolve(&candidate.session_id, Some(&candidate.cwd))?;
                adapter.parse_at(&path).ok()
            })
            .is_some_and(|session| crate::domain::turn::completed_count(&session) > 0),
        Ok(Placement::Claim(branch)) => {
            let Ok(repo) = checkout(project) else {
                return false;
            };
            let tip =
                |reference: String| repo.git_opt(&["rev-parse", "--verify", "--quiet", &reference]);
            tip(format!("refs/heads/{branch}")) != tip(format!("refs/remotes/origin/{branch}"))
                || transcript_modified(candidate) != caught.get(&session_key).copied()
        }
        _ => false,
    }
}

pub(crate) fn registration_scope(cwd: &Path) -> bool {
    nearest(cwd)
        .ok()
        .flatten()
        .is_some_and(|p| p.enabled && p.auto_upload && includes(&p, cwd).unwrap_or(false))
}

pub(crate) fn blocks_auto_push(repo: &Repo, slug: &str, branch: &str) -> Result<bool> {
    if withheld(slug, branch)?.is_some() || refused_as_withdrawn(repo, branch)? {
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

/// Whether the Hub this checkout publishes to refused `branch` as withdrawn. The push that met
/// the refusal said so once; later turns leave the branch out quietly, as an excluded session is
/// left out, until a push that names it asks the Hub again.
fn refused_as_withdrawn(repo: &Repo, branch: &str) -> Result<bool> {
    let Some(identity) = crate::hub::identity::read(repo)? else {
        return Ok(false);
    };
    Ok(crate::domain::privacy_git::withheld_refs(repo, &identity)?
        .contains(&format!("refs/heads/{branch}")))
}

/// How many of a folder's newest conversations `project list` describes.
const LISTED_SESSIONS: usize = 5;
/// How much of either end of a transcript is read to name it, so naming a conversation costs the
/// same however long it grows.
const TITLE_SCAN_BYTES: u64 = 256 * 1024;

/// A folder `project list` reports: one with a project policy, or a directory binding without one.
struct Listed {
    root: PathBuf,
    repository: String,
    hub: String,
    project: Option<Project>,
    sessions: Vec<adapter::SessionRef>,
}

/// Every existing folder with a policy or a binding; a policy wins over the binding it writes.
/// The flag is false when a policy or binding could not be read, so a short list is told apart
/// from a partial one.
fn listed() -> Result<(Vec<Listed>, bool)> {
    let mut folders = BTreeMap::new();
    let mut complete = true;
    let entries = match fs::read_dir(home()?) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    for entry in entries.into_iter().flatten() {
        let path = entry?.path();
        if !is_policy(&path) {
            continue;
        }
        let project = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Project>(&bytes).ok());
        // A policy belongs to a directory only when it is stored at that directory's own path.
        match project {
            Some(project) if state_path(&project.root)? == path => {
                if project.root.is_dir() {
                    folders.insert(
                        project.root.clone(),
                        Listed {
                            root: project.root.clone(),
                            repository: project.repository.clone(),
                            hub: project.identity.hub.clone(),
                            project: Some(project),
                            sessions: Vec::new(),
                        },
                    );
                }
            }
            _ => complete = false,
        }
    }
    let (bindings, bindings_complete) = workspace::all()?;
    let hub = identity::normalize_hub(&config::hub_url()).unwrap_or_else(|_| config::hub_url());
    for binding in bindings {
        let Ok(root) = Path::new(&binding.dir).canonicalize() else {
            continue;
        };
        if root.is_dir() {
            folders.entry(root.clone()).or_insert(Listed {
                root,
                repository: binding.repo,
                hub: hub.clone(),
                project: None,
                sessions: Vec::new(),
            });
        }
    }
    Ok((
        folders.into_values().collect(),
        complete && bindings_complete,
    ))
}

/// A conversation belongs to the deepest listed folder that holds its directory, as a nested
/// binding takes its directory out of the folder around it. Returns false when a runtime's
/// sessions could not be listed.
fn assign_sessions(folders: &mut [Listed]) -> Result<bool> {
    let mut complete = true;
    for runtime in ["claude-code", "codex"] {
        let Ok(sessions) = adapter::get(runtime)?.all_session_choices() else {
            complete = false;
            continue;
        };
        for session in sessions {
            let cwd = session
                .cwd
                .clone()
                .or_else(|| crate::tui::screens::selector::recorded_cwd(&session.path));
            let Some(cwd) = cwd.and_then(|p| PathBuf::from(p).canonicalize().ok()) else {
                continue;
            };
            if let Some(folder) = folders
                .iter_mut()
                .filter(|folder| cwd.starts_with(&folder.root))
                .max_by_key(|folder| folder.root.components().count())
            {
                folder.sessions.push(session);
            }
        }
    }
    for folder in folders {
        folder
            .sessions
            .sort_by_key(|session| std::cmp::Reverse(session.mtime));
    }
    Ok(complete)
}

/// Whole records from the start or the end of a transcript, at most [`TITLE_SCAN_BYTES`] of it.
fn transcript_end(path: &Path, start: bool) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let offset = if start {
        0
    } else {
        length.saturating_sub(TITLE_SCAN_BYTES)
    };
    file.seek(std::io::SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(TITLE_SCAN_BYTES).read_to_end(&mut bytes).ok()?;
    // A cut record is dropped: the head keeps what ends in a newline, the tail what follows one.
    let newline = |byte: &u8| *byte == b'\n';
    if start {
        bytes.truncate(bytes.iter().rposition(newline).map_or(0, |end| end + 1));
    } else if offset > 0 {
        bytes.drain(
            ..bytes
                .iter()
                .position(newline)
                .map_or(bytes.len(), |at| at + 1),
        );
    }
    String::from_utf8(bytes).ok()
}

/// The runtime's own name for a conversation, else its opening request. `None` when neither the
/// runtime's index nor the bounded ends of the transcript name it.
fn title(runtime: &str, session: &adapter::SessionRef) -> Option<String> {
    session
        .title
        .clone()
        .or_else(|| {
            let tail = transcript_end(&session.path, false)?;
            adapter::native_title::native_title(runtime, &session.id, &tail, None)
        })
        .or_else(|| session.gist.clone())
        .or_else(|| {
            let head = transcript_end(&session.path, true)?;
            adapter::get(runtime)
                .ok()?
                .parse(&head)
                .ok()?
                .gist(adapter::preview::SESSION_PREVIEW_CHARS)
        })
}

/// A failed child command records its JSON diagnostics after a `<command> failed: ` prefix;
/// the reason a listing reports is their messages.
fn readable(error: &str) -> String {
    if let Some((command, diagnostics)) = error.split_once(": [")
        && let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&format!("[{diagnostics}"))
    {
        let errors: Vec<_> = rows
            .iter()
            .filter(|row| row["level"] == "error")
            .filter_map(|row| row["message"].as_str())
            .collect();
        if !errors.is_empty() {
            return format!("{command}: {}", errors.join("; "));
        }
    }
    error.to_owned()
}

/// A capture result and a sync result in one shape. `uploaded` maps each session the attempt
/// published to the head its push started from; a result that recorded no head proves nothing.
struct LastUpload {
    at: Option<chrono::DateTime<chrono::FixedOffset>>,
    ok: bool,
    error: Option<String>,
    session_id: Option<String>,
    uploaded: BTreeMap<String, String>,
}

fn last_upload(result: &serde_json::Value) -> LastUpload {
    let at = result["at"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
    let reason = |error: &serde_json::Value| readable(error.as_str().unwrap_or("upload failed"));
    if let Some(rows) = result["sessions"].as_array() {
        let pushed = |row: &&serde_json::Value| row["outcome"]["status"] == "pushed";
        let failed = rows.iter().find(|row| !pushed(row));
        return LastUpload {
            at,
            ok: failed.is_none(),
            error: failed.map(|row| reason(&row["outcome"]["error"])),
            session_id: failed.and_then(|row| row["session_id"].as_str().map(str::to_owned)),
            uploaded: rows
                .iter()
                .filter(pushed)
                .filter_map(|row| {
                    let head = row["outcome"]["head"].as_str()?;
                    Some((row["session_id"].as_str()?.to_owned(), head.to_owned()))
                })
                .collect(),
        };
    }
    let ok = result["ok"] == true;
    let session_id = result["session_id"].as_str().map(str::to_owned);
    LastUpload {
        at,
        ok,
        error: (!ok).then(|| reason(&result["error"])),
        uploaded: session_id
            .iter()
            .filter(|_| ok)
            .zip(result["head"].as_str())
            .map(|(id, head)| (id.clone(), head.to_owned()))
            .collect(),
        session_id,
    }
}

/// A repository's local session heads with their commit times, and the heads it last saw on the
/// Hub.
#[derive(Default)]
struct Heads {
    local: BTreeMap<String, (String, Option<chrono::DateTime<chrono::FixedOffset>>)>,
    remote: BTreeMap<String, String>,
}

fn heads(repo: &Repo) -> Heads {
    let mut heads = Heads::default();
    let Some(output) = repo.git_opt(&[
        "for-each-ref",
        "--format=%(refname)%00%(objectname)%00%(committerdate:iso-strict)",
        "refs/heads",
        "refs/remotes/origin",
    ]) else {
        return heads;
    };
    for line in output.lines() {
        let mut fields = line.split('\0');
        let (Some(name), Some(commit), Some(date)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            let date = chrono::DateTime::parse_from_rfc3339(date).ok();
            heads.local.insert(branch.into(), (commit.into(), date));
        } else if let Some(branch) = name.strip_prefix("refs/remotes/origin/") {
            heads.remote.insert(branch.into(), commit.into());
        }
    }
    heads
}

/// Local repositories by `owner/repo`, opened and read once per listing.
type Checkouts = BTreeMap<String, Option<(Repo, Heads)>>;

/// Where one conversation stands. A saved one is `uploaded` when the Hub has its local head: the
/// last upload pushed that very commit, or this checkout last saw it there. A commit's time is
/// no proof, since it has whole-second precision and a session can be saved again while a batch
/// is still uploading. `waiting` when the Hub has an older head; `not_uploaded` when it has none
/// this checkout knows of.
struct Standing {
    status: &'static str,
    /// The `(owner, name, branch)` it is saved in.
    saved: Option<(String, String, String)>,
    saved_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    on_hub: bool,
}

fn standing(
    session: &adapter::SessionRef,
    store: Option<&Store>,
    checkouts: &mut Checkouts,
    last: Option<&LastUpload>,
) -> Standing {
    let saved = store
        .and_then(|store| link::get(store, session.runtime, &session.id))
        .and_then(|link| Some((link.owner?, link.agent?, link.branch?)));
    let Some((owner, name, branch)) = &saved else {
        return Standing {
            status: "not_saved",
            saved: None,
            saved_at: None,
            on_hub: false,
        };
    };
    let checkout = checkouts
        .entry(format!("{owner}/{name}"))
        .or_insert_with(|| {
            let repo = Repo::open(config::repo_dir(owner, name).ok()?)?;
            let heads = heads(&repo);
            Some((repo, heads))
        });
    let local = checkout
        .as_ref()
        .and_then(|(_, heads)| heads.local.get(branch));
    let remote = checkout
        .as_ref()
        .and_then(|(_, heads)| heads.remote.get(branch));
    let saved_at = local.and_then(|(_, at)| *at);
    let head = local.map(|(commit, _)| commit);
    let pushed = last.is_some_and(|last| head.is_some() && last.uploaded.get(&session.id) == head);
    let status = if pushed || (remote.is_some() && remote == head) {
        "uploaded"
    } else if remote.is_some() {
        "waiting"
    } else {
        "not_uploaded"
    };
    Standing {
        status,
        on_hub: remote.is_some(),
        saved_at,
        saved,
    }
}

fn session_json(
    session: &adapter::SessionRef,
    store: Option<&Store>,
    checkouts: &mut Checkouts,
    last: Option<&LastUpload>,
    hub: &str,
) -> serde_json::Value {
    let standing = standing(session, store, checkouts, last);
    let url = standing
        .saved
        .as_ref()
        .filter(|_| standing.on_hub)
        .and_then(|(owner, name, branch)| {
            let (repo, _) = checkouts.get(&format!("{owner}/{name}"))?.as_ref()?;
            super::repo::published_session_path(repo, owner, name, branch)
        })
        .map(|path| {
            super::with_sharer(
                format!("{}{path}", hub.trim_end_matches('/')),
                super::link_sharer(hub).as_deref(),
            )
        });
    serde_json::json!({
        "title": title(session.runtime, session),
        "runtime": session.runtime,
        "session_id": session.id,
        "last_active": chrono::DateTime::<chrono::Utc>::from(session.mtime),
        "status": standing.status,
        "repository": standing.saved.as_ref().map(|(owner, name, _)| format!("{owner}/{name}")),
        "saved_at": standing.saved_at,
        "url": url,
    })
}

fn folder_json(
    folder: &Listed,
    store: Option<&Store>,
    checkouts: &mut Checkouts,
) -> serde_json::Value {
    let slug = super::parse_slug(&folder.repository).ok();
    let auto_push = slug
        .as_ref()
        .and_then(|(owner, name)| Repo::open(config::repo_dir(owner, name).ok()?))
        .and_then(|repo| repo.auto_push_enabled().ok());
    // A policy that uploads automatically publishes nothing while its repository's own automatic
    // publication preference on this machine is off. The folder is then blocked, not uploading:
    // reported as "on", its conversations would quietly stay here.
    let (state, queued, last) = match &folder.project {
        Some(project) => (
            match (project.enabled && project.auto_upload, auto_push) {
                (true, Some(false)) => "blocked",
                (true, _) => "on",
                (false, _) => "off",
            },
            markers(&folder.root).map_or(0, |markers| markers.len()),
            project.last_result.as_ref().map(last_upload),
        ),
        None => ("local_only", 0, None),
    };
    let repository_url =
        slug.map(|(owner, name)| format!("{}/@{owner}/{name}", folder.hub.trim_end_matches('/')));
    let recent: Vec<_> = folder
        .sessions
        .iter()
        .take(LISTED_SESSIONS)
        .map(|session| session_json(session, store, checkouts, last.as_ref(), &folder.hub))
        .collect();
    let awaiting_upload = folder
        .sessions
        .iter()
        .filter(|session| {
            matches!(
                standing(session, store, checkouts, last.as_ref()).status,
                "waiting" | "not_uploaded"
            )
        })
        .count();
    // The refusal a disabled `push.auto` produced says nothing once the folder uploads again;
    // shown as its last result, it reads as a failure that no longer exists.
    let shown = last.as_ref().filter(|last| {
        !(state == "on"
            && !last.ok
            && last
                .error
                .as_deref()
                .is_some_and(|error| error.contains(super::auto_push::DISABLED_FOR_REPOSITORY)))
    });
    serde_json::json!({
        "folder": folder.root,
        "state": state,
        "repository": folder.repository,
        "repository_url": repository_url,
        "auto_push": auto_push,
        "queued": queued,
        "last_upload": shown.map(|last| serde_json::json!({
            "at": last.at, "ok": last.ok, "error": last.error, "session_id": last.session_id,
        })),
        "sessions_here": folder.sessions.len(),
        "awaiting_upload": awaiting_upload,
        "recent": recent,
    })
}

/// Reads local state only and changes none of it: no lock is taken, so a listing never delays
/// or turns away a capture worker.
fn listing() -> Result<serde_json::Value> {
    let (mut folders, policies_complete) = listed()?;
    let sessions_complete = assign_sessions(&mut folders)?;
    let store = Store::open().ok().flatten();
    let mut checkouts = Checkouts::new();
    let folders: Vec<_> = folders
        .iter()
        .map(|folder| folder_json(folder, store.as_ref(), &mut checkouts))
        .collect();
    Ok(serde_json::json!({
        "schema_version": 1,
        "hub": config::hub_url(),
        "folders": folders,
        "incomplete": !(policies_complete && sessions_complete),
    }))
}

fn list() -> CmdResult {
    let value = listing()?;
    if super::json::requested() {
        println!("{value}");
        return Ok(ExitCode::Ok);
    }
    let folders = value["folders"].as_array().map_or(&[][..], Vec::as_slice);
    if folders.is_empty() {
        println!("No folder is enrolled or bound yet; enroll one with `agit project bind`.");
    }
    for folder in folders {
        let state = match folder["state"].as_str() {
            Some("on") => "uploads automatically",
            Some("off") => "paused",
            Some("blocked") => "upload blocked",
            _ => "saves on this computer",
        };
        let repository = folder["repository"].as_str().unwrap_or_default();
        let count = folder["sessions_here"].as_u64().unwrap_or_default();
        println!(
            "{}  {}  {state}  ({count} conversation{})",
            ui::tilde(Path::new(folder["folder"].as_str().unwrap_or_default())),
            ui::bold(repository),
            if count == 1 { "" } else { "s" }
        );
        if folder["state"] == "blocked" {
            let waiting = folder["awaiting_upload"].as_u64().unwrap_or_default();
            println!(
                "  {} automatic upload (push.auto) is off for {repository} on this computer, so \
                 {waiting} conversation{} here {} saved but not uploaded; turn it on with \
                 `agit config --repo {repository} push.auto true`",
                ui::warn_text("blocked:"),
                if waiting == 1 { "" } else { "s" },
                if waiting == 1 { "is" } else { "are" },
            );
        } else if folder["last_upload"]["ok"] == false {
            println!(
                "  {} {}",
                ui::warn_text("the last upload failed:"),
                folder["last_upload"]["error"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(ExitCode::Ok)
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
                members_read: None,
            },
        )
        .unwrap();
        assert_eq!(audience(), Some((Some("private".into()), Some(true))));

        policy.auto_upload = false;
        save(&policy).unwrap();
        assert!(announced("fresh", &inside).is_none());
    }

    /// Moving a folder to another repository releases its routing first; a move that then fails
    /// must leave the folder exactly as it was. Restoring only the policy, and not the directory
    /// binding, or leaving a file the failed move created, routes the folder to neither
    /// repository. The move can stop before the binding is rewritten, and the routing still comes
    /// back. A setup another command committed in between is kept: writing the old routing over it
    /// would undo a move that already reported success.
    #[test]
    fn a_released_folder_is_restored_exactly() {
        if !isolated("a_released_folder_is_restored_exactly") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let project = project(work.path());
        save(&project).unwrap();
        workspace::bind(&project.root, &project.repository, false).unwrap();
        let catch_up = state_path(&project.root)
            .unwrap()
            .with_extension("catch-up");
        let previous = unbind_for_move(&project.root).unwrap();
        assert!(read(&project.root).unwrap().is_none());
        workspace::bind(&project.root, "alice/elsewhere", true).unwrap();
        fs::write(&catch_up, b"").unwrap();
        previous.restore(&project.root, "alice/elsewhere").unwrap();
        assert_eq!(
            read(&project.root).unwrap().unwrap().repository,
            "alice/app"
        );
        assert_eq!(workspace::read(&project.root).unwrap().repo, "alice/app");
        assert!(!catch_up.exists());

        // The binding could not be rewritten: it still names the previous repository.
        let previous = unbind_for_move(&project.root).unwrap();
        previous.restore(&project.root, "alice/elsewhere").unwrap();
        assert_eq!(
            read(&project.root).unwrap().unwrap().repository,
            "alice/app"
        );

        let previous = unbind_for_move(&project.root).unwrap();
        workspace::bind(&project.root, "alice/third", true).unwrap();
        assert!(previous.restore(&project.root, "alice/elsewhere").is_err());
        assert_eq!(workspace::read(&project.root).unwrap().repo, "alice/third");
        assert!(read(&project.root).unwrap().is_none());
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
        assert_eq!(
            placement(&project, &candidate(fresh)).unwrap(),
            Placement::New
        );
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

    /// `project list` reports every existing folder with a policy or a binding and the state it
    /// actually uploads in, and gives each conversation to the deepest folder holding it, named
    /// by the runtime's own title before its opening request. A listing that reads only policies
    /// drops bound folders; one that trusts the policy alone calls a folder whose repository has
    /// automatic publication off "on" instead of blocked, and one that counts only the listed
    /// conversations understates what waits there; one that keeps that refusal as the last
    /// result once publication is back on reports a failure that no longer exists; one that takes
    /// the first folder matching by prefix hands a nested folder's conversations to its parent;
    /// one that takes an upload recorded after a save as proof calls a conversation uploaded whose
    /// newer commit never left this computer; and one that names a conversation by its opening
    /// request alone ignores the user's name.
    #[test]
    fn listing_reports_each_folder_and_gives_conversations_to_the_nearest_one() {
        if !isolated("listing_reports_each_folder_and_gives_conversations_to_the_nearest_one") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let outer = work.path().join("app");
        let nested = outer.join("tools");
        let paused = work.path().join("paused");
        let gone = work.path().join("gone");
        for dir in [&nested, &paused, &gone] {
            fs::create_dir_all(dir).unwrap();
        }
        let mut on = project(&outer);
        on.last_result = Some(serde_json::json!({
            "at": "2026-01-01T00:00:00Z", "ok": false, "session_id": "s",
            "error": "push failed: [{\"level\":\"error\",\"message\":\"write access was revoked\"}]",
        }));
        save(&on).unwrap();
        let mut off = project(&paused);
        off.enabled = false;
        save(&off).unwrap();
        let muted = work.path().join("muted");
        fs::create_dir_all(&muted).unwrap();
        let mut quiet = project(&muted);
        quiet.repository = "alice/muted".into();
        let checkout = config::repo_dir("alice", "muted").unwrap();
        fs::create_dir_all(&checkout).unwrap();
        let muted_repo = Repo::init(&checkout).unwrap();
        muted_repo.set_auto_push(Some(false)).unwrap();
        // Two saves of one conversation; the later upload record names only the first.
        let tree = muted_repo.git(&["write-tree"]).unwrap();
        let commit = |message: &str, parent: Option<&str>| {
            let mut args = vec!["-c", "user.name=t", "-c", "user.email=t@example.com"];
            args.extend(["commit-tree", tree.trim(), "-m", message]);
            args.extend(parent.into_iter().flat_map(|parent| ["-p", parent]));
            muted_repo.git(&args).unwrap().trim().to_owned()
        };
        let first = commit("first save", None);
        let second = commit("second save", Some(&first));
        muted_repo
            .git(&["update-ref", "refs/heads/notes", &second])
            .unwrap();
        let uploaded = |head: &str| {
            serde_json::json!({
                "at": "2999-01-01T00:00:00Z", "ok": true,
                "session_id": "cccccccc-0000-4000-8000-000000000023", "head": head,
            })
        };
        quiet.last_result = Some(uploaded(&first));
        let resumed = work.path().join("resumed");
        fs::create_dir_all(&resumed).unwrap();
        let mut back = project(&resumed);
        back.repository = "alice/resumed".into();
        back.last_result = Some(serde_json::json!({
            "at": "2026-01-01T00:00:00Z", "ok": false, "session_id": "s",
            "error": format!("push failed: {}", super::super::auto_push::DISABLED_FOR_REPOSITORY),
        }));
        save(&back).unwrap();
        let resumed_checkout = config::repo_dir("alice", "resumed").unwrap();
        fs::create_dir_all(&resumed_checkout).unwrap();
        Repo::init(&resumed_checkout)
            .unwrap()
            .set_auto_push(Some(true))
            .unwrap();
        workspace::bind(&nested, "alice/tools", false).unwrap();
        workspace::bind(&gone, "alice/gone", false).unwrap();
        fs::remove_dir(&gone).unwrap();

        let claude = |cwd: &Path, id: &str, prompt: &str, title: Option<&str>| {
            let cwd = cwd.canonicalize().unwrap();
            let native = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap())
                .join("projects")
                .join(adapter::claude_code::slug_for(&cwd));
            fs::create_dir_all(&native).unwrap();
            let mut records = vec![serde_json::json!({"type": "user", "isSidechain": false,
                "sessionId": id, "cwd": cwd, "message": {"role": "user", "content": prompt}})];
            records.extend(title.map(|title| {
                serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": id})
            }));
            let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
            fs::write(native.join(format!("{id}.jsonl")), lines).unwrap();
        };
        claude(
            &outer,
            "aaaaaaaa-0000-4000-8000-000000000021",
            "the login page loops",
            Some("Fix the login page"),
        );
        claude(
            &nested,
            "bbbbbbbb-0000-4000-8000-000000000022",
            "add a release script",
            None,
        );
        claude(
            &muted,
            "cccccccc-0000-4000-8000-000000000023",
            "draft the notes",
            None,
        );
        let mut saved = link::Link::new(
            "claude-code",
            "cccccccc-0000-4000-8000-000000000023",
            Some(&muted),
        );
        saved.owner = Some("alice".into());
        saved.agent = Some("muted".into());
        saved.branch = Some("notes".into());
        link::write(&Store::at(config::store_root().unwrap()), &saved).unwrap();

        save(&quiet).unwrap();

        let value = listing().unwrap();
        assert_eq!(value["incomplete"], false, "{value:#}");
        let folders = value["folders"].as_array().unwrap();
        let folder = |dir: &Path| {
            let dir = serde_json::json!(dir.canonicalize().unwrap());
            folders
                .iter()
                .find(|folder| folder["folder"] == dir)
                .unwrap()
        };
        assert_eq!(folders.len(), 5, "{value:#}");
        assert_eq!(folder(&outer)["state"], "on");
        assert_eq!(folder(&paused)["state"], "off");
        assert_eq!(folder(&nested)["state"], "local_only");
        assert_eq!(folder(&muted)["state"], "blocked");
        assert_eq!(folder(&muted)["auto_push"], false);
        assert_eq!(folder(&muted)["awaiting_upload"], 1);
        assert_eq!(folder(&muted)["recent"][0]["status"], "not_uploaded");
        assert_eq!(folder(&resumed)["state"], "on");
        assert!(folder(&resumed)["last_upload"].is_null(), "{value:#}");
        assert_eq!(
            folder(&outer)["last_upload"]["error"],
            "push failed: write access was revoked"
        );
        assert_eq!(folder(&outer)["sessions_here"], 1);
        assert_eq!(folder(&outer)["recent"][0]["title"], "Fix the login page");
        assert_eq!(folder(&nested)["sessions_here"], 1);
        let opened = &folder(&nested)["recent"][0];
        assert_eq!(opened["title"], "add a release script");
        assert_eq!(opened["status"], "not_saved");

        quiet.last_result = Some(uploaded(&second));
        save(&quiet).unwrap();
        let value = listing().unwrap();
        let muted = value["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|folder| folder["folder"] == serde_json::json!(muted.canonicalize().unwrap()))
            .unwrap();
        assert_eq!(muted["recent"][0]["status"], "uploaded", "{value:#}");
        assert_eq!(muted["awaiting_upload"], 0);
    }

    /// A paused, upload-off or excluding project swallows only passive capture: the Stop hook
    /// treats the session as handled. A session claimed on its own branch is not handled there,
    /// so the hook settles it from its link; a capture that also swallowed it would drop the
    /// turns of a session `agit run` launched in the folder.
    #[test]
    fn paused_project_leaves_explicitly_claimed_sessions_to_settlement() {
        if !isolated("paused_project_leaves_explicitly_claimed_sessions_to_settlement") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let project = project(work.path());
        let store = Store::at(config::store_root().unwrap());
        let claimed = "aaaaaaaa-0000-4000-8000-000000000041";
        let mut claim = link::Link::new("codex", claimed, Some(&project.root));
        claim.owner = Some("alice".into());
        claim.agent = Some("app".into());
        claim.branch = Some("run-1".into());
        link::write(&store, &claim).unwrap();
        let passive = "bbbbbbbb-0000-4000-8000-000000000042";
        let cwd = project.root.to_str().unwrap().to_owned();
        for pause in 0..3 {
            let mut paused = project.clone();
            match pause {
                0 => paused.enabled = false,
                1 => paused.auto_upload = false,
                _ => paused
                    .excluded
                    .extend([key("codex", claimed), key("codex", passive)]),
            }
            save(&paused).unwrap();
            assert!(!capture("codex", claimed, Some(&cwd)).unwrap());
            assert!(capture("codex", passive, Some(&cwd)).unwrap());
            assert!(markers(&paused.root).unwrap().is_empty());
        }
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

    /// A branch the checkout's Hub refused as withdrawn is not pushed after a turn, until a named
    /// push releases it. Checking only the project policy would start an automatic push on every
    /// turn and warn each time that it failed, after the refusal was already reported once.
    #[test]
    fn a_branch_the_hub_refused_is_not_pushed_automatically() {
        if !isolated("a_branch_the_hub_refused_is_not_pushed_automatically") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let repo = Repo::init(&temp.path().join("repo")).unwrap();
        repo.git(&["checkout", "-b", "work"]).unwrap();
        meta::write(repo.root(), &Meta::new_file_line()).unwrap();
        repo.add_all().unwrap();
        repo.commit("File line").unwrap();
        let identity = project(temp.path()).identity;
        crate::hub::identity::pin(&repo, &identity).unwrap();
        assert!(!blocks_auto_push(&repo, "alice/app", "work").unwrap());
        let refused = ["refs/heads/work".to_owned()];
        crate::domain::privacy_git::withhold_refs(&repo, &identity, &refused).unwrap();
        assert!(blocks_auto_push(&repo, "alice/app", "work").unwrap());
        crate::domain::privacy_git::release_refs(&repo, &identity, &refused).unwrap();
        assert!(!blocks_auto_push(&repo, "alice/app", "work").unwrap());
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

    /// A branch is named by the words of the request, never by the markup around a paste.
    #[test]
    fn a_pasted_request_names_its_branch_by_what_was_asked() {
        let name = |text: &str| super::super::front::slug(&without_markup(text), 40);
        assert_eq!(
            name("<pasted_content id=\"8d2d\">Next task Y8 from 4a8d</pasted_content>"),
            "next-task-y8-from-4a8d"
        );
        assert_eq!(
            name("[Pasted text #1 +20 lines] fix the login"),
            "fix-the-login"
        );
        assert_eq!(name("compare a < b and c > d"), "compare-a-b-and-c-d");
    }

    /// A capture named by its request is known by the conversation its line records as opening
    /// it, so the capture rule holds when this machine's list of captures is gone (a new machine,
    /// a wiped AGIT_HOME): a conversation started outside the folder is withheld, and so is one
    /// whose opener this machine cannot find, and so is one whose opener the user excluded after
    /// a resume moved the claim to a new session id, while the project's own conversation and a
    /// branch the user named pass. A gate that knew captures only by their legacy name, or only by
    /// the local list, would publish the first three.
    #[test]
    fn a_capture_named_by_its_request_keeps_the_capture_rule_without_the_local_list() {
        if !isolated("a_capture_named_by_its_request_keeps_the_capture_rule_without_the_local_list")
        {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let project = project(work.path());
        save(&project).unwrap();
        let store = Store::at(config::store_root().unwrap());
        let named = |claim: link::Link, branch: &str| {
            let mut claim = claim;
            claim.branch = Some(branch.into());
            link::write(&store, &claim).unwrap();
            claim
        };
        let inside = named(
            captured(
                &project,
                "aaaaaaaa-0000-4000-8000-000000000031",
                &project.root,
            ),
            "add-dark-mode-1a2b",
        );
        let strayed = named(
            captured(
                &project,
                "bbbbbbbb-0000-4000-8000-000000000032",
                &outside.path().canonicalize().unwrap(),
            ),
            "fix-login-redirect-3c4d",
        );
        let users = named(
            captured(
                &project,
                "cccccccc-0000-4000-8000-000000000033",
                &outside.path().canonicalize().unwrap(),
            ),
            "my-notes",
        );
        let opener = named(
            captured(
                &project,
                "eeeeeeee-0000-4000-8000-000000000035",
                &project.root,
            ),
            "rename-the-tabs-7a8b",
        );
        let mut resumed = opener.clone();
        resumed.session_id = "ffffffff-0000-4000-8000-000000000036".into();
        link::write(&store, &resumed).unwrap();
        let mut superseded = opener.clone();
        superseded.superseded_by = Some(resumed.instance());
        link::write(&store, &superseded).unwrap();
        let mut project = project;
        project
            .excluded
            .insert(key(&opener.source, &opener.session_id));
        save(&project).unwrap();
        let repo = Repo::init(&config::repo_dir("alice", "app").unwrap()).unwrap();
        let line = |branch: &str, session: &str, capture: bool| {
            repo.git(&["checkout", "-q", "-B", branch]).unwrap();
            let mut metadata = Meta::new(
                format!("agit-{}", "a".repeat(40)),
                "claude-code".into(),
                project.root.to_string_lossy().into_owned(),
            );
            metadata.capture = capture.then(|| meta::CaptureOrigin {
                runtime: "claude-code".into(),
                session: session.into(),
            });
            meta::write(repo.root(), &metadata).unwrap();
            repo.add_all().unwrap();
            repo.commit(branch).unwrap();
        };
        line("add-dark-mode-1a2b", &inside.session_id, true);
        line("fix-login-redirect-3c4d", &strayed.session_id, true);
        line("my-notes", &users.session_id, false);
        line("rename-the-tabs-7a8b", &opener.session_id, true);
        line(
            "lease-notes-5e6f",
            "dddddddd-0000-4000-8000-000000000034",
            true,
        );
        assert!(!home().unwrap().join(CAPTURED).exists());
        assert_eq!(withheld("alice/app", "add-dark-mode-1a2b").unwrap(), None);
        assert_eq!(
            withheld("alice/app", "fix-login-redirect-3c4d").unwrap(),
            Some(Withholding::StartedOutside)
        );
        assert_eq!(
            withheld("alice/app", "lease-notes-5e6f").unwrap(),
            Some(Withholding::UnknownStart)
        );
        assert_eq!(withheld("alice/app", "my-notes").unwrap(), None);
        assert_eq!(
            withheld("alice/app", "rename-the-tabs-7a8b").unwrap(),
            Some(Withholding::Excluded)
        );
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
