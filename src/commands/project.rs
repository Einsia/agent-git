//! Explicit project enrollment composes discovery, import and ordinary publication.

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
    config::create_state_dir(&home()?)?;
    let path = state_path(root)?.with_extension("lock");
    let file = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    crate::infra::local_state::lock_patiently(&file, &path, "the project operation lock", true)?;
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
                .or_else(|| crate::tui::screens::selector::preview(runtime, &session.path).cwd);
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

/// Where a project session is published. A native session holds one claim, and the project
/// never moves it: a session claimed by another repository, such as a desktop RC project, is
/// published to the project repository as a separate copy of that claim's branch.
#[derive(Debug, PartialEq, Eq)]
enum Placement {
    /// The project repository holds, or will hold, the session's claim on this branch.
    Claim(String),
    /// Another repository holds the claim; the value is its `owner/repo@branch`.
    Copy(String),
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
                return Ok(Placement::Copy(format!("{source}@{branch}")));
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
    Ok(Placement::Claim(format!(
        "project-{}-{}",
        candidate.runtime,
        hash(&key(&candidate.runtime, &candidate.session_id))
    )))
}

/// The outcome of one session's publication.
enum Synced {
    Claimed(String),
    Copied(String),
}

/// Who started a publication. Only an explicit sync may publish a copy: a hook capture checked
/// its placement before waiting for the project lock, and the claim can move to another
/// repository while it waits, so the placement read under the lock decides again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Explicit,
    Hook,
}

/// The placement a publication acts on, re-read where it is used.
fn publication(project: &Project, candidate: &Candidate, trigger: Trigger) -> Result<Placement> {
    let placement = placement(project, candidate)?;
    ensure!(
        trigger == Trigger::Explicit || matches!(placement, Placement::Claim(_)),
        "session is claimed by another repository; only an explicit project sync publishes its copy"
    );
    Ok(placement)
}

fn sync_one(project: &Project, candidate: &Candidate, trigger: Trigger) -> Result<Synced> {
    verify(project)?;
    let branch = match publication(project, candidate, trigger)? {
        Placement::Claim(branch) => branch,
        Placement::Copy(source) => {
            // An explicit separate publication: the claim, and an RC repository's own
            // publication target, stay where they are.
            child(
                &[
                    "push".into(),
                    source.clone(),
                    "--to".into(),
                    project.repository.clone(),
                    "--separate".into(),
                    "--yes".into(),
                ],
                &candidate.cwd,
                false,
            )?;
            return Ok(Synced::Copied(source));
        }
    };
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
    Ok(Synced::Claimed(branch))
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
        let outcome = match sync_one(project, &candidate, Trigger::Explicit) {
            Ok(Synced::Claimed(branch)) => {
                serde_json::json!({"branch": branch, "status": "pushed"})
            }
            Ok(Synced::Copied(source)) => serde_json::json!({"source": source, "status": "copied"}),
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
        } => bind(&path, &repo, history, auto_upload, dry_run),
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
            project.enabled = false;
            save(&project)?;
            println!(
                "Project capture stopped. Session history and repository preferences are unchanged."
            );
            Ok(ExitCode::Ok)
        }
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

fn bind(
    path: &Path,
    repository: &str,
    history: History,
    automatic: bool,
    dry_run: bool,
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
    };
    let rows = candidates(&project)?;
    if dry_run {
        println!(
            "{}",
            serde_json::json!({"directory":root,"repository":repository,"history":history,"repository_auto_upload":automatic,"candidates":rows})
        );
        return Ok(ExitCode::Ok);
    }
    ensure!(
        !crate::rc::harness::settlement_is_delegated(),
        "configure project capture in a device terminal outside the RC supervisor"
    );
    let _guard = lock(&root)?;
    if let Some(binding) = workspace::read(&root) {
        ensure!(
            binding.repo == repository,
            "directory is bound to {}; rebind it explicitly first",
            binding.repo
        );
    }
    if let Some(existing) = read(&root)? {
        ensure!(
            existing.repository == repository,
            "project is already assigned to another repository"
        );
        project.last_result = existing.last_result;
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
    remote.require_encryption_enabled()?;
    let me = super::remote_request(client.me())?;
    project.account = me.username;
    project.account_id = me.account_id;
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
            "Automatic upload uses the existing repository-wide push.auto preference. Other managed sessions in this local repository inherit that preference. Uploads retain all identity and privacy checks, and an encrypted repository's consent checks."
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
    if automatic {
        let runtimes = super::setup::project_hook_runtimes()?;
        ensure!(
            super::config::get("commit.auto").as_deref() != Some("false"),
            "commit.auto is disabled; enable it explicitly before automatic project capture"
        );
        // Only encrypted automatic publication reads saved consent; an ordinary repository's
        // automatic uploads follow push.auto alone, so no policy is shown or saved for it.
        let consent = super::push::publication_consent(&repo, repository, &project.identity)?;
        let encrypted = consent.mode.is_encrypted();
        if encrypted {
            println!(
                "Automatic publication policy: {}",
                serde_json::to_string(&consent)?
            );
            confirm(
                "Authorize future publication under this exact repository policy and enable repository-wide automatic push?",
            )?;
        } else {
            confirm("Enable repository-wide automatic push to this ordinary repository?")?;
        }
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
        if encrypted {
            consent.save(&repo)?;
        }
        repo.set_auto_push(Some(true))?;
    }
    workspace::bind(&root, repository, false)?;
    save(&project)?;
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
    // A session claimed elsewhere is left to its own repository; only an explicit sync copies it.
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
    // A copy is an explicit separate publication; automatic capture leaves a session claimed
    // elsewhere to its own repository's settlement.
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
    let result = sync_one(&project, &candidate, Trigger::Hook);
    project.last_result = Some(
        serde_json::json!({"at":chrono::Utc::now(),"session_id":candidate.session_id,"ok":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:#}"))}),
    );
    save(&project)?;
    result?;
    Ok(true)
}

pub(crate) fn registration_scope(cwd: &Path) -> bool {
    nearest(cwd)
        .ok()
        .flatten()
        .is_some_and(|p| p.enabled && p.auto_upload && includes(&p, cwd).unwrap_or(false))
}

pub(crate) fn blocks_auto_push(repo: &Repo, slug: &str, branch: &str) -> Result<bool> {
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
        }
    }

    /// A session claimed by another repository, such as a desktop RC project, is published as a
    /// copy of that claim; a session claimed here keeps its branch. A placement that refused
    /// every foreign claim would leave RC sessions out of the project, and one that imported them
    /// independently would move a claim its own repository still settles.
    #[test]
    fn foreign_claims_are_copied_and_never_reassigned() {
        if !isolated("foreign_claims_are_copied_and_never_reassigned") {
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
            Placement::Copy("desktop-machine/project-local@rc-branch".into())
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
        // A hook capture that saw an unclaimed session refuses once the claim has moved to
        // another repository, instead of publishing the copy only an explicit sync may make.
        assert!(publication(&project, &candidate(fresh), Trigger::Hook).is_ok());
        claim(fresh, "desktop-machine", "project-local", "moved");
        assert!(publication(&project, &candidate(fresh), Trigger::Hook).is_err());
        assert_eq!(
            publication(&project, &candidate(fresh), Trigger::Explicit).unwrap(),
            Placement::Copy("desktop-machine/project-local@moved".into())
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
}
