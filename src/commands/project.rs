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
        /// The person's consent, given in agit's menu bar, to upload automatically into this
        /// public repository. It must name the `--repo` repository; from inside an agent session
        /// it is an unattended answer, like `--yes`.
        #[arg(long, value_name = "OWNER/REPO", requires = "auto_upload", hide = true)]
        public_consent: Option<String>,
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
    /// Sessions the person stopped uploading (`agit project drop`). A policy written before
    /// `earlier` existed also kept its earlier sessions here, and the two cannot be told apart, so
    /// every entry counts as dropped: only a save that names the session lets it back in.
    excluded: BTreeSet<String>,
    /// Sessions that ran here before the folder was bound without earlier history. A save that
    /// asks for earlier sessions (`agit save --all`, `--today`) lets them in.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    earlier: BTreeSet<String>,
    /// Whether `earlier` holds the OpenCode conversations that ran here before the binding. A
    /// policy bound without it never looked for them, so every OpenCode conversation there counts
    /// as earlier until it is saved into this repository.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    earlier_opencode: bool,
    last_result: Option<serde_json::Value>,
    /// The repository's own push.auto override as it stood before bind turned automatic push
    /// on; absent when bind did not turn it on. Unbind puts it back, so a binding never leaves
    /// every managed session of the repository uploading after the folder is unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    push_auto_before: Option<PushAutoBefore>,
}

impl Project {
    /// Whether the policy keeps this session from uploading by itself: dropped, or from before
    /// the folder was on.
    fn leaves_out(&self, session_key: &str) -> bool {
        self.excluded.contains(session_key) || self.earlier.contains(session_key)
    }
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
    // An off the person recorded for this destination before its checkout existed holds: a
    // folder bind is not their choice to upload into it. Only their own `push.auto true`, or
    // naming it with `agit on --repo`, lifts it.
    // A recorded off also outranks a setting the checkout holds, as the checkout's own reads do,
    // so a bind over a refused command's leftover setting says why it stays off.
    let (owner, name) = super::parse_slug(&project.repository)?;
    let recorded = crate::domain::repo::automatic_upload_off::recorded(&owner, &name);
    if recorded == crate::domain::repo::automatic_upload_off::Recorded::Off
        || (repo.auto_push_override()?.is_none() && recorded.holds_off())
    {
        println!(
            "automatic upload into {} stays off: you turned it off on this computer; `agit config --repo {} push.auto true` turns it on",
            project.repository, project.repository
        );
        return Ok(());
    }
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

#[derive(Clone, Serialize, Deserialize)]
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

/// Whether `path` lies in an agit project folder or a bound workspace, where what is written can
/// be captured by automatic upload.
pub(crate) fn covers(path: &Path) -> Result<bool> {
    for ancestor in path.ancestors() {
        if read(ancestor)?.is_some() || workspace::read(ancestor).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `path` is in the project's folder and no folder between them is set up on its own.
fn within(project: &Project, path: &Path) -> Result<bool> {
    within_root(&project.root, path)
}

/// [`within`] for a project at `root`, whether or not it exists yet.
fn within_root(root: &Path, path: &Path) -> Result<bool> {
    if !path.starts_with(root) {
        return Ok(false);
    }
    for ancestor in path.ancestors() {
        if ancestor == root {
            return Ok(true);
        }
        if read(ancestor)?.is_some() || workspace::read(ancestor).is_some() {
            return Ok(false);
        }
    }
    Ok(false)
}

/// How a project covers a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    /// The path is in the project's folder.
    Folder,
    /// The path is in this linked git worktree of the project's folder, which is its main
    /// checkout.
    Worktree(PathBuf),
}

/// How `project` covers `path`, if it does. A folder covers what is in it, and, when it is a main
/// checkout, the linked git worktrees that share its git common directory, as far as it would
/// cover the same place in itself. A path with a setting of its own, or below a folder with one,
/// keeps it: only a path nobody set up is widened. Coverage flows from the main checkout only, so
/// a worktree turned on covers neither the main checkout nor its siblings, and a separate clone of
/// the same remote or a submodule is never covered.
fn coverage(project: &Project, path: &Path) -> Result<Option<Via>> {
    coverage_at(&project.root, path)
}

/// [`coverage`] for a project at `root`, whether or not it exists yet.
fn coverage_at(root: &Path, path: &Path) -> Result<Option<Via>> {
    if path.starts_with(root) {
        return Ok(within_root(root, path)?.then_some(Via::Folder));
    }
    let Ok(path) = path.canonicalize() else {
        return Ok(None);
    };
    let path = path.as_path();
    for ancestor in path.ancestors() {
        if read(ancestor)?.is_some() || workspace::read(ancestor).is_some() {
            return Ok(None);
        }
    }
    let Some(linked) = crate::domain::worktree::linked(path) else {
        return Ok(None);
    };
    if linked.main != root {
        return Ok(None);
    }
    let Ok(inside) = path.strip_prefix(&linked.worktree) else {
        return Ok(None);
    };
    Ok(within_root(root, &linked.main.join(inside))?.then_some(Via::Worktree(linked.worktree)))
}

fn includes(project: &Project, path: &Path) -> Result<bool> {
    Ok(coverage(project, path)?.is_some())
}

/// The project policy that covers `path`, paused or not, and how it covers it.
fn governing(path: &Path) -> Result<Option<(Project, Via)>> {
    let project = match nearest(path)? {
        Some(project) => project,
        None => match crate::domain::worktree::linked(path) {
            Some(linked) => match read(&linked.main)? {
                Some(project) => project,
                None => return Ok(None),
            },
            None => return Ok(None),
        },
    };
    Ok(coverage(&project, path)?.map(|via| (project, via)))
}

/// The main checkout whose policy covers `folder` as one of its git worktrees.
pub(crate) fn worktree_of(folder: &Path) -> Option<PathBuf> {
    match governing(folder).ok()?? {
        (project, Via::Worktree(_)) => Some(project.root),
        _ => None,
    }
}

/// The folder whose policy and records decide for `folder`: the main checkout of a git worktree
/// it covers, else `folder` itself.
fn policy_root(folder: &Path) -> PathBuf {
    worktree_of(folder).unwrap_or_else(|| folder.to_path_buf())
}

/// What the person dropped for conversations in `folder` while another policy decided for it: the
/// folder above it, or the main checkout of the git worktree it is. A folder that gets a policy of
/// its own keeps them, so a drop survives `agit off` and `agit on` there and `agit save --all`.
/// The whole set comes along; keys of sessions that ran elsewhere never match one that runs here.
fn inherited_drops(folder: &Path) -> Result<BTreeSet<String>> {
    Ok(match governing(folder)? {
        Some((covering, _)) if covering.root != folder => covering.excluded,
        _ => BTreeSet::new(),
    })
}

/// What a new binding of `folder` keeps of what the person dropped there (#266): the drops of the
/// policy it replaces (another repository's, on `--rebind`), of the policy a move to another
/// repository released (`agit on --repo`), and of the policy that covered it until now.
fn kept_drops(folder: &Path) -> Result<BTreeSet<String>> {
    let mut drops = inherited_drops(folder)?;
    if let Some(replaced) = read(folder)? {
        drops.extend(replaced.excluded);
    }
    drops.extend(moved_drops(folder)?);
    Ok(drops)
}

/// The drops of the policy that `unbind_for_move` released at `root`, kept until the binding that
/// completes the move takes them over, or the move is undone. A record that cannot be read is an
/// error, never an empty set: binding without the drops would lose them, and the record stays.
fn moved_drops(root: &Path) -> Result<BTreeSet<String>> {
    let path = state_path(root)?.with_extension("moved-drops");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "cannot read what was dropped before the folder moved ({})",
                    path.display()
                )
            });
        }
    };
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "cannot read what was dropped before the folder moved ({})",
            path.display()
        )
    })
}

/// The linked git worktrees a policy at `root` would cover if the folder were turned on now: the
/// ones `agit on` there names as also covered.
pub(crate) fn worktrees_covered_from(root: &Path) -> Vec<PathBuf> {
    crate::domain::worktree::of(root)
        .into_iter()
        .filter(|worktree| matches!(coverage_at(root, worktree), Ok(Some(Via::Worktree(_)))))
        .collect()
}

/// The linked git worktrees the policy at `root` covers now.
pub(crate) fn covered_worktrees(root: &Path) -> Vec<PathBuf> {
    match read(root) {
        Ok(Some(project)) => worktrees_of(&project),
        _ => Vec::new(),
    }
}

fn worktrees_of(project: &Project) -> Vec<PathBuf> {
    crate::domain::worktree::of(&project.root)
        .into_iter()
        .filter(|worktree| matches!(coverage(project, worktree), Ok(Some(Via::Worktree(_)))))
        .collect()
}

/// When the project at `root` began covering one of its git worktrees, and the conversation
/// whose start made it begin, if one did.
#[derive(Clone, Serialize, Deserialize)]
struct Covered {
    since: chrono::DateTime<chrono::Utc>,
    opener: Option<String>,
}

/// The worktrees the project at `root` covers, by when it began covering each. Kept beside the
/// policy, as the audience is, so every version still reads the policy file.
fn covered(root: &Path) -> BTreeMap<PathBuf, Covered> {
    state_path(root)
        .ok()
        .and_then(|path| fs::read(path.with_extension("worktrees")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Read-modify-write of the record of the worktrees the project at `root` covers. It has a lock of
/// its own, so a caller holding the project lock can change it too.
fn update_covered<T>(
    root: &Path,
    change: impl FnOnce(&mut BTreeMap<PathBuf, Covered>) -> T,
) -> Result<T> {
    let _guard = lock_at(
        state_path(root)?.with_extension("worktrees-lock"),
        "the worktree record lock",
    )?;
    let mut records = covered(root);
    let result = change(&mut records);
    let mut file = tempfile::NamedTempFile::new_in(home()?)?;
    file.write_all(&serde_json::to_vec(&records)?)?;
    file.persist(state_path(root)?.with_extension("worktrees"))
        .map_err(|e| e.error)?;
    Ok(result)
}

/// Where AgentGit records that Codex ran its hooks for `session`, `None` for an id that is not a
/// plain file name.
fn codex_hooked_path(session: &str) -> Option<PathBuf> {
    let plain = !session.is_empty()
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    plain
        .then(|| config::agit_home().ok())?
        .map(|home| home.join("hooks-seen").join("codex-sessions").join(session))
}

/// Records, as a Codex hook (SessionStart or Stop) runs for `session`, that Codex runs AgentGit's
/// hooks for that conversation.
pub(crate) fn codex_hooks_seen(session: &str) {
    if let Some(path) = codex_hooked_path(session)
        && path
            .parent()
            .is_some_and(|dir| config::create_state_dir(dir).is_ok())
    {
        let _ = fs::write(path, b"");
    }
}

/// Whether Codex's `session` may be saved without the person naming it and without its own hook
/// running: AgentGit has seen a hook of that very conversation. `agit on` tells the person nothing
/// from Codex is saved until Codex runs the hooks, and a person who stops trusting them makes that
/// true again, so a folder where they once ran is not enough. A catch-up
/// then only completes an upload the hooks already started. `root`, the bound folder, is kept for
/// callers that judge per folder; the answer depends on the conversation alone.
pub(crate) fn codex_capture_allowed(_root: &Path, session: &str) -> bool {
    codex_hooked_path(session).is_some_and(|path| path.exists())
}

/// Why the folder whose policy decides for `path` keeps `session` of `runtime` from being saved or
/// uploaded without the person asking, if it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeptBack {
    /// The person dropped it, or it ran before the folder was on.
    LeftOut,
    /// The folder doesn't upload by itself.
    NoAutomaticUpload,
    /// The folder is off.
    Off,
    /// The folder's policy can't be read.
    Unreadable,
}

/// Whether, and why, the folder whose policy decides for `path` keeps `session` of `runtime` back.
/// `Err` holds the reason; `Ok(None)` when no folder covers `path`.
fn folder_judgement(path: &Path, runtime: &str, session: &str) -> Result<Option<()>, KeptBack> {
    match governing(path) {
        Ok(Some((project, _))) if !project.enabled => Err(KeptBack::Off),
        Ok(Some((project, _)))
            if project.leaves_out(&key(runtime, session))
                || before_covered(
                    &project,
                    &Candidate {
                        runtime: runtime.into(),
                        session_id: session.into(),
                        cwd: path.to_path_buf(),
                    },
                    false,
                )
                .unwrap_or(true) =>
        {
            Err(KeptBack::LeftOut)
        }
        Ok(Some((project, _))) if !project.auto_upload => Err(KeptBack::NoAutomaticUpload),
        Ok(Some(_)) => Ok(Some(())),
        Ok(None) => Ok(None),
        Err(_) => Err(KeptBack::Unreadable),
    }
}

/// What the folder whose policy decides for `path` says about saving and uploading `session` of
/// `runtime`, running in `path`, without the person asking: no while the folder is off or doesn't
/// upload by itself, or for a session it leaves out (dropped, from before the folder was on, or
/// from before the folder covered its git worktree, or of unknown start there); yes otherwise. `None`
/// when no folder covers `path`; a policy that can't be read says no.
pub(crate) fn folder_decision(path: &Path, runtime: &str, session: &str) -> Option<bool> {
    match folder_judgement(path, runtime, session) {
        Ok(decided) => decided.map(|()| true),
        Err(_) => Some(false),
    }
}

/// A session branch this computer saw accepted on the Hub although the folder its conversation ran
/// in keeps it from uploading by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeptBackUpload {
    pub repository: String,
    pub branch: String,
    /// The session's page on the Hub, or the repository's when the session can't be named.
    pub url: String,
}

impl KeptBackUpload {
    pub(crate) fn describe(&self) -> String {
        format!(
            "Also on the Hub from this folder: {}@{}. You can review it at {}.",
            self.repository, self.branch, self.url
        )
    }
}

/// The Hub page of the session `branch` of `repo` holds, by the checkout's pinned Hub.
fn session_page(repo: &Repo, owner: &str, agent: &str, branch: &str) -> String {
    let hub = crate::hub::identity::read(repo)
        .ok()
        .flatten()
        .map(|identity| identity.hub)
        .unwrap_or_else(config::hub_url);
    let repository = format!("{}/@{owner}/{agent}", hub.trim_end_matches('/'));
    match crate::domain::meta::read_at_ref_result(repo, &format!("refs/heads/{branch}")) {
        Ok(Some(meta)) if meta::is_bare_id(&meta.session) => {
            format!("{repository}/s/{}", meta.session)
        }
        _ => repository,
    }
}

/// Every session branch on this computer that was accepted on the Hub although its folder keeps
/// the conversation back, judged from local records only: claims, folder policies, publication
/// receipts and remote-tracking refs. Nothing is sent.
pub(crate) fn kept_back_uploads() -> Result<Vec<KeptBackUpload>> {
    let mut found = BTreeMap::new();
    for claim in link::list(&Store::at(config::store_root()?)) {
        let (Some(owner), Some(agent), Some(branch), Some(cwd)) = (
            claim.owner.as_deref(),
            claim.agent.as_deref(),
            claim.branch.as_deref(),
            claim.cwd.as_deref(),
        ) else {
            continue;
        };
        if folder_judgement(Path::new(cwd), &claim.source, &claim.session_id).is_ok() {
            continue;
        }
        let Some(repo) = Repo::open(config::repo_dir(owner, agent)?) else {
            continue;
        };
        let accepted = crate::domain::privacy_receipt::PublicationReceipt::load(&repo, branch)
            .ok()
            .flatten()
            .is_some()
            || repo
                .git_opt(&[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(refname)",
                    &format!("refs/remotes/*/{branch}"),
                ])
                .is_some_and(|refs| !refs.trim().is_empty());
        if accepted {
            let repository = format!("{owner}/{agent}");
            found
                .entry((repository.clone(), branch.to_owned()))
                .or_insert_with(|| KeptBackUpload {
                    repository,
                    branch: branch.to_owned(),
                    url: session_page(&repo, owner, agent, branch),
                });
        }
    }
    Ok(found.into_values().collect())
}

/// The kept-back uploads `agit status` has not shown yet, recorded as shown: each is told once.
pub(crate) fn kept_back_uploads_to_tell() -> Result<Vec<KeptBackUpload>> {
    let path = config::agit_home()?.join("kept-back-uploads-told.json");
    let mut told: BTreeSet<String> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => return Err(error.into()),
    };
    let fresh: Vec<_> = kept_back_uploads()?
        .into_iter()
        .filter(|upload| !told.contains(&format!("{}@{}", upload.repository, upload.branch)))
        .collect();
    if !fresh.is_empty() {
        told.extend(
            fresh
                .iter()
                .map(|upload| format!("{}@{}", upload.repository, upload.branch)),
        );
        config::create_state_dir(&config::agit_home()?)?;
        fs::write(&path, serde_json::to_vec(&told)?)?;
    }
    Ok(fresh)
}

/// Writes the policy of the folder `root` as `agit on` would, with `dropped` sessions left out as
/// `agit project drop` would leave them.
#[cfg(test)]
pub(crate) fn policy_for_test(
    root: &Path,
    enabled: bool,
    auto_upload: bool,
    dropped: &[(&str, &str)],
) {
    save(&Project {
        root: root.canonicalize().unwrap(),
        repository: "alice/app".into(),
        identity: RemoteIdentity::new(
            "https://example.test",
            "bbbbbbbb-0000-4000-8000-000000000002",
        )
        .unwrap(),
        account: "alice".into(),
        account_id: None,
        enabled,
        auto_upload,
        history: History::All,
        excluded: dropped
            .iter()
            .map(|(runtime, session)| key(runtime, session))
            .collect(),
        earlier: BTreeSet::new(),
        earlier_opencode: false,
        last_result: None,
        push_auto_before: None,
    })
    .unwrap();
}

/// Whether the project keeps `candidate` because it is a Codex session that did not run with
/// AgentGit's hooks in the folder (see [`codex_capture_allowed`]).
fn codex_unhooked(project: &Project, candidate: &Candidate) -> bool {
    candidate.runtime == "codex" && !codex_capture_allowed(&project.root, &candidate.session_id)
}

/// Records that the project at `root` covers `worktree` from now on, unless it already does.
/// `opener` is the conversation whose start is the first sight of the worktree: a runtime may
/// write a new conversation's first record before it runs the hook, so that conversation counts
/// as begun after, not before.
fn adopt_worktree(root: &Path, worktree: &Path, opener: Option<String>) -> Result<Covered> {
    if let Some(covered) = covered(root).remove(worktree) {
        return Ok(covered);
    }
    update_covered(root, |records| {
        records
            .entry(worktree.to_path_buf())
            .or_insert_with(|| Covered {
                since: chrono::Utc::now(),
                opener,
            })
            .clone()
    })
}

/// Records, as a conversation starts in `origin`, that the git worktree it starts in is covered
/// from now on, when an on folder covers it and nothing recorded that before.
pub(crate) fn adopt(origin: &Path, runtime: &str, session: &str) {
    if let Ok(Some((project, Via::Worktree(worktree)))) = governing(origin) {
        let _ = adopt_worktree(&project.root, &worktree, Some(key(runtime, session)));
    }
}

/// Whether the session is claimed in the project's repository.
fn claimed_here(project: &Project, runtime: &str, session_id: &str) -> bool {
    Store::open()
        .ok()
        .flatten()
        .and_then(|store| link::get(&store, runtime, session_id))
        .is_some_and(|link| {
            link.branch.is_some()
                && link.owner.as_deref().zip(link.agent.as_deref())
                    == project.repository.split_once('/')
        })
}

/// Whether `candidate` ran in a git worktree the project covers and began before the project
/// covered that worktree. Such a conversation is what a folder's earlier sessions are when it is
/// turned on without its history: it never uploads by itself, and only `agit save --all` or a save
/// that names it uploads it. Once claimed in the project's repository it is the project's like any
/// other. A conversation that has recorded nothing yet is only now starting; one whose start
/// cannot be read counts as earlier. `adopt` records a worktree seen for the first time as covered
/// from now; otherwise such a worktree reads as covered from now without a record.
fn before_covered(project: &Project, candidate: &Candidate, adopt: bool) -> Result<bool> {
    let Some(Via::Worktree(worktree)) = coverage(project, &candidate.cwd)? else {
        return Ok(false);
    };
    let session_key = key(&candidate.runtime, &candidate.session_id);
    if claimed_here(project, &candidate.runtime, &candidate.session_id) {
        return Ok(false);
    }
    let covered = if adopt {
        adopt_worktree(&project.root, &worktree, None)?
    } else {
        covered(&project.root)
            .remove(&worktree)
            .unwrap_or_else(|| Covered {
                since: chrono::Utc::now(),
                opener: None,
            })
    };
    if covered.opener.as_deref() == Some(session_key.as_str()) {
        return Ok(false);
    }
    // A Codex start is metadata in its first record, including while its history is cold.
    // Restoring the carrier here would make eligibility inspection rewrite native storage.
    if candidate.runtime == "codex" {
        let Ok(path) = crate::adapter::codex::resolve_readonly(&candidate.session_id) else {
            return Ok(false);
        };
        let bytes = crate::adapter::codex_rollout::first_line(
            &path,
            crate::adapter::codex_rollout::HEADER_LIMIT,
        )?;
        if bytes.is_empty() {
            return Ok(false);
        }
        let start = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|record| {
                chrono::DateTime::parse_from_rfc3339(record["timestamp"].as_str()?).ok()
            });
        return Ok(start.is_none_or(|start| start < covered.since));
    }
    let Some(transcript) = adapter::get(&candidate.runtime).ok().and_then(|adapter| {
        adapter
            .resolve_present(&candidate.session_id, Some(&candidate.cwd))
            .or_else(|| adapter.resolve(&candidate.session_id, Some(&candidate.cwd)))
    }) else {
        return Ok(false);
    };
    if fs::metadata(&transcript).is_ok_and(|meta| meta.len() == 0) {
        return Ok(false);
    }
    Ok(crate::adapter::transcript_cwd::recorded_start(&transcript)
        .is_none_or(|start| start < covered.since))
}

fn candidates(project: &Project) -> Result<Vec<Candidate>> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let worktrees = crate::domain::worktree::of(&project.root);
    for runtime in ["codex", "claude-code"] {
        let adapter = adapter::get(runtime)?;
        for session in adapter.all_session_choices()? {
            let cwd = session
                .cwd
                .or_else(|| crate::tui::screens::selector::recorded_cwd(&session.path));
            let Some(cwd) = cwd.and_then(|p| PathBuf::from(p).canonicalize().ok()) else {
                continue;
            };
            // Only a path in the folder or in one of its worktrees can be covered.
            let near = cwd.starts_with(&project.root)
                || worktrees.iter().any(|worktree| cwd.starts_with(worktree));
            if near && includes(project, &cwd)? && seen.insert(key(runtime, &session.id)) {
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

/// The OpenCode conversations of the project, found the way [`candidates`] finds the others, from
/// a complete reading of OpenCode's store. A project never uploads them by itself: OpenCode says
/// nothing when a turn ends, so only `agit save` takes them, and bind records them only to keep
/// its earlier ones out of a plain save.
fn saved_only_candidates(project: &Project) -> Result<Vec<Candidate>> {
    let worktrees = crate::domain::worktree::of(&project.root);
    let mut result = Vec::new();
    for session in crate::adapter::opencode::every_session_in_full()? {
        let Some(cwd) = session
            .cwd
            .and_then(|p| PathBuf::from(p).canonicalize().ok())
        else {
            continue;
        };
        let near = cwd.starts_with(&project.root)
            || worktrees.iter().any(|worktree| cwd.starts_with(worktree));
        if near && includes(project, &cwd)? {
            result.push(Candidate {
                runtime: "opencode".into(),
                session_id: session.id,
                cwd,
            });
        }
    }
    Ok(result)
}

/// The OpenCode conversations a bind without earlier history records, and whether OpenCode's
/// store was read in full. One that cannot be read leaves the policy unmarked, so every OpenCode
/// conversation there keeps counting as earlier ([`opencode_earlier`]) once the store is readable.
fn opencode_history(project: &Project) -> (Vec<Candidate>, bool) {
    match saved_only_candidates(project) {
        Ok(found) => (found, true),
        Err(_) => (Vec::new(), false),
    }
}

/// Whether an OpenCode conversation counts as earlier history of a folder bound without it. One
/// saved into the project's repository is the project's. A policy that never recorded OpenCode's
/// earlier conversations fails closed: every other OpenCode conversation counts as earlier, so a
/// plain save never takes one the person kept out by binding without history.
fn opencode_earlier(project: &Project, session_id: &str) -> bool {
    project.history == History::None
        && !claimed_here(project, "opencode", session_id)
        && (!project.earlier_opencode || project.earlier.contains(&key("opencode", session_id)))
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
    command.args(["--quiet", "--json"]).args(arguments);
    if automatic {
        super::auto_push::configure(&mut command);
    }
    run_child(command, arguments, cwd)
}

/// The folder's own automatic upload of `target`.
fn automatic_push(target: &str, cwd: &Path) -> Result<serde_json::Value> {
    let mut command = crate::infra::background::command(std::env::current_exe()?);
    command.args(["--quiet", "--json"]);
    super::auto_push::automatic_push(&mut command, target);
    run_child(command, &["push".into(), target.into()], cwd)
}

fn run_child(
    mut command: std::process::Command,
    arguments: &[String],
    cwd: &Path,
) -> Result<serde_json::Value> {
    command
        .current_dir(cwd)
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_YES")
        .stdin(Stdio::null());
    let output = command.output()?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("child command did not return a CLI result")?;
    if !(output.status.success() && value["ok"] == true) {
        let messages: Vec<&str> = value["diagnostics"]["stderr"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["level"] == "error")
            .filter_map(|d| d["message"].as_str())
            // Git's own summary of a refused push names only the URL; agit's line says why.
            .filter(|message| !message.starts_with("failed to push some refs"))
            .collect();
        let said = if messages.is_empty() {
            format!(
                "`agit {}` failed",
                arguments.first().map_or("command", String::as_str)
            )
        } else {
            messages.join("; ")
        };
        return Err(anyhow::Error::new(ChildFailed {
            exit_code: output.status.code(),
            message: said,
        }));
    }
    Ok(value)
}

/// A child command that failed, with the category its exit status reports.
#[derive(Debug)]
struct ChildFailed {
    exit_code: Option<i32>,
    /// What the child reported, which is all a person is shown of the failure.
    message: String,
}

impl std::fmt::Display for ChildFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChildFailed {}

/// What a failed push child reported, for its upload's record to class.
fn refusal(error: &anyhow::Error) -> super::upload_outcome::Refusal {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ChildFailed>())
        .map_or_else(super::upload_outcome::Refusal::unexplained, |failed| {
            super::upload_outcome::Refusal::child(failed.exit_code, failed.message.clone())
        })
}

/// Why an automatic upload failed, in words that carry nothing of the error itself. The reason is
/// shown in the conversation, and a Hub response or an error chain can hold URLs with query
/// tokens, header values or paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UploadFailure {
    SignedOut,
    Hub,
    PrivacyIncomplete,
    TooLarge,
    BuildsOnWithdrawn,
    Refused,
    /// The Hub's copy of the branch and this computer's have each moved on without the other.
    Diverged,
    /// The Hub's copy of the branch has moved on, and nothing on this computer is new.
    Behind,
    /// A folder's capture rule keeps the branch from this computer's uploads.
    Withheld,
    /// The publication rules changed while it uploaded, and again on the second try.
    RulesChanged,
    Other,
}

impl UploadFailure {
    fn of(error: &anyhow::Error, project: &Project, candidate: &Candidate) -> Self {
        if error.is::<RulesChanged>() {
            return Self::RulesChanged;
        }
        let reported = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ChildFailed>())
            .map_or(Self::Other, |failed| {
                Self::reported(failed.exit_code, &failed.message)
            });
        if reported == Self::Other && sign_in(project) == SignIn::Absent {
            return Self::SignedOut;
        }
        let branch = Store::open()
            .ok()
            .flatten()
            .and_then(|store| link::get(&store, &candidate.runtime, &candidate.session_id))
            .and_then(|link| link.branch);
        reported.waiting_on(
            Some(candidate.session_id.as_str()),
            branch
                .map(|branch| format!("{}@{branch}", project.repository))
                .as_deref(),
        )
    }

    /// A policy exit is told as a privacy wait only when one is recorded for the conversation
    /// (`session`) or its branch (`target`): that is what `agit status` shows, and a line that
    /// sends the person there for a wait it does not list leaves them nowhere. Every other policy
    /// exit, such as a refusal this class has no name for, is told as [`Self::Other`].
    pub(crate) fn waiting_on(self, session: Option<&str>, target: Option<&str>) -> Self {
        if self != Self::PrivacyIncomplete {
            return self;
        }
        let capture = session
            .is_some_and(|session| crate::domain::privacy::pending::load().contains_key(session));
        let upload = target.is_some_and(|target| {
            crate::domain::privacy::pending::upload::load().contains_key(target)
        });
        if capture || upload { self } else { Self::Other }
    }

    /// The class of a failed `agit push` child, from its exit status and the error it reported.
    pub(crate) fn reported(exit_code: Option<i32>, message: &str) -> Self {
        // Refused again on every turn until the person chooses how to bring the two lines together.
        if message.contains(crate::domain::privacy_git::sessions::DIVERGED) {
            return Self::Diverged;
        }
        // Nothing here to send, and the next turn here is refused as diverged.
        if message.contains(crate::domain::privacy_git::sessions::NOTHING_NEW) {
            return Self::Behind;
        }
        if message.contains(super::push::OVERSIZED) {
            return Self::TooLarge;
        }
        if message.contains(super::push::BUILDS_ON_WITHDRAWN) {
            return Self::BuildsOnWithdrawn;
        }
        if message.contains(super::push::WITHHELD_HERE) {
            return Self::Withheld;
        }
        // A refusal by the Hub is a policy exit too, but no privacy check is missing; a branch
        // name to rename is refused before the Hub is asked.
        if super::push::refused_for_good(message) {
            return Self::Refused;
        }
        match exit_code {
            Some(code) if code == ExitCode::Auth.as_i32() => Self::SignedOut,
            Some(code) if code == ExitCode::Network.as_i32() => Self::Hub,
            Some(code) if code == ExitCode::Policy.as_i32() => Self::PrivacyIncomplete,
            _ => Self::Other,
        }
    }

    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::SignedOut => "agit is signed out; run `agit login` and the next turn uploads it",
            Self::Hub => "the Hub could not be reached or refused it",
            Self::PrivacyIncomplete => {
                "its privacy check did not complete, so nothing was sent; `agit status` says why and what ends the wait"
            }
            Self::TooLarge => {
                "the session is larger than the Hub accepts, so later turns of it stay on this computer; pushing it yourself asks the Hub again"
            }
            Self::BuildsOnWithdrawn => {
                "the session builds on one withdrawn on the Hub, so later turns of it stay on this computer; once that session is restored, pushing it yourself publishes it"
            }
            Self::Refused => "it can't be uploaded as it is, and `agit` in this folder shows why",
            Self::Diverged => {
                "this conversation also continued on another computer, so the Hub has turns this computer doesn't; turns here stay on this computer, and `agit push` for this branch shows how to keep both or merge them"
            }
            Self::Behind => {
                "this conversation also continued on another computer, so the Hub has turns this computer doesn't, and this computer has nothing new to upload; `agit push` for this branch shows how to go on"
            }
            Self::Withheld => {
                "this computer keeps the conversation from uploading, so no later turn sends it either; pushing its branch yourself says why and what to do"
            }
            Self::RulesChanged => {
                "the rules for publishing to this repository changed while it was uploading, so the upload didn't finish; the turn stays saved here, and the next turn or `agit save` uploads what's left"
            }
            // Nothing is known of a refusal without a name of its own, so no retry is promised.
            Self::Other => {
                "it could not be completed on this computer; `agit push` for this branch shows why"
            }
        }
    }
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
        .and_then(|gist| branch_words(project, gist))
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

/// The words of a new branch's name: the opening request as it is published, with every value
/// publication replaces left out, so nothing replaced in the conversation reaches the name.
/// `None` when that can't be checked.
fn branch_words(project: &Project, request: &str) -> Option<String> {
    let redactor = match checkout(project) {
        Ok(repo) => crate::domain::redact::Redactor::try_this_machine()
            .ok()?
            .with_repository(repo.root())
            .ok()?,
        Err(_) => crate::domain::redact::Redactor::try_this_machine().ok()?,
    };
    let published = redactor.branch_name_source(&without_markup(request))?;
    Some(super::front::slug(&published, 40))
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

/// Imports `candidate` into `branch` of the project's repository. A branch the capture opens
/// (`opens`) is marked as a capture by the import and in this machine's list.
fn import_capture(
    project: &Project,
    candidate: &Candidate,
    branch: &str,
    opens: bool,
) -> Result<()> {
    // Without its start record the first collection takes in nothing, which is safe, so filing it
    // never stops the upload.
    if opens && let Ok(primary) = checkout(project) {
        let _ = super::memory::name_start(
            &primary,
            &key(&candidate.runtime, &candidate.session_id),
            branch,
        );
    }
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
        format!("{}@{branch}", project.repository),
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
            branch: branch.to_owned(),
            runtime: candidate.runtime.clone(),
            session: candidate.session_id.clone(),
        })?;
    }
    Ok(())
}

/// Imports and publishes one session; returns its branch and the head the push started from.
/// An `automatic` upload leaves out, once it is imported, a branch the Hub refused for a reason
/// the next turn does not change, and says so with [`LeftOut`].
fn sync_one(
    project: &Project,
    candidate: &Candidate,
    automatic: bool,
) -> Result<(String, Option<String>)> {
    verify(project)?;
    let (branch, opens) = publication(project, candidate)?;
    let target = format!("{}@{branch}", project.repository);
    import_capture(project, candidate, &branch, opens)?;
    landed();
    // The session is claimed, but no turn of it is complete yet; the Stop that completes one
    // uploads it.
    if !has_turns(&checkout(project)?, &branch) {
        return Err(NothingSettled.into());
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
    if automatic && (refused_as_withdrawn(&repo, &branch)? || refused_as_oversized(&repo, &branch)?)
    {
        return Err(LeftOut.into());
    }
    // Read before the push: a commit saved while it runs may not be in it, and is not counted as
    // uploaded.
    let head = repo.git_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}^{{commit}}"),
    ]);
    let pushed = if project.auto_upload {
        super::upload_outcome::recorded(
            super::upload_outcome::Place::of(&project.repository, &branch),
            head.as_deref().unwrap_or_default(),
            || retry_stale(|| automatic_push(&target, &candidate.cwd)),
            |pushed| {
                pushed
                    .as_ref()
                    .err()
                    .map(|error| refusal(error).of_session(&candidate.session_id))
            },
        )
        .map_err(anyhow::Error::new)
        .and_then(|pushed| pushed)
    } else {
        // The person asked for this save: their push, which tells them how it went.
        let args = vec!["push".into(), target, "--yes".into()];
        retry_stale(|| child(&args, &candidate.cwd, false))
    };
    pushed?;
    // A publication does not move the local view of the Hub's branches; fetching does, so local
    // status and links can tell that this branch is on the Hub.
    let _ = child(
        &["fetch".into(), project.repository.clone()],
        &candidate.cwd,
        false,
    );
    Ok((branch, head))
}

/// Runs an upload, and once more, from the start, when it was prepared under publication rules
/// that changed before it was sent: the Hub refuses a preview or a receive whose rules no longer
/// hold, and the upload refuses a receive when this computer's privacy settings changed under it.
/// The second run decides everything again, consent and visibility included, so it never sends
/// under rules the first one was not judged by. A second such refusal is [`RulesChanged`]; any
/// other failure is passed on as it is.
fn retry_stale<T>(mut upload: impl FnMut() -> Result<T>) -> Result<T> {
    let stale = |error: &anyhow::Error| {
        let said = format!("{error:#}");
        said.contains("create a new preview")
            || said.contains(crate::domain::privacy_git::POLICY_CHANGED)
    };
    match upload() {
        Err(error) if stale(&error) => upload().map_err(|again| {
            if stale(&again) {
                RulesChanged.into()
            } else {
                again
            }
        }),
        result => result,
    }
}

/// An upload whose publication rules changed while it ran, twice: on the Hub (who may read it, or
/// what it requires) or on this computer. Parts a staged upload had sent before the refusal stay
/// published; the rest waits on this computer for the next upload.
#[derive(Debug)]
struct RulesChanged;

impl std::fmt::Display for RulesChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(UploadFailure::RulesChanged.describe())
    }
}

impl std::error::Error for RulesChanged {}

/// Whether the branch records any turn. A claim alone leaves its log empty; a branch whose log
/// cannot be read is left for the push to judge.
fn has_turns(repo: &Repo, branch: &str) -> bool {
    repo.git_opt(&["cat-file", "-s", &format!("refs/heads/{branch}:LOG")])
        .and_then(|size| size.trim().parse::<u64>().ok())
        .is_none_or(|size| size > 0)
}

/// A claimed session with no completed turn: nothing to upload yet, and nothing failed.
#[derive(Debug)]
struct NothingSettled;

impl std::fmt::Display for NothingSettled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no turn of this session has finished yet")
    }
}

impl std::error::Error for NothingSettled {}

/// A session automatic upload leaves out on every turn, because the Hub refused its branch for a
/// reason the next turn does not change: withdrawn, or larger than the Hub accepts. The push that
/// met the refusal reported it; later turns are imported and left out quietly, or each would
/// report it again.
#[derive(Debug)]
struct LeftOut;

impl std::fmt::Display for LeftOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("automatic upload leaves this session out")
    }
}

impl std::error::Error for LeftOut {}

fn sync(project: &mut Project) -> CmdResult {
    let mut rows = Vec::new();
    let mut failed = false;
    for candidate in candidates(project)? {
        if project.leaves_out(&key(&candidate.runtime, &candidate.session_id))
            || before_covered(project, &candidate, true).unwrap_or(true)
            || codex_unhooked(project, &candidate)
            || delegation_withheld(&project.repository, &candidate, Some(project), true)
                .unwrap_or(true)
        {
            continue;
        }
        let outcome = match sync_one(project, &candidate, false) {
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
            public_consent,
        } => bind(
            &path,
            &repo,
            history,
            auto_upload,
            dry_run,
            rebind,
            public_consent.as_deref(),
        ),
        Action::List => list(),
        Action::Status { path } => {
            let root = root(&path)?;
            let project = read(&root)?.context("directory has no project policy")?;
            println!("{}", serde_json::to_string(&project)?);
            // Codex captures this directory only through hooks it has approved.
            if project.enabled
                && let Some(problem) =
                    super::setup::codex_hooks(&root).and_then(|hooks| hooks.problem())
            {
                ui::warning(&problem);
            }
            Ok(ExitCode::Ok)
        }
        Action::Unbind { path } => {
            let root = root(&path)?;
            let _guard = lock(&root)?;
            // A git worktree its main checkout covers is stopped on its own: it gets a policy of
            // its own, turned off, and the main checkout and its repository's settings stay.
            let mut project = match read(&root)? {
                Some(project) => project,
                None => match governing(&root)? {
                    // What the person dropped there is in the main checkout's set, and stays
                    // dropped: the whole set comes along, as keys of the main checkout's own
                    // sessions never match one that ran here.
                    Some((project, Via::Worktree(worktree))) if worktree == root => Project {
                        root: root.clone(),
                        history: History::None,
                        earlier: BTreeSet::new(),
                        earlier_opencode: false,
                        last_result: None,
                        // The worktree's own policy replaced nothing; the main checkout keeps its
                        // record.
                        push_auto_before: None,
                        ..project
                    },
                    _ => anyhow::bail!("directory has no project policy"),
                },
            };
            let _repository = repository_lock(&project.repository)?;
            project.enabled = false;
            save(&project)?;
            super::upload_outcome::forget(&project.repository, None)?;
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
                    serde_json::json!({"project":project.root,"candidates":rows,"excluded":project.excluded,"earlier":project.earlier})
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
    public_consent: Option<&str>,
) -> CmdResult {
    let root = root(path)?;
    let (owner, name) = super::parse_slug(repository)?;
    crate::domain::repo::valid_name(&name)?;
    ensure!(
        !repository.contains('@'),
        "project binding requires OWNER/REPO, not a session branch"
    );
    let name = super::checkout_spelling(&owner, &name).unwrap_or(name);
    let repository = format!("{owner}/{name}");
    let repository = repository.as_str();
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
        earlier: BTreeSet::new(),
        earlier_opencode: false,
        last_result: None,
        push_auto_before: None,
    };
    let rows = candidates(&project)?;
    let in_worktrees = rows.iter().filter(|s| !s.cwd.starts_with(&root)).count();
    // A dry run answers whether the bind would go through, so it refuses what the bind refuses.
    assignable(&root, repository, rebind)?;
    if dry_run {
        // Whoever asks the person first (agit's menu bar) learns here whether the destination is
        // public, so it can say so in its question. It is read, never changed; an unreadable
        // audience is null, and the bind itself checks it again.
        let visibility = super::require_login().ok().and_then(|client| {
            let (owner, name) = super::parse_slug(repository).ok()?;
            super::remote_request(client.get_agent(&owner, &name))
                .ok()
                .map(|remote| remote.visibility)
        });
        println!(
            "{}",
            serde_json::json!({
                "directory": root, "repository": repository, "history": history,
                "repository_auto_upload": automatic, "repository_auto_push": automatic,
                "repository_auto_push_scope": automatic.then(|| auto_push_scope(repository)),
                "repository_visibility": visibility,
                "candidates": rows, "candidates_in_worktrees": in_worktrees,
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
        // What the person dropped stays dropped, whatever history the new binding takes.
        project.excluded = existing.excluded;
        if history == History::None {
            project.earlier = existing.earlier;
        }
    }
    project.excluded.extend(kept_drops(&root)?);
    if history == History::None {
        // Earlier sessions stay out whether or not another repository claims them; only a
        // session already claimed in this repository keeps publishing here. Those of the
        // folder's git worktrees stay out by when the worktree was covered, recorded below.
        let store = Store::at(config::store_root()?);
        let (opencode, read) = opencode_history(&project);
        project.earlier_opencode = read;
        project.earlier.extend(
            rows.iter()
                .chain(&opencode)
                .filter(|s| s.cwd.starts_with(&root))
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
    // The Hub finds the repository whatever the case of its name; the binding records the
    // spelling it stores, which its checkout gets.
    ensure!(
        remote.owner == owner && remote.name.eq_ignore_ascii_case(&name),
        "the Hub answered for {repository} with {}/{}",
        remote.owner,
        remote.name
    );
    let repository = format!("{}/{}", remote.owner, remote.name);
    let repository = repository.as_str();
    project.repository = repository.into();
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
    let public_answer = if automatic && remote.visibility == "public" {
        Some(super::push::answer_public_switch_on(
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
            public_consent,
        )?)
    } else {
        None
    };
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
        if remote.visibility == "public" {
            "public: anyone can read it"
        } else {
            remote.visibility.as_str()
        }
    );
    println!(
        "History: {history:?}; candidates: {} session{}{}; automatic upload: {automatic}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        if in_worktrees > 0 {
            format!(", {in_worktrees} of them in git worktrees of this folder")
        } else {
            String::new()
        }
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
        if public_answer == Some(super::push::PublicAnswer::Unattended) {
            super::push::record_unattended(repository)?;
        }
        repository_guard = Some(repository_lock(repository)?);
        enable_auto_push(&mut project, &repo)?;
    }
    workspace::bind(&root, repository, rebind)?;
    save(&project)?;
    // The new policy holds what a move released; the record of it has done its work.
    match fs::remove_file(state_path(&root)?.with_extension("moved-drops")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    drop(repository_guard);
    record_audience(&root, &audience)?;
    // Binding starts the folder's coverage of its git worktrees afresh: with its history, from
    // the beginning, so the sync below takes their sessions too; without, from now.
    let since = match history {
        History::All => chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        History::None => chrono::Utc::now(),
    };
    let worktrees = worktrees_of(&project);
    update_covered(&root, |records| {
        *records = worktrees
            .into_iter()
            .map(|worktree| {
                (
                    worktree,
                    Covered {
                        since,
                        opener: None,
                    },
                )
            })
            .collect();
    })?;
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
        let mut paths: Vec<PathBuf> = ["json", "caught", "catch-up", "worktrees", "moved-drops"]
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
    let released = read(root)?;
    // What the person dropped here stays dropped in the repository the folder moves to: the
    // binding that completes the move takes these over (#266). A move that is undone puts the
    // policy back and removes this record with the other routing files.
    if let Some(drops) = released.as_ref().map(|project| &project.excluded)
        && !drops.is_empty()
    {
        let mut file = tempfile::NamedTempFile::new_in(home()?)?;
        file.write_all(&serde_json::to_vec(drops)?)?;
        file.as_file().sync_all()?;
        file.persist(state_path(root)?.with_extension("moved-drops"))
            .map_err(|e| e.error)?;
    }
    let policy = released.map(|project| project.repository);
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

/// The directory of the project policy that covers `path` from a folder above it, paused or not.
/// A git worktree is its own folder, whichever main checkout covers it.
pub(crate) fn covering_root(path: &Path) -> Option<PathBuf> {
    let project = nearest(path).ok().flatten()?;
    within(&project, path).ok()?.then_some(project.root)
}

/// The nearest directory at or above `path` that is bound on its own, by a project policy or a
/// workspace binding. Its sessions belong to that binding, not to a folder above it.
pub(crate) fn bound_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| read(dir).ok().flatten().is_some() || workspace::read(dir).is_some())
        .map(Path::to_path_buf)
}

/// Whether the transcript of a session the project covering `cwd` uploaded changed after that
/// upload, as when its Stop hook did not run: the Hub lacks the newer turns although the branch is
/// still what was pushed. A session uploaded before such records were kept counts by its branch.
/// Listings call this, so a transcript in a runtime's cold storage is not restored: one that was
/// moved there is not being written to.
pub(crate) fn changed_since_upload(runtime: &str, session_id: &str, cwd: &Path) -> bool {
    let Ok(Some((project, _))) = governing(cwd) else {
        return false;
    };
    let Some(&uploaded) = caught(&project.root).get(&key(runtime, session_id)) else {
        return false;
    };
    let length = caught_lengths(&project.root)
        .get(&key(runtime, session_id))
        .copied();
    adapter::get(runtime)
        .ok()
        .and_then(|adapter| adapter.resolve_present(session_id, Some(cwd)))
        .is_some_and(|path| changed_after(runtime, &path, uploaded, length))
}

/// Whether the transcript at `path` gained more than the runtime's own bookkeeping after the
/// upload that saw it at `modified` and `length` bytes. A runtime writes such records after a
/// turn's Stop hook has uploaded it (hook and duration summaries), and they are not a turn.
fn changed_after(runtime: &str, path: &Path, modified: u64, length: Option<u64>) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let now = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_secs());
    if now == Some(modified) {
        return false;
    }
    let Some(length) = length.filter(|length| *length <= metadata.len()) else {
        return true;
    };
    let (Ok(adapter), Ok(mut file)) = (adapter::get(runtime), fs::File::open(path)) else {
        return true;
    };
    let mut added = String::new();
    if std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(length)).is_err()
        || std::io::Read::read_to_string(&mut file, &mut added).is_err()
    {
        return true;
    }
    // The part before is read only when a copy of a prompt has to be found in it, from its end:
    // the whole records within `window` bytes of it first.
    let settled = |window: Option<usize>| -> crate::Result<std::borrow::Cow<'static, str>> {
        // From the byte before the window, so a record that starts right at it is whole.
        let start = window
            .and_then(|window| length.checked_sub(window as u64 + 1))
            .unwrap_or(0);
        let mut file = fs::File::open(path)?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start))?;
        let mut settled = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(file, length - start), &mut settled)?;
        if start > 0 {
            let whole = settled
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(settled.len(), |newline| newline + 1);
            settled.drain(..whole);
        }
        Ok(String::from_utf8(settled)?.into())
    };
    // Judged as settlement judges it: what it would leave behind is not new.
    !matches!(
        adapter::judge_tail(adapter.as_ref(), settled, &added, &added),
        Ok(adapter::Tail::Settled)
    )
}

/// Whether the project policy deciding for the folder at `root` was turned off (`agit off`,
/// `project unbind`).
/// Whether no project has ever governed `root`: it has no policy of its own and no folder above
/// it, or main checkout of a git worktree it is, decides for it. A paused or turned-off folder
/// keeps its policy, so it is not one.
pub(crate) fn never_bound(root: &Path) -> bool {
    matches!(read(&policy_root(root)), Ok(None)) && matches!(governing(root), Ok(None))
}

/// Where this machine records what the person was asked about turning `root` on: `offered` once
/// a push suggested it, `declined` once they said no. Kept beside the policies, keyed as a policy
/// would be, and never read as one.
fn offer_path(root: &Path) -> Result<PathBuf> {
    Ok(state_path(root)?.with_extension("offer"))
}

/// Whether a push has suggested turning `root` on, or the person declined it.
pub(crate) fn automatic_upload_offered(root: &Path) -> bool {
    offer_path(root).is_ok_and(|path| path.exists())
}

/// Whether the person declined turning `root` on. Nothing suggests it there again.
pub(crate) fn automatic_upload_declined(root: &Path) -> bool {
    offer_path(root)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|said| said.trim() == "declined")
}

/// Takes the one chance to suggest turning `root` on: `true` only for the caller that records
/// the offer. The check and the record are one step under the folder's offer lock, so of two
/// pushes that both found the offer due, one says it, and a no recorded while a push waited on
/// the Hub is never asked again.
pub(crate) fn claim_automatic_upload_offer(root: &Path) -> Result<bool> {
    let _guard = offer_lock(root)?;
    if !never_bound(root) || automatic_upload_offered(root) {
        return Ok(false);
    }
    write_offer(root, "offered")?;
    Ok(true)
}

/// Records that the person declined turning `root` on. It changes nothing else: a binding, a
/// paused folder and its local saving stay as they were.
pub(crate) fn record_automatic_upload_declined(root: &Path) -> Result<()> {
    let _guard = offer_lock(root)?;
    write_offer(root, "declined")
}

/// Forgets what the person was asked about `root`, once it is on.
pub(crate) fn forget_automatic_upload_offer(root: &Path) {
    let Ok(_guard) = offer_lock(root) else {
        return;
    };
    if let Ok(path) = offer_path(root) {
        let _ = fs::remove_file(path);
    }
}

fn offer_lock(root: &Path) -> Result<fs::File> {
    lock_at(
        state_path(root)?.with_extension("offer-lock"),
        "the folder's offer record lock",
    )
}

fn write_offer(root: &Path, said: &str) -> Result<()> {
    let home = home()?;
    config::create_state_dir(&home)?;
    let mut file = tempfile::NamedTempFile::new_in(&home)?;
    file.write_all(said.as_bytes())?;
    file.persist(offer_path(root)?).map_err(|e| e.error)?;
    Ok(())
}

pub(crate) fn turned_off(root: &Path) -> bool {
    read(&policy_root(root))
        .ok()
        .flatten()
        .is_some_and(|project| !project.enabled)
}

/// Whether the session, in `cwd`, is one the folder's binding left out until the person lets it
/// in with `agit save --all`: from before the folder was turned on without its history, or a
/// session of a covered git worktree from before the worktree was covered.
pub(crate) fn left_out(folder: &Path, runtime: &str, session_id: &str, cwd: &Path) -> bool {
    let Some(project) = read(&policy_root(folder)).ok().flatten() else {
        return false;
    };
    let candidate = Candidate {
        runtime: runtime.into(),
        session_id: session_id.into(),
        cwd: cwd.to_path_buf(),
    };
    if runtime == "opencode" {
        return opencode_earlier(&project, session_id);
    }
    project.earlier.contains(&key(runtime, session_id))
        || before_covered(&project, &candidate, false).unwrap_or(true)
        || codex_unhooked(&project, &candidate)
}

/// Whether the person stopped uploading this session from the folder at `root`, or from the main
/// checkout whose policy covers it as a git worktree (`agit project drop`). Only a save that names
/// it lets it back in.
pub(crate) fn is_dropped(root: &Path, runtime: &str, session_id: &str) -> bool {
    read(&policy_root(root))
        .ok()
        .flatten()
        .is_some_and(|project| project.excluded.contains(&key(runtime, session_id)))
}

/// The repository of the project policy that covers `path`, paused or not.
pub(crate) fn policy_repository(path: &Path) -> Option<String> {
    governing(path).ok()?.map(|(project, _)| project.repository)
}

/// Whether the project policy covering `path` uploads automatically, without network access.
pub(crate) fn uploads_automatically(path: &Path) -> Option<bool> {
    governing(path)
        .ok()?
        .map(|(project, _)| project.enabled && project.auto_upload)
}

/// In a git worktree an on folder covers, the line saying which folder's setting it follows and
/// where its new sessions go; `None` anywhere else.
pub(crate) fn worktree_notice(path: &Path) -> Option<String> {
    let (project, Via::Worktree(worktree)) = governing(path).ok()?? else {
        return None;
    };
    (project.enabled && project.auto_upload).then(|| {
        format!(
            "{} is a git worktree of {}, which uploads new sessions to {}; to stop here: `agit off`",
            ui::tilde(&worktree),
            ui::tilde(&project.root),
            project.repository
        )
    })
}

/// The last automatic upload's error, when it failed.
pub(crate) fn last_failure(path: &Path) -> Option<String> {
    let (project, _) = governing(path).ok()??;
    let last = project.last_result?;
    (last["ok"] == false).then(|| last["error"].as_str().unwrap_or("unknown error").to_owned())
}

/// Sessions under `root` that the policy left out when it was bound without earlier history and
/// that no repository holds yet.
pub(crate) fn earlier_count(root: &Path) -> usize {
    let Some(project) = read(root).ok().flatten() else {
        return 0;
    };
    let store = Store::open().ok().flatten();
    project
        .earlier
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

/// Whether a project captures this runtime's conversations by itself: only the runtimes whose
/// hooks call [`capture`]. Any other runtime's conversation is saved only when someone imports it,
/// so telling its agent the project saves it would leave it unsaved.
fn captured_runtime(runtime: &str) -> bool {
    matches!(runtime, "codex" | "claude-code")
}

/// Exact hook identity remains authoritative; a project policy never overrides another claim.
pub(crate) fn capture(runtime: &str, session: &str, cwd: Option<&str>) -> Result<bool> {
    let Some(cwd) = cwd.and_then(|p| Path::new(p).canonicalize().ok()) else {
        return Ok(false);
    };
    if !captured_runtime(runtime) {
        return Ok(false);
    }
    let Some((project, _)) = governing(&cwd)? else {
        return Ok(false);
    };
    if runtime == "codex" {
        codex_hooks_seen(session);
    }
    let candidate = Candidate {
        runtime: runtime.into(),
        session_id: session.into(),
        cwd,
    };
    if delegation_withheld(&project.repository, &candidate, Some(&project), true).unwrap_or(true) {
        return Ok(true);
    }
    // A session claimed elsewhere is left to its source repository.
    let branch = match placement(&project, &candidate) {
        Ok(Placement::Claim(branch)) => Some(branch),
        Ok(Placement::New) => None,
        _ => return Ok(false),
    };
    if withholds(&project, &candidate, true) {
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

/// A captured header remains local evidence after its native file is removed. Ordinary legacy
/// claims without a header keep their existing behavior; a known delegate still needs its parent.
fn delegation_header(session: &str) -> Result<Option<serde_json::Value>> {
    if let Ok(path) = crate::adapter::codex::resolve_readonly(session) {
        let bytes = crate::adapter::codex_rollout::first_line(
            &path,
            crate::adapter::codex_rollout::HEADER_LIMIT,
        )?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let header: serde_json::Value = serde_json::from_slice(&bytes)?;
        return Ok((header["type"] == "session_meta").then_some(header));
    }
    let Some(claim) = link::get_checked(&Store::at(config::store_root()?), "codex", session)?
    else {
        return Ok(None);
    };
    let (Some(owner), Some(agent), Some(branch)) = (claim.owner, claim.agent, claim.branch) else {
        return Ok(None);
    };
    let (owner, agent) = super::parse_slug(&format!("{owner}/{agent}"))?;
    let Some(repo) = Repo::open(config::repo_dir(&owner, &agent)?) else {
        return Ok(None);
    };
    let Some(line) = crate::domain::storage::materialize_head_at(
        repo.root(),
        &format!("refs/heads/{branch}"),
        "LOG",
    )?
    else {
        return Ok(None);
    };
    let header = crate::domain::storage::parse_envelope_line(&line)?.content;
    Ok((header["type"] == "session_meta" && header["payload"]["id"] == session).then_some(header))
}

/// A delegated conversation cannot acquire capture authority its ancestors do not have.
/// Inspect each parent rather than caching a verdict: a later drop must cover existing children.
fn delegation_withheld(
    repository: &str,
    candidate: &Candidate,
    fallback: Option<&Project>,
    adopt: bool,
) -> Result<bool> {
    if candidate.runtime != "codex" {
        return Ok(false);
    }
    let mut session = candidate.session_id.clone();
    let mut seen = BTreeSet::new();
    let records = withheld_records()?;
    loop {
        ensure!(
            seen.len() < 64 && seen.insert(session.clone()),
            "invalid delegated ancestry"
        );
        let Some(header) = delegation_header(&session)? else {
            ensure!(seen.len() == 1, "delegated ancestry is unavailable");
            return Ok(false);
        };
        let payload = &header["payload"];
        let spawn = &payload["source"]["subagent"]["thread_spawn"];
        if !crate::adapter::codex::delegated_header(&header) {
            return Ok(false);
        }
        ensure!(payload["id"] == session, "delegated identity changed");
        let parents: Vec<_> = [
            spawn["parent_thread_id"].as_str(),
            payload["parent_thread_id"].as_str(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let parent = parents
            .first()
            .copied()
            .or_else(|| payload["forked_from_id"].as_str())
            .context("delegated parent is missing")?;
        ensure!(
            uuid::Uuid::parse_str(parent).is_ok() && parents.iter().all(|id| *id == parent),
            "ambiguous delegated parent"
        );
        let parent_key = key("codex", parent);
        if records
            .iter()
            .any(|record| record.session.as_deref() == Some(&parent_key))
        {
            return Ok(true);
        }
        let parent_header =
            delegation_header(parent)?.context("delegated parent is unavailable")?;
        ensure!(
            parent_header["payload"]
                .get("id")
                .is_none_or(|id| id.as_str() == Some(parent)),
            "delegated parent identity changed"
        );
        link::get_checked(&Store::at(config::store_root()?), "codex", parent)?;
        let cwd = parent_header["payload"]["cwd"]
            .as_str()
            .and_then(|cwd| PathBuf::from(cwd).canonicalize().ok())
            .context("delegated parent workspace is unavailable")?;
        let ancestor = Candidate {
            runtime: "codex".into(),
            session_id: parent.into(),
            cwd,
        };
        let governing = governing(&ancestor.cwd)?.map(|(project, _)| project);
        let policy = governing.as_ref().or(fallback);
        if let Some(policy) = policy {
            if policy.repository != repository
                || !policy.enabled
                || !policy.auto_upload
                || !includes(policy, &ancestor.cwd)?
                || policy.leaves_out(&parent_key)
                || before_covered(policy, &ancestor, adopt)?
                || !matches!(
                    placement(policy, &ancestor),
                    Ok(Placement::New | Placement::Claim(_))
                )
            {
                return Ok(true);
            }
        } else if let Some(claim) =
            link::get_checked(&Store::at(config::store_root()?), "codex", parent)?
            && claim
                .owner
                .as_deref()
                .zip(claim.agent.as_deref())
                .is_some_and(|(owner, name)| format!("{owner}/{name}") != repository)
        {
            return Ok(true);
        }
        session = parent.into();
    }
}

/// Whether `project` keeps `candidate`'s turns from being uploaded: the project is paused, it
/// uploads nothing, it leaves the session out (dropped, or from before the folder was on), or the
/// session began in one of the folder's git worktrees before the folder covered it. `adopt`
/// records a worktree seen for the first time as covered from now.
fn withholds(project: &Project, candidate: &Candidate, adopt: bool) -> bool {
    !project.enabled
        || !project.auto_upload
        || project.leaves_out(&key(&candidate.runtime, &candidate.session_id))
        || before_covered(project, candidate, adopt).unwrap_or(true)
        || delegation_withheld(&project.repository, candidate, Some(project), adopt).unwrap_or(true)
}

/// What the project policy of a folder does with one live session. Only team activity reports
/// of terminal sessions registered for native control ask, and those run on Unix alone.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Capture {
    /// No project policy covers the folder.
    Unbound,
    /// A policy covers the folder and does not upload this session's turns: it is paused or
    /// uploads nothing, the session is excluded or claimed by another repository, a nearer
    /// binding interposes, or the policy cannot be read.
    Withheld,
    /// The policy uploads the session's turns to this repository.
    Uploads(RemoteIdentity),
}

/// What the project policy covering `cwd`, a canonical directory, does with `runtime`'s
/// `session`. It answers with the same tests a Stop hook's capture applies, without capturing.
#[cfg(unix)]
pub(crate) fn capture_state(runtime: &str, session: &str, cwd: &Path) -> Capture {
    let state = || -> Result<Capture> {
        let project = match governing(cwd)? {
            Some((project, _)) => project,
            // A folder below a policy that does not cover it is withheld, not unbound.
            None if nearest(cwd)?.is_some() => return Ok(Capture::Withheld),
            None => return Ok(Capture::Unbound),
        };
        let candidate = Candidate {
            runtime: runtime.into(),
            session_id: session.into(),
            cwd: cwd.to_owned(),
        };
        // `placement` reads a claim it cannot read as no claim, which suits capture, whose
        // import refuses it later. A report cannot wait for that: a claim that exists but cannot
        // be read leaves the session's owner unconfirmed, so it withholds.
        link::get_checked(&Store::at(config::store_root()?), runtime, session)?;
        if withholds(&project, &candidate, false)
            || !matches!(
                placement(&project, &candidate)?,
                Placement::Claim(_) | Placement::New
            )
        {
            return Ok(Capture::Withheld);
        }
        Ok(Capture::Uploads(project.identity))
    };
    state().unwrap_or(Capture::Withheld)
}

/// The folders bound at `path` or above it, by a project policy or a workspace binding, either
/// of which [`includes`] stops at. A binding file that exists counts even when it cannot be read.
#[cfg(unix)]
fn bound_at(path: &Path) -> Result<Vec<PathBuf>> {
    let mut bound = Vec::new();
    for ancestor in path.ancestors() {
        if read(ancestor)?.is_some() || workspace::path_for(ancestor)?.try_exists()? {
            bound.push(ancestor.to_owned());
        }
    }
    Ok(bound)
}

/// The policy of the one folder binding that covers `cwd`, a canonical directory. `spelled`, the
/// folder as the session names it, must lie under that binding alone too. A folder under two
/// bindings, nested or not, or one whose spelling reaches other bindings, as through a symlink,
/// has none; so has a folder whose one binding records no policy.
#[cfg(unix)]
fn sole_binding(cwd: &Path, spelled: &Path) -> Result<Option<Project>> {
    let bound = bound_at(cwd)?;
    let [root] = bound.as_slice() else {
        return Ok(None);
    };
    if bound_at(spelled)? != bound {
        return Ok(None);
    }
    read(root)
}

/// The repository agit uploads `runtime`'s `session` in `cwd` to when that is certain: one
/// folder binding covers it ([`sole_binding`]), its policy uploads the session now
/// ([`capture_state`]), and the repository passes the check an upload makes ([`verify`]).
/// It comes with the id of the account the binding uploads as, if the policy records one.
/// Anything less certain, or an error, answers `None`.
#[cfg(unix)]
pub(crate) fn sole_upload(
    runtime: &str,
    session: &str,
    cwd: &Path,
    spelled: &Path,
) -> Option<(RemoteIdentity, Option<String>)> {
    let upload = || -> Result<Option<(RemoteIdentity, Option<String>)>> {
        let Some(project) = sole_binding(cwd, spelled)? else {
            return Ok(None);
        };
        let Capture::Uploads(identity) = capture_state(runtime, session, cwd) else {
            return Ok(None);
        };
        verify(&project)?;
        Ok((identity == project.identity).then_some((identity, project.account_id)))
    };
    upload().ok().flatten()
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
/// The session is marked as being captured before its marker goes, so it shows as still to save
/// at every moment until its capture ends.
fn take_pending(root: &Path) -> Result<Option<(Candidate, Capturing)>> {
    for path in markers(root)? {
        let bytes = fs::read(&path)?;
        let Ok(candidate) = serde_json::from_slice::<Candidate>(&bytes) else {
            fs::remove_file(&path)?;
            continue;
        };
        let capturing = Capturing::mark(root, &candidate);
        fs::remove_file(&path)?;
        return Ok(Some((candidate, capturing)));
    }
    Ok(None)
}

/// The session a project's worker is capturing, and that worker's process id, so a record left by
/// a worker that ended mid-capture is told apart from one still capturing.
#[derive(Serialize, Deserialize)]
struct CapturingRecord {
    session: String,
    pid: u32,
}

/// Marks a session as being captured until its turn lands on its branch here, or until dropped.
struct Capturing(Option<PathBuf>);

/// The mark of the capture this process runs now. It is released as soon as the capture's turn
/// is on its branch, before the upload: the upload may take long, and the end of a session that
/// waited for it would call a saved turn still being saved.
static CAPTURING: std::sync::Mutex<Option<Capturing>> = std::sync::Mutex::new(None);

/// Releases the mark of this process's running capture, its turn being on its branch now.
fn landed() {
    let mark = CAPTURING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(mark);
}

impl Capturing {
    fn path(root: &Path) -> Result<PathBuf> {
        Ok(state_path(root)?.with_extension("capturing"))
    }

    fn mark(root: &Path, candidate: &Candidate) -> Self {
        let record = CapturingRecord {
            session: hash(&key(&candidate.runtime, &candidate.session_id)),
            pid: std::process::id(),
        };
        let written = Self::path(root).ok().filter(|path| {
            serde_json::to_vec(&record).is_ok_and(|bytes| {
                tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))
                    .and_then(|mut file| file.write_all(&bytes).map(|()| file))
                    .is_ok_and(|file| file.persist(path).is_ok())
            })
        });
        Self(written)
    }
}

impl Drop for Capturing {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Whether the project that saves the folder at `cwd` has `runtime`'s `session` still to save:
/// queued for its worker, or being captured by a worker that is still running. A Stop in such a
/// folder hands the turn to the worker and returns, so until the worker is done nothing else shows
/// that the turn is not saved yet.
pub(crate) fn capture_pending(runtime: &str, session: &str, cwd: &Path) -> bool {
    let Ok(Some((project, _))) = governing(cwd) else {
        return false;
    };
    let name = hash(&key(runtime, session));
    if pending_dir(&project.root).is_ok_and(|pending| pending.join(&name).exists()) {
        return true;
    }
    Capturing::path(&project.root)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<CapturingRecord>(&bytes).ok())
        .is_some_and(|record| record.session == name && super::upload_outcome::alive(record.pid))
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
                if !before_covered(&project, &candidate, true).unwrap_or(true)
                    && !codex_unhooked(&project, &candidate)
                    && behind(&project, &candidate, &caught)
                {
                    enqueue(root, &candidate)?;
                }
            }
        }
        while let Some((candidate, capturing)) = take_pending(root)? {
            *CAPTURING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(capturing);
            let Some(project) = read(root)? else {
                landed();
                return Ok(());
            };
            // A failure is recorded in the project's last result; the remaining sessions go on.
            let _ = capture_candidate(project, candidate, false, true);
            landed();
        }
        drop(worker);
        if markers(root)?.is_empty() && !catch_up_requested(root)? {
            return Ok(());
        }
    }
}

/// Uploads a queued or saved session. `let_in` says a save lets in the sessions of the folder's
/// git worktrees from before the folder covered them; automatic capture never does. `automatic`
/// says no person asked for this upload.
fn capture_candidate(
    mut project: Project,
    candidate: Candidate,
    let_in: bool,
    automatic: bool,
) -> Result<bool> {
    // Automatic capture leaves foreign claims to their source repository's settlement.
    if !matches!(
        placement(&project, &candidate),
        Ok(Placement::Claim(_) | Placement::New)
    ) {
        return Ok(false);
    }
    let session_key = key(&candidate.runtime, &candidate.session_id);
    let held = |project: &Project| {
        !project.enabled
            || !project.auto_upload
            || project.leaves_out(&session_key)
            || (!let_in && before_covered(project, &candidate, true).unwrap_or(true))
            || delegation_withheld(&project.repository, &candidate, Some(project), true)
                .unwrap_or(true)
    };
    if held(&project) {
        return Ok(true);
    }
    let _guard = lock(&project.root)?;
    project = read(&project.root)?.context("project policy disappeared")?;
    if held(&project) {
        return Ok(true);
    }
    // A queued capture outlives its Stop: a directory bound on its own in between belongs to
    // that binding's history and upload choices, not to this project's.
    if !includes(&project, &candidate.cwd)? {
        return Ok(false);
    }
    let modified = transcript_modified(&candidate);
    let length = transcript_length(&candidate);
    let result = sync_one(&project, &candidate, automatic);
    if result
        .as_ref()
        .is_err_and(|e| e.is::<NothingSettled>() || e.is::<LeftOut>())
    {
        return Ok(true);
    }
    if result.is_ok()
        && let Some(modified) = modified
    {
        let _ = record_caught(&project.root, &candidate, modified);
        if let Some(length) = length {
            let _ = record_caught_length(&project.root, &candidate, length);
        }
    }
    let reason = result
        .as_ref()
        .err()
        .map(|error| UploadFailure::of(error, &project, &candidate));
    project.last_result = Some(serde_json::json!({
        "at": chrono::Utc::now(), "session_id": candidate.session_id, "ok": result.is_ok(),
        "error": result.as_ref().err().map(|e| format!("{e:#}")),
        "reason": reason,
        "head": result.as_ref().ok().and_then(|(_, head)| head.as_deref()),
    }));
    save(&project)?;
    result?;
    Ok(true)
}

/// What an explicit save did with one session.
pub(crate) enum Saved {
    /// On the Hub, on this branch.
    Uploaded(String),
    /// Claimed on this branch; its first turn has not finished, and the Stop that finishes it
    /// uploads it.
    Pending(String),
    /// Left where it was, with the reason; saving again may get it through.
    Failed(String),
    /// Left where it was for a reason saving again does not change; the reason says what does.
    Refused(String),
}

impl Saved {
    /// A failed upload, told apart by whether the same upload meets the same refusal.
    fn upload_failed(message: String) -> Self {
        if super::push::refused_for_good(&message) {
            Self::Refused(message)
        } else {
            Self::Failed(message)
        }
    }
}

/// Uploads these sessions of the project deciding for the folder at `root` now, after any
/// automatic upload already running there. With `earlier`, sessions the binding left out are let
/// in first. A session the person dropped is let in only when `named`: the person asked for that
/// one session, and not for every earlier one.
pub(crate) fn save_now(
    root: &Path,
    sessions: &[(String, String, PathBuf)],
    earlier: bool,
    named: bool,
) -> Result<Vec<Saved>> {
    let root = &policy_root(root);
    let worker = wait_for_worker(root)?;
    let mut saved = Vec::new();
    for (runtime, session_id, cwd) in sessions {
        let candidate = Candidate {
            runtime: runtime.clone(),
            session_id: session_id.clone(),
            cwd: cwd.clone(),
        };
        let session_key = key(runtime, session_id);
        if earlier || named {
            let _guard = lock(root)?;
            let mut project = read(root)?.context("folder is not set up")?;
            let mut changed = project.earlier.remove(&session_key);
            if named {
                changed |= project.excluded.remove(&session_key);
            }
            if changed {
                save(&project)?;
            }
        }
        let project = read(root)?.context("folder is not set up")?;
        // `--all` and `--today` ask for earlier sessions, never for one the person dropped.
        if project.excluded.contains(&session_key) {
            saved.push(Saved::Refused(format!(
                "you stopped uploading it from this folder (`agit project drop`); to upload it anyway, name it: `agit save {session_id}`"
            )));
            continue;
        }
        let outcome = match save_one(project, candidate, earlier || named) {
            Ok(outcome) => outcome,
            Err(error) => Saved::upload_failed(format!("{error:#}")),
        };
        // A conversation the person saved themselves no longer stays out of the folder's uploads
        // because it predates `agit rc start --here` (see `rc::folder_cutoff`).
        if (earlier || named) && matches!(outcome, Saved::Uploaded(_) | Saved::Pending(_)) {
            crate::rc::folder_cutoff::admit(cwd, runtime, session_id)?;
        }
        saved.push(outcome);
    }
    // A Stop that arrived meanwhile queued its session and left it to the lock holder.
    drop(worker);
    if !markers(root)?.is_empty() || catch_up_requested(root)? {
        let _ = spawn_capture(root);
    }
    Ok(saved)
}

/// Why a folder below the project's that is bound on its own keeps this session.
fn bound_apart(project: &Project, candidate: &Candidate) -> Result<Option<String>> {
    if includes(project, &candidate.cwd)? {
        return Ok(None);
    }
    let folder = bound_root(&candidate.cwd).unwrap_or_else(|| candidate.cwd.clone());
    Ok(Some(format!(
        "it ran in {}, which is set up on its own; run `agit save` there",
        ui::tilde(&folder)
    )))
}

/// Uploads one session for a save. `let_in` says the save lets in sessions of the folder's git
/// worktrees from before they were covered.
fn save_one(project: Project, candidate: Candidate, let_in: bool) -> Result<Saved> {
    if delegation_withheld(&project.repository, &candidate, Some(&project), false)? {
        return Ok(Saved::Refused(
            "the root conversation's capture policy keeps this delegated session out".into(),
        ));
    }
    if let Some(why) = bound_apart(&project, &candidate)? {
        return Ok(Saved::Refused(why));
    }
    let finished = adapter::get(&candidate.runtime)
        .ok()
        .and_then(|adapter| {
            let path = adapter.resolve(&candidate.session_id, Some(&candidate.cwd))?;
            adapter.parse_at(&path).ok()
        })
        .is_some_and(|session| crate::domain::turn::completed_count(&session) > 0);
    if let Placement::Foreign(source) = placement(&project, &candidate)? {
        return Ok(Saved::Refused(format!("it is saved in {source}")));
    }
    if !finished {
        let _guard = lock(&project.root)?;
        // A folder below may have been set up on its own since the session was picked; the
        // claim must not take a session that binding now keeps.
        let project = read(&project.root)?.context("folder is not set up")?;
        if let Some(why) = bound_apart(&project, &candidate)? {
            return Ok(Saved::Refused(why));
        }
        verify(&project)?;
        let (branch, opens) = publication(&project, &candidate)?;
        import_capture(&project, &candidate, &branch, opens)?;
        return Ok(Saved::Pending(branch));
    }
    let store = Store::at(config::store_root()?);
    if let Placement::Foreign(source) = placement(&project, &candidate)? {
        return Ok(Saved::Refused(format!("it is saved in {source}")));
    }
    if !capture_candidate(project.clone(), candidate.clone(), let_in, false)? {
        // The capture rechecked under the lock and left the session where it now belongs.
        let why = bound_apart(&read(&project.root)?.unwrap_or(project), &candidate)?;
        return Ok(Saved::Refused(
            why.unwrap_or_else(|| "it is saved in another repository".into()),
        ));
    }
    if let Some(link) = link::get(&store, &candidate.runtime, &candidate.session_id)
        && let Some(branch) = link.branch.clone()
        && checkout(&project).is_ok_and(|repo| !has_turns(&repo, &branch))
    {
        return Ok(Saved::Pending(branch));
    }
    let after = read(&project.root)?.context("folder is not set up")?;
    if let Some(last) = &after.last_result
        && last["session_id"] == candidate.session_id.as_str()
        && last["ok"] == false
    {
        return Ok(Saved::upload_failed(
            last["error"].as_str().unwrap_or("unknown error").to_owned(),
        ));
    }
    let branch = link::get(&store, &candidate.runtime, &candidate.session_id)
        .and_then(|link| link.branch)
        .context("the session was not saved")?;
    Ok(Saved::Uploaded(branch))
}

/// The capture worker lock, waiting for a worker that holds it.
fn wait_for_worker(root: &Path) -> Result<fs::File> {
    config::create_state_dir(&home()?)?;
    let file = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_path(root)?.with_extension("capture"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

/// Queues a catch-up of the project covering `path`: every session of it with turns the Hub does
/// not have yet, which a Stop hook that did not run (an untrusted hook, a crash, no network) left
/// behind. The worker does the work; this returns at once.
pub(crate) fn catch_up(path: &Path) -> Result<bool> {
    let Some((project, _)) = governing(path)? else {
        return Ok(false);
    };
    if !project.enabled || !project.auto_upload {
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

/// The folders this machine's project policies set up, paused ones included; a policy that cannot
/// be read is left out.
pub(crate) fn policy_roots() -> Vec<PathBuf> {
    let Ok(entries) = home().and_then(|home| Ok(fs::read_dir(home)?)) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| is_policy(path))
        .filter_map(|path| {
            let project: Project = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
            Some(project.root)
        })
        .collect()
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
    let excluded = |session: String| projects.iter().any(|p| p.leaves_out(&session));
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
        let all_links = match &links {
            Some(links) => links,
            None => links.insert(link::list(&Store::at(config::store_root()?))),
        };
        let branch_claims = recording_into(all_links, &owner, &agent, branch);
        if branch_claims.iter().any(|claim| {
            let candidate = Candidate {
                runtime: claim.source.clone(),
                session_id: claim.session_id.clone(),
                cwd: claim.cwd.as_deref().map(PathBuf::from).unwrap_or_default(),
            };
            delegation_withheld(slug, &candidate, None, false).unwrap_or(true)
        }) {
            verdicts.push(Some(Withholding::Excluded));
            continue;
        }
        if projects.is_empty() {
            verdicts.push(None);
            continue;
        }
        // An excluded session's capture branch stays withheld after its claim is gone.
        let excluded_branch = projects
            .iter()
            .flat_map(|p| p.excluded.iter().chain(&p.earlier))
            .any(|session| {
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

/// Which kind of exclusion keeps a branch from being published, and the session to name to let
/// it in where naming it does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Exclusion {
    /// The session ran in `folder` before agit was on there.
    Earlier { folder: PathBuf, session: String },
    /// The person stopped uploading the session from `folder`.
    Dropped { folder: PathBuf, session: String },
    /// The conversation that started this delegated session keeps it out.
    Delegated,
}

/// What [`Withholding::Excluded`] stands for on `slug@branch`, read the way [`withheld_all`]
/// decides it, so a refusal can say which and how to get past it. `None` when no exclusion is
/// found any more.
pub(crate) fn exclusion(slug: &str, branch: &str) -> Result<Option<Exclusion>> {
    let (owner, agent) = super::parse_slug(slug)?;
    let links = link::list(&Store::at(config::store_root()?));
    let claims = recording_into(&links, &owner, &agent, branch);
    if claims.iter().any(|claim| {
        let candidate = Candidate {
            runtime: claim.source.clone(),
            session_id: claim.session_id.clone(),
            cwd: claim.cwd.as_deref().map(PathBuf::from).unwrap_or_default(),
        };
        delegation_withheld(slug, &candidate, None, false).unwrap_or(true)
    }) {
        return Ok(Some(Exclusion::Delegated));
    }
    let mut sessions: Vec<String> = claims
        .iter()
        .map(|claim| key(&claim.source, &claim.session_id))
        .collect();
    if let Some(origin) = capture_origin(slug, branch)? {
        sessions.push(key(&origin.runtime, &origin.session));
    }
    let records_here = |session: &String| {
        sessions.contains(session)
            || session
                .split_once(':')
                .is_some_and(|(runtime, id)| capture_branch(runtime, id) == branch)
    };
    let id = |session: &String| {
        session
            .split_once(':')
            .map_or_else(|| session.clone(), |(_, id)| id.to_owned())
    };
    let projects: Vec<_> = policies()?
        .into_iter()
        .filter(|p| p.repository == slug)
        .collect();
    // The person's own drop says more than a binding's start does.
    for project in &projects {
        if let Some(session) = project.excluded.iter().find(|s| records_here(s)) {
            return Ok(Some(Exclusion::Dropped {
                folder: project.root.clone(),
                session: id(session),
            }));
        }
    }
    for project in &projects {
        if let Some(session) = project.earlier.iter().find(|s| records_here(s)) {
            return Ok(Some(Exclusion::Earlier {
                folder: project.root.clone(),
                session: id(session),
            }));
        }
    }
    Ok(None)
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
            link::remove(store, &claim.source, &claim.session_id)?;
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
        && let Some((project, _)) = governing(&start)?
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
    super::upload_outcome::forget(repository, Some(branch))?;
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
    pub(crate) sign_in: SignIn,
    /// The git worktree the conversation started in and the main checkout whose setting uploads
    /// it, when it started in one.
    pub(crate) worktree: Option<(PathBuf, PathBuf)>,
}

/// What the saved sign-in lets a project do on this device, judged without asking the Hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignIn {
    /// Signed in as the project's account, and the sign-in can still be renewed.
    Current,
    /// Signed in as the project's account, but the sign-in can no longer be renewed: turns are
    /// still saved on this device, and their upload waits for `agit login`.
    Expired,
    /// Not signed in as the project's account on its Hub: the project neither saves nor uploads
    /// until `agit login`.
    Absent,
}

fn sign_in(project: &Project) -> SignIn {
    match credentials::load_checked(&project.identity.hub) {
        Ok(Some(credential))
            if credential.username == project.account
                && credential.account_id == project.account_id =>
        {
            if !credential.renewable() {
                SignIn::Expired
            } else {
                SignIn::Current
            }
        }
        _ => SignIn::Absent,
    }
}

impl SignIn {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Expired => "expired",
            Self::Absent => "absent",
        }
    }
}

/// The project that saves this conversation by itself, and how it covers the folder the
/// conversation started in, judged without asking the Hub. `None` when no project there uploads
/// automatically, the runtime is not one a project captures, the project withholds the
/// conversation (see [`withholds`]), or it records into another repository.
fn capturing(runtime: &str, session: &str, origin: &Path) -> Option<(Project, Via)> {
    if !captured_runtime(runtime) {
        return None;
    }
    let (project, via) = governing(origin).ok()??;
    let candidate = Candidate {
        runtime: runtime.into(),
        session_id: session.into(),
        cwd: origin.to_path_buf(),
    };
    if withholds(&project, &candidate, false) || claimed_elsewhere(&project, runtime, session) {
        return None;
    }
    Some((project, via))
}

/// Whether this conversation records into a repository other than `project`'s.
fn claimed_elsewhere(project: &Project, runtime: &str, session: &str) -> bool {
    Store::open()
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
        })
}

/// A conversation a project that uploads automatically holds back as the folder's earlier
/// history, and what would upload it.
pub(crate) struct HeldEarlier {
    pub(crate) repository: String,
    /// Who can read that repository, as the Hub last said; `None` when it has not said.
    pub(crate) visibility: Option<String>,
    pub(crate) org: Option<bool>,
    pub(crate) members_read: Option<bool>,
    /// The folder whose setting covers the conversation.
    pub(crate) folder: PathBuf,
    /// The git worktree of that folder the conversation started in, when it started in one.
    pub(crate) worktree: Option<PathBuf>,
}

/// The project that would upload this conversation by itself but holds it back as earlier
/// history: it is from before the folder was on, or from before the folder covered the git
/// worktree it started in (see [`left_out`]). `None` when the project uploads it, the person
/// dropped it, it records into another repository, or no project there is on and uploads
/// automatically. Judged from local state alone, the audience included, so a hook may ask.
pub(crate) fn held_earlier(runtime: &str, session: &str, origin: &Path) -> Option<HeldEarlier> {
    if !captured_runtime(runtime) {
        return None;
    }
    let origin = origin
        .canonicalize()
        .unwrap_or_else(|_| origin.to_path_buf());
    let (project, via) = governing(&origin).ok()??;
    let session_key = key(runtime, session);
    if !project.enabled || !project.auto_upload || project.excluded.contains(&session_key) {
        return None;
    }
    let candidate = Candidate {
        runtime: runtime.into(),
        session_id: session.into(),
        cwd: origin,
    };
    let earlier = project.earlier.contains(&session_key)
        || before_covered(&project, &candidate, false).unwrap_or(true);
    if !earlier || claimed_elsewhere(&project, runtime, session) {
        return None;
    }
    let recorded = recorded_audience(&project.root);
    Some(HeldEarlier {
        repository: project.repository,
        visibility: recorded.visibility,
        org: recorded.org,
        members_read: recorded.members_read,
        folder: project.root,
        worktree: match via {
            Via::Worktree(worktree) => Some(worktree),
            Via::Folder => None,
        },
    })
}

/// Whether the project covering `origin` captures this conversation by itself; see [`capturing`].
pub(crate) fn captured_by_project(runtime: &str, session: &str, origin: &Path) -> bool {
    let origin = origin
        .canonicalize()
        .unwrap_or_else(|_| origin.to_path_buf());
    capturing(runtime, session, &origin).is_some()
}

/// Record the runtime memory baseline of a conversation the project uploads, as it starts: the
/// first collection on the branch it is uploaded to then takes in what the conversation itself
/// writes, and leaves alone the personal memory that was already there. A conversation already
/// uploaded keeps its branch's own baseline.
pub(crate) fn record_memory_start(runtime: &str, session: &str, origin: &Path) -> Result<()> {
    let cwd = origin.canonicalize()?;
    let Some((project, _)) = capturing(runtime, session, &cwd) else {
        return Ok(());
    };
    let candidate = Candidate {
        runtime: runtime.into(),
        session_id: session.into(),
        cwd,
    };
    let branch = match placement(&project, &candidate)? {
        Placement::Claim(branch) => Some(branch),
        Placement::New => None,
        Placement::Foreign(_) => return Ok(()),
    };
    super::memory::record_start(
        &checkout(&project)?,
        branch.as_deref(),
        &key(runtime, session),
        runtime,
        &candidate.cwd,
    )?;
    Ok(())
}

/// The repository a project saves this conversation into by itself, and what the sign-in lets it
/// do, read from local state alone.
pub(crate) fn automatic_capture(
    runtime: &str,
    session: &str,
    origin: &Path,
) -> Option<(String, SignIn)> {
    let origin = origin
        .canonicalize()
        .unwrap_or_else(|_| origin.to_path_buf());
    let (project, _) = capturing(runtime, session, &origin)?;
    let sign_in = sign_in(&project);
    Some((project.repository, sign_in))
}

/// The repository a project uploads this conversation to by itself, judged by the folder the
/// conversation started in, as [`capturing`] judges it. Who can see it is the Hub's answer within
/// `wait`, recorded for later, or else the last recorded answer; the requests never renew
/// credentials, so a caller may run them inside a hook.
pub(crate) fn automatic_upload(
    runtime: &str,
    session: &str,
    origin: &Path,
    wait: std::time::Duration,
) -> Option<AutomaticUpload> {
    let (project, via) = capturing(runtime, session, origin)?;
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
    let sign_in = sign_in(&project);
    Some(AutomaticUpload {
        visibility: audience.visibility.filter(|v| !v.is_empty()),
        org: audience.org,
        members_read: audience.members_read,
        sign_in,
        worktree: match via {
            Via::Worktree(worktree) => Some((worktree, project.root)),
            Via::Folder => None,
        },
        repository: project.repository,
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

/// The transcript's length in bytes at the same upload, so what was added after it can be read.
fn caught_lengths(root: &Path) -> std::collections::BTreeMap<String, u64> {
    caught_path(root)
        .ok()
        .and_then(|path| fs::read(path.with_extension("caught-length")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record_caught_length(root: &Path, candidate: &Candidate, length: u64) -> Result<()> {
    let mut map = caught_lengths(root);
    map.insert(key(&candidate.runtime, &candidate.session_id), length);
    let mut file = tempfile::NamedTempFile::new_in(home()?)?;
    file.write_all(&serde_json::to_vec(&map)?)?;
    file.persist(caught_path(root)?.with_extension("caught-length"))
        .map_err(|e| e.error)?;
    Ok(())
}

fn record_caught(root: &Path, candidate: &Candidate, modified: u64) -> Result<()> {
    let mut map = caught(root);
    map.insert(key(&candidate.runtime, &candidate.session_id), modified);
    let mut file = tempfile::NamedTempFile::new_in(home()?)?;
    file.write_all(&serde_json::to_vec(&map)?)?;
    file.persist(caught_path(root)?).map_err(|e| e.error)?;
    Ok(())
}

fn transcript_length(candidate: &Candidate) -> Option<u64> {
    let adapter = adapter::get(&candidate.runtime).ok()?;
    let path = adapter.resolve(&candidate.session_id, Some(&candidate.cwd))?;
    Some(fs::metadata(path).ok()?.len())
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
    if project.leaves_out(&session_key) {
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
            !crate::domain::privacy_git::hub_holds_head(&repo, &branch)
                || match caught.get(&session_key) {
                    None => transcript_modified(candidate).is_some(),
                    Some(&modified) => adapter::get(&candidate.runtime)
                        .ok()
                        .and_then(|adapter| {
                            adapter.resolve(&candidate.session_id, Some(&candidate.cwd))
                        })
                        .is_some_and(|path| {
                            changed_after(
                                &candidate.runtime,
                                &path,
                                modified,
                                caught_lengths(&project.root).get(&session_key).copied(),
                            )
                        }),
                }
        }
        _ => false,
    }
}

pub(crate) fn registration_scope(cwd: &Path) -> bool {
    governing(cwd)
        .ok()
        .flatten()
        .is_some_and(|(p, _)| p.enabled && p.auto_upload)
}

pub(crate) fn blocks_auto_push(repo: &Repo, slug: &str, branch: &str) -> Result<bool> {
    Ok(auto_push_blocker(repo, slug, branch)?.is_some())
}

/// Why automatic pushes leave a branch out, in fixed words that carry nothing of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocker {
    Withheld(Withholding),
    Withdrawn,
    Oversized,
    /// The project of the folder the conversation started in is paused or does not upload.
    ProjectOff,
}

impl Blocker {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Withheld(Withholding::Dropped) => "you stopped uploading it from this computer",
            Self::Withheld(reason) => reason.describe(),
            Self::Withdrawn => UploadFailure::BuildsOnWithdrawn.describe(),
            Self::Oversized => UploadFailure::TooLarge.describe(),
            Self::ProjectOff => "automatic upload is off for the folder it started in",
        }
    }
}

/// [`blocks_auto_push`], with the reason.
pub(crate) fn auto_push_blocker(repo: &Repo, slug: &str, branch: &str) -> Result<Option<Blocker>> {
    if let Some(reason) = withheld(slug, branch)? {
        return Ok(Some(Blocker::Withheld(reason)));
    }
    if refused_as_withdrawn(repo, branch)? {
        return Ok(Some(Blocker::Withdrawn));
    }
    if refused_as_oversized(repo, branch)? {
        return Ok(Some(Blocker::Oversized));
    }
    let meta = crate::domain::meta::read_at_ref_result(repo, &format!("refs/heads/{branch}"))?
        .context("cannot verify project policy without branch metadata")?;
    if meta.is_file_line() {
        return Ok(None);
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
        return Ok(None);
    }
    let cwd = hydrated.content;
    let cwd = Path::new(&cwd)
        .canonicalize()
        .context("cannot resolve the session directory to verify project policy")?;
    let Some((project, _)) = governing(&cwd)? else {
        return Ok(None);
    };
    Ok(
        (project.repository == slug && (!project.enabled || !project.auto_upload))
            .then_some(Blocker::ProjectOff),
    )
}

/// Whether the folder the conversation of `slug@branch` ran in keeps it from this upload: it leaves
/// the conversation out (dropped, or from before the folder was on), or, for an `automatic`
/// upload, the folder is off or doesn't upload by itself. The folder's word holds whatever
/// repository the branch publishes to, and however the branch was recorded. A branch whose
/// directory or folder policy can't be read is refused with an error.
pub(crate) fn folder_refuses_upload(
    repo: &Repo,
    slug: &str,
    branch: &str,
    automatic: bool,
) -> Result<bool> {
    // A branch without session metadata holds no conversation.
    let Some(meta) =
        crate::domain::meta::read_at_ref_result(repo, &format!("refs/heads/{branch}"))?
    else {
        return Ok(false);
    };
    // A conversation that recorded no directory ran in no folder whose policy could object.
    if meta.is_file_line() || meta.cwd.is_empty() {
        return Ok(false);
    }
    let hydrated = crate::domain::privacy::service::transform(
        Some(repo.root()),
        &meta.cwd,
        crate::domain::privacy::projector::Mode::HydrateText,
    );
    ensure!(
        crate::domain::privacy::projector::tokens(&hydrated.content)
            .next()
            .is_none(),
        "cannot resolve the session directory to verify the folder's policy"
    );
    // A folder's policy is kept by its path, so a directory removed since still finds it.
    let recorded = Path::new(&hydrated.content);
    let cwd = recorded
        .canonicalize()
        .unwrap_or_else(|_| recorded.to_path_buf());
    let Some((project, _)) = governing(&cwd)? else {
        return Ok(false);
    };
    if automatic && (!project.enabled || !project.auto_upload) {
        return Ok(true);
    }
    let mut sessions: Vec<String> = meta
        .runtime_instances
        .iter()
        .map(|id| key(&meta.runtime, id))
        .collect();
    sessions.extend(
        meta.capture
            .iter()
            .chain(capture_origin(slug, branch)?.iter())
            .map(|origin| key(&origin.runtime, &origin.session)),
    );
    let (owner, agent) = super::parse_slug(slug)?;
    sessions.extend(
        recording_into(
            &link::list(&Store::at(config::store_root()?)),
            &owner,
            &agent,
            branch,
        )
        .iter()
        .map(|claim| key(&claim.source, &claim.session_id)),
    );
    // A session of a git worktree from before the folder covered it is left out too.
    Ok(sessions.iter().any(|session| {
        project.leaves_out(session)
            || session.split_once(':').is_some_and(|(runtime, id)| {
                before_covered(
                    &project,
                    &Candidate {
                        runtime: runtime.into(),
                        session_id: id.into(),
                        cwd: cwd.clone(),
                    },
                    false,
                )
                .unwrap_or(true)
            })
    }))
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

/// Whether the Hub this checkout publishes to refused `branch` for holding more event entries
/// than it reads, and its tip still holds more than the limit the Hub named.
fn refused_as_oversized(repo: &Repo, branch: &str) -> Result<bool> {
    let Some(identity) = crate::hub::identity::read(repo)? else {
        return Ok(false);
    };
    let Some(&limit) = crate::domain::privacy_git::oversized_refs(repo, &identity)?
        .get(&format!("refs/heads/{branch}"))
    else {
        return Ok(false);
    };
    let Some(tip) = repo.git_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}^{{commit}}"),
    ]) else {
        return Ok(false);
    };
    crate::domain::privacy_git::event_entries_exceed(repo, tip.trim(), limit)
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
        let reasons: Vec<_> = errors
            .iter()
            .copied()
            .filter(|message| !is_git_summary(message))
            .collect();
        // git's summary alone is still all there is to say.
        let errors = if reasons.is_empty() { errors } else { reasons };
        if !errors.is_empty() {
            return format!("{command}: {}", errors.join("; "));
        }
    }
    without_git_summary(error)
}

/// Whether `part` is git's own summary of a refused push, which names only the URL. A result saved
/// before the push child left that line out still leads with it, as text or as diagnostics, and a
/// reader shown the first line or two would see the URL and not why.
fn is_git_summary(part: &str) -> bool {
    part.starts_with("failed to push some refs")
}

/// `error` without git's summary of a refused push (`is_git_summary`).
fn without_git_summary(error: &str) -> String {
    let (command, rest) = error.split_once(": ").unwrap_or(("", error));
    let kept: Vec<_> = rest
        .split("; ")
        .filter(|part| !is_git_summary(part))
        .collect();
    if kept.is_empty() || command.is_empty() {
        return error.to_owned();
    }
    format!("{command}: {}", kept.join("; "))
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
    let projected = remote.is_some()
        && checkout
            .as_ref()
            .is_some_and(|(repo, _)| crate::domain::privacy_git::hub_holds_head(repo, branch));
    let status = if pushed || (remote.is_some() && remote == head) || projected {
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

/// Reads local state only and changes none of it. It takes no lock a capture worker waits on; the
/// upload records are read as `agit status` reads them, which looks at a running upload's lock for
/// an instant, so an upload starting meanwhile waits that instant and is never turned away.
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
        // Public automatic uploads switched on with no person answering, for the menu bar to
        // tell the person once each.
        "public_switched_on_unattended": super::push::unattended(),
        // Branches whose newest automatic upload didn't go through, for the menu bar to show as
        // soon as they are recorded: an upload runs after its turn's end, which can't say it.
        "uploads_not_through": super::status::uploads_not_through_json(),
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
            .env("XDG_DATA_HOME", home.path().join("data"))
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
            earlier: BTreeSet::new(),
            earlier_opencode: false,
            last_result: None,
            push_auto_before: None,
        }
    }

    /// Runs git in `dir`, with no configuration but its own.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "protocol.file.allow=always",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("GIT_CONFIG_GLOBAL")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// `to` as a path relative to `from`, both canonical: Git is given relative paths only, as it
    /// does not read every canonical form a platform writes.
    fn relative(from: &Path, to: &Path) -> PathBuf {
        let common = from
            .components()
            .zip(to.components())
            .take_while(|(a, b)| a == b)
            .count();
        let mut path = PathBuf::new();
        for _ in from.components().skip(common) {
            path.push("..");
        }
        for part in to.components().skip(common) {
            path.push(part);
        }
        path
    }

    /// A git checkout at `dir` with one commit.
    fn git_checkout(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "start"]);
    }

    /// A linked worktree of the checkout at `main`, made at `at`, both canonical; returns its
    /// canonical path.
    fn worktree(main: &Path, at: &Path) -> PathBuf {
        let path = relative(main, at);
        git(
            main,
            &["worktree", "add", "-q", "--detach", path.to_str().unwrap()],
        );
        at.canonicalize().unwrap()
    }

    /// A Claude Code conversation in `cwd` whose first record is stamped `at`.
    fn transcript(id: &str, cwd: &Path, at: &str) {
        let native = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").unwrap())
            .join("projects")
            .join(adapter::claude_code::slug_for(cwd));
        fs::create_dir_all(&native).unwrap();
        let record = serde_json::json!({"type": "user", "sessionId": id, "cwd": cwd,
            "timestamp": at, "message": {"role": "user", "content": "hello"}});
        fs::write(native.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
    }

    /// A folder covers the linked worktrees that share its git common directory, and nothing else
    /// that looks alike: not a separate clone, not a submodule, not a copy of a worktree whose Git
    /// directory names another, not a worktree whose common directory cannot be read, not one that
    /// is gone. A worktree below a folder set up for another repository, one with a setting of its
    /// own, and the place where the folder holds a nested setting keep theirs. Coverage flows only
    /// from the main checkout: a worktree that is on covers neither the main checkout nor its
    /// siblings. An implementation that matched the remote, the folder name or the `.git` file's
    /// text alone would cover the clone, the copy or the submodule; one that read coverage both
    /// ways would let a worktree publish the main checkout's sessions.
    #[test]
    fn a_policy_covers_its_worktrees_by_common_dir_only() {
        if !isolated("a_policy_covers_its_worktrees_by_common_dir_only") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let main = base.join("main");
        git_checkout(&main);
        fs::create_dir_all(main.join("pkg")).unwrap();
        let tree = worktree(&main, &base.join("tree"));
        fs::create_dir_all(tree.join("src")).unwrap();
        fs::create_dir_all(tree.join("pkg")).unwrap();
        git(&base, &["clone", "-q", "main", "clone"]);
        git_checkout(&base.join("library"));
        git(&tree, &["submodule", "add", "-q", "../library", "sub"]);
        let copy = base.join("copy");
        fs::create_dir_all(&copy).unwrap();
        fs::copy(tree.join(".git"), copy.join(".git")).unwrap();
        let broken = worktree(&main, &base.join("broken"));
        fs::remove_file(main.join(".git/worktrees/broken/commondir")).unwrap();
        let gone = worktree(&main, &base.join("gone"));
        fs::remove_dir_all(&gone).unwrap();
        let other = base.join("other");
        fs::create_dir_all(&other).unwrap();
        workspace::bind(&other, "alice/other", false).unwrap();
        let below_other = worktree(&main, &other.join("tree"));
        let own = worktree(&main, &base.join("own"));
        let mut paused = project(&own);
        paused.enabled = false;
        save(&paused).unwrap();
        save(&project(&main)).unwrap();
        let mut nested = project(&main.join("pkg"));
        nested.repository = "alice/pkg".into();
        save(&nested).unwrap();

        let via = |path: &Path| {
            governing(path)
                .unwrap()
                .map(|(project, via)| (project.root, via))
        };
        let covered = Some((main.clone(), Via::Worktree(tree.clone())));
        assert_eq!(via(&tree), covered);
        assert_eq!(via(&tree.join("src")), covered);
        for path in [
            base.join("clone"),
            tree.join("sub"),
            copy.clone(),
            broken.clone(),
            below_other.clone(),
            tree.join("pkg"),
        ] {
            assert_eq!(via(&path), None, "{}", path.display());
        }
        assert_eq!(via(&own), Some((own.clone(), Via::Folder)));
        assert_eq!(covered_worktrees(&main), std::slice::from_ref(&tree));
        assert_eq!(worktree_of(&tree).as_deref(), Some(main.as_path()));
        assert_eq!(policy_root(&tree), main);

        let second = base.join("second");
        git_checkout(&second);
        let on = worktree(&second, &base.join("second-on"));
        let sibling = worktree(&second, &base.join("second-sibling"));
        save(&project(&on)).unwrap();
        assert_eq!(via(&on), Some((on.clone(), Via::Folder)));
        assert_eq!(via(&second), None);
        assert_eq!(via(&sibling), None);
    }

    /// A conversation is announced as uploading only where the project would upload it: started
    /// inside an automatic project by a runtime whose hooks capture it, not excluded, not recording
    /// into another repository. A judgement without the runtime check tells an OpenCode agent the
    /// project saves its conversation, and hides the import that would. The
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
        for earlier in ["old", "excluded", "elsewhere"] {
            policy.earlier.insert(key("codex", earlier));
        }
        save(&policy).unwrap();
        let wait = std::time::Duration::from_millis(1);
        let announced = |id: &str, origin: &Path| {
            automatic_upload("codex", id, origin, wait).map(|upload| upload.repository)
        };
        assert_eq!(announced("fresh", &inside).as_deref(), Some("alice/app"));
        assert!(automatic_capture("codex", "fresh", &inside).is_some());
        assert!(automatic_capture("opencode", "fresh", &inside).is_none());
        assert!(announced("excluded", &inside).is_none());
        assert!(announced("fresh", policy.root.parent().unwrap()).is_none());
        let store = Store::at(config::store_root().unwrap());
        let mut claim = link::Link::new("codex", "elsewhere", Some(&inside));
        claim.owner = Some("bob".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("work".into());
        link::write(&store, &claim).unwrap();
        assert!(announced("elsewhere", &inside).is_none());
        // Held back as the folder's earlier history, a conversation is not announced, and a resume
        // of it is told why; not when the person dropped it, it records into another repository,
        // or its runtime is not one the project captures.
        let held = |runtime: &str, id: &str, origin: &Path| {
            held_earlier(runtime, id, origin).map(|held| (held.repository, held.worktree))
        };
        assert!(announced("old", &inside).is_none());
        assert_eq!(
            held("codex", "old", &inside),
            Some(("alice/app".into(), None))
        );
        for (runtime, id) in [
            ("opencode", "old"),
            ("codex", "fresh"),
            ("codex", "excluded"),
            ("codex", "elsewhere"),
        ] {
            assert!(held(runtime, id, &inside).is_none(), "{runtime} {id}");
        }

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

        // In a git worktree the folder covers, a conversation from before the folder covered it
        // is the folder's earlier history: not announced and not captured; one that began after,
        // or has recorded nothing yet, uploads and says through which folder. A conversation whose
        // start was the first sight of a worktree counts as after even when the runtime stamped
        // it a moment before the hook ran; another stamped the same moment does not. Claimed in
        // the repository, an earlier one is the folder's like any other; dropped, a later one is
        // not. A save that lets earlier sessions in lets it in; automatic capture never does.
        const BEFORE: &str = "aaaaaaaa-0000-4000-8000-000000000041";
        const UNSTAMPED: &str = "bbbbbbbb-0000-4000-8000-000000000042";
        const AFTER: &str = "cccccccc-0000-4000-8000-000000000043";
        const STARTING: &str = "dddddddd-0000-4000-8000-000000000044";
        const OPENER: &str = "eeeeeeee-0000-4000-8000-000000000045";
        const SIBLING: &str = "ffffffff-0000-4000-8000-000000000046";
        git_checkout(&policy.root);
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        let tree = worktree(&policy.root, &outside.join("tree"));
        adopt_worktree(&policy.root, &tree, None).unwrap();
        let now = chrono::Utc::now();
        let hours = |offset: i64| (now + chrono::Duration::hours(offset)).to_rfc3339();
        transcript(BEFORE, &tree, &hours(-1));
        transcript(UNSTAMPED, &tree, "not a time");
        transcript(AFTER, &tree, &hours(1));
        let claude = |id: &str, origin: &Path| automatic_upload("claude-code", id, origin, wait);
        assert!(claude(BEFORE, &tree).is_none());
        assert!(claude(UNSTAMPED, &tree).is_none());
        assert!(left_out(&tree, "claude-code", BEFORE, &tree));
        assert_eq!(
            held("claude-code", BEFORE, &tree),
            Some(("alice/app".into(), Some(tree.clone())))
        );
        assert!(held("claude-code", UNSTAMPED, &tree).is_some());
        assert!(held("claude-code", AFTER, &tree).is_none());
        assert_eq!(
            claude(AFTER, &tree).and_then(|upload| upload.worktree),
            Some((tree.clone(), policy.root.clone()))
        );
        assert!(!left_out(&tree, "claude-code", AFTER, &tree));
        // Saving in the background (RC, watching from the Hub) holds the worktree to the same
        // start: earlier history and an unknown start are kept back; a later one is the folder's.
        assert_eq!(folder_decision(&tree, "claude-code", BEFORE), Some(false));
        assert_eq!(
            folder_decision(&tree, "claude-code", UNSTAMPED),
            Some(false)
        );
        assert_eq!(folder_decision(&tree, "claude-code", AFTER), Some(true));
        assert!(claude(STARTING, &tree).is_some());
        let candidate = |id: &str, cwd: &Path| Candidate {
            runtime: "claude-code".into(),
            session_id: id.into(),
            cwd: cwd.to_path_buf(),
        };
        let found: BTreeSet<String> = candidates(&policy)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.session_id)
            .collect();
        assert!(found.contains(BEFORE) && found.contains(AFTER), "{found:?}");
        assert!(before_covered(&policy, &candidate(BEFORE, &tree), true).unwrap());
        assert!(!before_covered(&policy, &candidate(AFTER, &tree), true).unwrap());
        assert!(withholds(&policy, &candidate(BEFORE, &tree), false));
        assert!(capture_candidate(policy.clone(), candidate(BEFORE, &tree), false, true).unwrap());
        assert!(read(&policy.root).unwrap().unwrap().last_result.is_none());
        let mut kept = link::Link::new("claude-code", BEFORE, Some(&tree));
        kept.owner = Some("alice".into());
        kept.agent = Some("app".into());
        kept.branch = Some("kept".into());
        link::write(&store, &kept).unwrap();
        assert!(claude(BEFORE, &tree).is_some());
        assert!(held("claude-code", BEFORE, &tree).is_none());
        policy.excluded.insert(key("claude-code", AFTER));
        save(&policy).unwrap();
        assert!(claude(AFTER, &tree).is_none());
        assert!(is_dropped(&tree, "claude-code", AFTER));
        let fresh = worktree(&policy.root, &outside.join("fresh"));
        let moment = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        transcript(OPENER, &fresh, &moment);
        transcript(SIBLING, &fresh, &moment);
        adopt(&fresh, "claude-code", OPENER);
        assert!(claude(OPENER, &fresh).is_some());
        assert!(claude(SIBLING, &fresh).is_none());
        // Turned off (`agit off`, as after a no once a save turned the folder on), the folder
        // stops its worktrees with it: a conversation there is neither announced, captured,
        // caught up nor reported, the worktree reads as off, and a save there is refused as for
        // the folder. A check that covered a worktree whatever the folder's state would go on
        // uploading it.
        const LATER: &str = "abababab-0000-4000-8000-000000000047";
        transcript(LATER, &tree, &hours(2));
        assert!(claude(LATER, &tree).is_some());
        policy.enabled = false;
        save(&policy).unwrap();
        assert!(claude(LATER, &tree).is_none());
        assert!(held("claude-code", UNSTAMPED, &tree).is_none());
        assert_eq!(uploads_automatically(&tree), Some(false));
        assert!(turned_off(&tree));
        assert!(!registration_scope(&tree));
        assert!(!catch_up(&tree).unwrap());
        assert!(capture("claude-code", LATER, tree.to_str()).unwrap());
        assert!(markers(&policy.root).unwrap().is_empty());
        #[cfg(unix)]
        assert_eq!(
            capture_state("claude-code", LATER, &tree),
            Capture::Withheld
        );
        policy.enabled = true;
        save(&policy).unwrap();
        // What the person dropped in the worktree is in the folder's set. `agit off` in the
        // worktree gives it a policy of its own, and a folder bound on its own (a worktree or a
        // folder below) starts from the drops of the policy that covered it, so the drop survives
        // `agit off`, `agit on` and `agit save --all` there: it is not captured, announced or let
        // in by a save that asks for earlier sessions, and only a save that names it lets it back.
        // A policy of its own that started empty would upload it again.
        assert_eq!(inherited_drops(&tree).unwrap(), policy.excluded);
        assert_eq!(inherited_drops(&inside).unwrap(), policy.excluded);
        assert!(inherited_drops(&policy.root).unwrap().is_empty());
        run(Args {
            action: Action::Unbind { path: tree.clone() },
        })
        .unwrap();
        let mut own = read(&tree).unwrap().unwrap();
        assert!(!own.enabled);
        assert!(own.excluded.contains(&key("claude-code", AFTER)));
        own.enabled = true;
        save(&own).unwrap();
        assert!(is_dropped(&tree, "claude-code", AFTER));
        assert!(claude(AFTER, &tree).is_none());
        assert!(capture("claude-code", AFTER, tree.to_str()).unwrap());
        assert!(markers(&tree).unwrap().is_empty());
        let session = |id: &str| ("claude-code".to_string(), id.to_string(), tree.clone());
        let saved = save_now(&tree, &[session(AFTER)], true, false).unwrap();
        assert!(
            matches!(&saved[..], [Saved::Refused(why)] if why.contains("agit project drop")),
            "a dropped session was let in"
        );
        let _ = save_now(&tree, &[session(AFTER)], false, true).unwrap();
        assert!(!is_dropped(&tree, "claude-code", AFTER));
        fs::remove_file(state_path(&tree).unwrap()).unwrap();

        policy.auto_upload = false;
        save(&policy).unwrap();
        assert!(announced("fresh", &inside).is_none());
    }

    /// An OpenCode database holding `sessions`, each started in `cwd`.
    fn opencode_database(path: &Path, cwd: &Path, sessions: &[&str]) {
        let database = rusqlite::Connection::open(path).unwrap();
        database
            .execute_batch(
                "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
                 CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, directory TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, version TEXT NOT NULL);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);",
            )
            .unwrap();
        let cwd = cwd.to_str().unwrap();
        database
            .execute("INSERT INTO project VALUES ('global', ?1)", [cwd])
            .unwrap();
        for session in sessions {
            database
                .execute(
                    "INSERT INTO session VALUES (?1, 'global', NULL, ?2, 100, 150, '1.18.13')",
                    [session, &cwd],
                )
                .unwrap();
        }
    }

    /// A captured OpenCode conversation is judged by the folder it started in, like the others:
    /// that folder is the `directory` of OpenCode's session, which its materialized transcript
    /// names in its first record rather than as a `cwd`. A reader that knows only `cwd` finds no
    /// start for any OpenCode conversation, and publication withholds every one of them as having
    /// started somewhere it cannot verify.
    #[test]
    fn an_opencode_conversation_starts_in_its_sessions_folder() {
        if !isolated("an_opencode_conversation_starts_in_its_sessions_folder") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let folder = work.path().canonicalize().unwrap();
        let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("opencode");
        fs::create_dir_all(&data).unwrap();
        opencode_database(&data.join("opencode.db"), &folder, &["ses_started_here"]);
        let claim = link::Link::new("opencode", "ses_started_here", Some(&folder));
        assert_eq!(start(&claim), Some(folder));
    }

    /// A bind without earlier history records the folder's OpenCode conversations only from a
    /// complete reading of OpenCode's store. A store it cannot read, the database or the path to
    /// it, leaves the policy unmarked, so a conversation that was there keeps counting as earlier
    /// once the store reads again; only a database that does not exist reads as empty. A
    /// complete reading counts what was there and nothing started later. Marking the policy after
    /// an unread store lets those conversations into a plain save; failing closed after a complete
    /// reading would keep every later one out as well.
    #[test]
    fn opencode_history_is_recorded_only_from_a_complete_reading() {
        if !isolated("opencode_history_is_recorded_only_from_a_complete_reading") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        policy.history = History::None;
        let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("opencode");
        fs::create_dir_all(&data).unwrap();
        let database = data.join("opencode.db");

        let (found, read) = opencode_history(&policy);
        assert!(found.is_empty() && read, "no database holds no sessions");

        // A database path that cannot be looked at for any reason but its absence reads as unread,
        // not empty: here a file stands where OpenCode's folder belongs, which every user, root
        // included, is refused with "not a directory". Windows reports that case as not found.
        #[cfg(unix)]
        {
            fs::remove_dir(&data).unwrap();
            fs::write(&data, b"").unwrap();
            let (found, read) = opencode_history(&policy);
            assert!(
                found.is_empty() && !read,
                "a path it cannot look at is not an empty store"
            );
            fs::remove_file(&data).unwrap();
            fs::create_dir_all(&data).unwrap();
        }

        fs::write(&database, b"not a database").unwrap();
        let (found, read) = opencode_history(&policy);
        assert!(found.is_empty() && !read);
        policy.earlier_opencode = read;
        fs::remove_file(&database).unwrap();
        opencode_database(&database, &policy.root, &["ses_before"]);
        assert!(opencode_earlier(&policy, "ses_before"));

        let (found, read) = opencode_history(&policy);
        assert!(read);
        assert_eq!(
            found
                .iter()
                .map(|c| c.session_id.as_str())
                .collect::<Vec<_>>(),
            ["ses_before"]
        );
        policy.earlier_opencode = read;
        policy
            .earlier
            .extend(found.iter().map(|c| key(&c.runtime, &c.session_id)));
        assert!(opencode_earlier(&policy, "ses_before"));
        assert!(!opencode_earlier(&policy, "ses_after"));
    }

    /// A Claude Code conversation the project uploads records its memory baseline as it starts,
    /// with the personal memory already there; an excluded one and a Codex one (no per-project
    /// memory) record nothing. Without the record, the first upload collects nothing the
    /// conversation wrote; with one for every conversation, folders that never upload leave
    /// records behind in the repository.
    #[test]
    fn a_conversation_the_project_uploads_records_its_memory_start() {
        if !isolated("a_conversation_the_project_uploads_records_its_memory_start") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        policy.excluded.insert(key("claude-code", "excluded"));
        save(&policy).unwrap();
        let repo = Repo::init(&config::repo_dir("alice", "app").unwrap()).unwrap();
        let mem = crate::infra::runtime_memory::locate("claude-code", &policy.root)
            .unwrap()
            .dir;
        fs::create_dir_all(&mem).unwrap();
        fs::write(mem.join("personal.md"), "before the start\n").unwrap();
        let starts = || -> Vec<PathBuf> {
            fs::read_dir(repo.common_dir().unwrap().join("agit-memory-start"))
                .map(|dir| dir.map(|entry| entry.unwrap().path()).collect())
                .unwrap_or_default()
        };

        record_memory_start("claude-code", "excluded", &policy.root).unwrap();
        record_memory_start("codex", "fresh", &policy.root).unwrap();
        assert!(starts().is_empty());
        record_memory_start("claude-code", "fresh", &policy.root).unwrap();
        let recorded = starts();
        assert_eq!(recorded.len(), 1);
        assert!(
            fs::read_to_string(&recorded[0])
                .unwrap()
                .contains("personal.md")
        );
    }

    /// What the person dropped in a folder stays dropped when the folder moves to another
    /// repository (`agit on --repo`, #266) or is bound to one with `--rebind`: the binding that
    /// completes the move starts from the drops of the policy it replaces. A move that is undone
    /// puts the policy back, drops included, and leaves no record behind. A move that forgot them
    /// would count a dropped conversation as an earlier session, and `agit save --all` would upload
    /// it to the new repository.
    #[test]
    fn a_move_to_another_repository_keeps_what_the_person_dropped() {
        if !isolated("a_move_to_another_repository_keeps_what_the_person_dropped") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        let dropped = "dddddddd-0000-4000-8000-000000000051";
        policy.excluded.insert(key("claude-code", dropped));
        save(&policy).unwrap();
        workspace::bind(&policy.root, &policy.repository, false).unwrap();
        let record = state_path(&policy.root)
            .unwrap()
            .with_extension("moved-drops");

        // `--rebind` replaces the policy in place: its drops come along.
        assert!(
            kept_drops(&policy.root)
                .unwrap()
                .contains(&key("claude-code", dropped))
        );

        // A move releases the policy first; the drops wait for the binding that completes it.
        let previous = unbind_for_move(&policy.root).unwrap();
        assert!(read(&policy.root).unwrap().is_none());
        assert_eq!(moved_drops(&policy.root).unwrap(), policy.excluded);
        assert!(
            kept_drops(&policy.root)
                .unwrap()
                .contains(&key("claude-code", dropped))
        );
        // Undone, the folder is as it was, and the record is gone.
        previous.restore(&policy.root, "alice/other").unwrap();
        assert_eq!(
            read(&policy.root).unwrap().unwrap().excluded,
            policy.excluded
        );
        assert!(!record.exists());

        // Completed: the new repository's policy starts from them, as bind builds it, so a save
        // that asks for earlier sessions refuses the dropped one and only naming it lets it in.
        let _previous = unbind_for_move(&policy.root).unwrap();
        let mut moved = project(&policy.root);
        moved.repository = "alice/other".into();
        moved.excluded.extend(kept_drops(&policy.root).unwrap());
        save(&moved).unwrap();
        let session = || {
            (
                "claude-code".to_string(),
                dropped.to_string(),
                policy.root.clone(),
            )
        };
        let saved = save_now(&policy.root, &[session()], true, false).unwrap();
        assert!(
            matches!(&saved[..], [Saved::Refused(why)] if why.contains("agit project drop")),
            "a dropped session was let in after the move"
        );
        let _ = save_now(&policy.root, &[session()], false, true).unwrap();
        assert!(!is_dropped(&policy.root, "claude-code", dropped));

        // A record that cannot be read stops the binding instead of going ahead without the drops,
        // and it stays for the next try.
        fs::write(&record, b"not a list").unwrap();
        let error = kept_drops(&policy.root).unwrap_err();
        assert!(format!("{error:#}").contains("moved-drops"), "{error:#}");
        assert!(record.exists());
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

    /// Capture inspection must keep both ordinary and delegated histories cold while still
    /// applying the root's worktree cutoff. Restoring a rollout merely to read its header changes
    /// storage state and makes metadata work scale with conversation content.
    #[test]
    fn capture_inspection_keeps_codex_history_cold() {
        if !isolated("capture_inspection_keeps_codex_history_cold") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        git_checkout(&main);
        let tree = worktree(&main, &temp.path().join("tree"));
        let policy = project(&main);
        save(&policy).unwrap();
        let root = "aaaaaaaa-0000-4000-8000-000000000001";
        let child = "bbbbbbbb-0000-4000-8000-000000000002";
        let native =
            PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()).join("sessions/2026/11/01");
        fs::create_dir_all(&native).unwrap();
        let mut carriers = Vec::new();
        for (id, timestamp, parent) in [
            (root, "2026-01-01T00:00:00Z", None),
            (child, "2028-01-01T00:00:00Z", Some(root)),
        ] {
            let mut header = serde_json::json!({"type":"session_meta","timestamp":timestamp,"payload":{"id":id,"cwd":tree}});
            if let Some(parent) = parent {
                header["payload"]["thread_source"] = "subagent".into();
                header["payload"]["parent_thread_id"] = parent.into();
            }
            let body = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"retained history"}]}});
            let bytes = ruzstd::encoding::compress_to_vec(
                format!("{header}\n{body}\n").as_bytes(),
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            let plain = native.join(format!("rollout-2026-11-01T00-00-00-{id}.jsonl"));
            let cold = plain.with_extension("jsonl.zst");
            fs::write(&cold, &bytes).unwrap();
            carriers.push((plain, cold, bytes));
        }
        let cover = |date: &str| {
            update_covered(&policy.root, |records| {
                records.insert(
                    tree.clone(),
                    Covered {
                        since: date.parse().unwrap(),
                        opener: None,
                    },
                );
            })
            .unwrap()
        };
        for (date, withheld) in [
            ("2025-01-01T00:00:00Z", false),
            ("2027-01-01T00:00:00Z", true),
        ] {
            cover(date);
            for id in [root, child] {
                let candidate = Candidate {
                    runtime: "codex".into(),
                    session_id: id.into(),
                    cwd: tree.clone(),
                };
                assert_eq!(withholds(&policy, &candidate, false), withheld);
            }
            for (plain, cold, bytes) in &carriers {
                assert!(
                    !plain.exists(),
                    "metadata inspection restored a plaintext rollout"
                );
                assert_eq!(
                    &fs::read(cold).expect("compressed carrier must remain"),
                    bytes
                );
            }
        }
    }

    /// Siblings and nested delegates share the root's exclusion, history cutoff and repository
    /// authority. A child's own claim cannot override a later change to the root's policy.
    #[test]
    fn delegated_tree_inherits_root_capture_authority() {
        if !isolated("delegated_tree_inherits_root_capture_authority") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        git_checkout(&main);
        let tree = worktree(&main, &temp.path().join("tree"));
        let mut policy = project(&main);
        save(&policy).unwrap();
        let root = "aaaaaaaa-0000-4000-8000-000000000001";
        let child = "bbbbbbbb-0000-4000-8000-000000000002";
        let sibling = "cccccccc-0000-4000-8000-000000000003";
        let grandchild = "dddddddd-0000-4000-8000-000000000004";
        let native =
            PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()).join("sessions/2026/11/01");
        fs::create_dir_all(&native).unwrap();
        for (id, parent) in [
            (root, None),
            (child, Some(root)),
            (sibling, Some(root)),
            (grandchild, Some(child)),
        ] {
            let mut header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":id,"cwd":tree}});
            if let Some(parent) = parent {
                header["payload"]["thread_source"] = "subagent".into();
                header["payload"]["source"] =
                    serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":parent}}});
            }
            fs::write(
                native.join(format!("rollout-2026-11-01T00-00-00-{id}.jsonl")),
                format!("{header}\n"),
            )
            .unwrap();
        }
        let cover = |date: &str| {
            update_covered(&policy.root, |records| {
                records.insert(
                    tree.clone(),
                    Covered {
                        since: date.parse().unwrap(),
                        opener: None,
                    },
                );
            })
            .unwrap()
        };
        cover("2025-01-01T00:00:00Z");
        let store = Store::at(config::store_root().unwrap());
        let candidates: Vec<_> = [child, sibling, grandchild]
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut claim = link::Link::new("codex", id, Some(&tree));
                claim.owner = Some("alice".into());
                claim.agent = Some("app".into());
                claim.branch = Some(format!("child-{index}"));
                link::write(&store, &claim).unwrap();
                Candidate {
                    runtime: "codex".into(),
                    session_id: (*id).into(),
                    cwd: tree.clone(),
                }
            })
            .collect();
        assert!(candidates.iter().all(|c| !withholds(&policy, c, false)));
        for earlier in [false, true] {
            if earlier {
                policy.earlier.insert(key("codex", root));
            } else {
                policy.excluded.insert(key("codex", root));
            }
            save(&policy).unwrap();
            for (index, candidate) in candidates.iter().enumerate() {
                assert!(withholds(&policy, candidate, false));
                assert!(capture_candidate(policy.clone(), candidate.clone(), false, true).unwrap());
                assert_eq!(
                    withheld("alice/app", &format!("child-{index}")).unwrap(),
                    Some(Withholding::Excluded)
                );
            }
            assert!(read(&policy.root).unwrap().unwrap().last_result.is_none());
            policy.earlier.clear();
            policy.excluded.clear();
        }
        save(&policy).unwrap();
        cover("2027-01-01T00:00:00Z");
        assert!(candidates.iter().all(|c| withholds(&policy, c, false)));
        cover("2025-01-01T00:00:00Z");
        let mut foreign = link::Link::new("codex", root, Some(&tree));
        foreign.owner = Some("bob".into());
        foreign.agent = Some("other".into());
        foreign.branch = Some("root".into());
        link::write(&store, &foreign).unwrap();
        assert!(candidates.iter().all(|c| withholds(&policy, c, false)));
        // A recorded drop remains authoritative after its native claim is released.
        fs::remove_file(link::link_path(&store, "codex", root)).unwrap();
        update_withheld(|records| {
            records.push(Withheld {
                repository: "alice/app".into(),
                branch: "root".into(),
                session: Some(key("codex", root)),
                reason: Withholding::Dropped,
                announced: true,
            })
        })
        .unwrap();
        assert!(candidates.iter().all(|c| withholds(&policy, c, false)));
        let repo = Repo::init(&config::repo_dir("alice", "app").unwrap()).unwrap();
        repo.git(&["config", "user.name", "Synthetic User"])
            .unwrap();
        repo.git(&["config", "user.email", "synthetic@example.invalid"])
            .unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        repo.git(&["checkout", "-b", "child-0"]).unwrap();
        let child_path = native.join(format!("rollout-2026-11-01T00-00-00-{child}.jsonl"));
        let raw = fs::read_to_string(&child_path).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let log = crate::domain::transcript::wrap_lines(&raw, "codex", &session);
        crate::domain::storage::write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(
            repo.root(),
            &Meta::new(session, "codex".into(), tree.display().to_string()),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("Capture delegated header").unwrap();
        fs::remove_file(child_path).unwrap();
        fs::remove_file(native.join(format!("rollout-2026-11-01T00-00-00-{root}.jsonl"))).unwrap();
        assert_eq!(
            withheld("alice/app", "child-0").unwrap(),
            Some(Withholding::Excluded)
        );
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
        let error = bind(
            bound.path(),
            "alice/app",
            History::None,
            false,
            true,
            false,
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("add `--rebind` to this command to move it to alice/app"),
            "{error:#}"
        );
        assert!(
            bind(
                bound.path(),
                "alice/app",
                History::None,
                false,
                true,
                true,
                None
            )
            .is_ok()
        );

        let enrolled = tempfile::tempdir().unwrap();
        save(&project(enrolled.path())).unwrap();
        let error = bind(
            enrolled.path(),
            "alice/other",
            History::None,
            false,
            true,
            false,
            None,
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
                false,
                None
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
                true,
                None
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
        let worker = std::thread::spawn(move || capture_candidate(stale, candidate, false, true));
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

    /// Pins that the capture check team activity asks agrees with what a Stop hook's capture
    /// uploads: a session no repository has claimed yet uploads to the project, while one the
    /// person dropped, one that ran before the folder was bound without earlier history, one in
    /// a paused project and one in a nested binding of another repository do not, and a folder
    /// outside every project has no policy. Nor does a session whose claim exists but cannot be
    /// read, since its owner is unconfirmed. In a git worktree the folder covers, a session that
    /// began before the folder covered it is earlier history too, while one that began after
    /// uploads. A team activity report of a session rests on this check, so a dropped or earlier
    /// session, one from before a worktree was covered, or one with an unreadable claim, is never
    /// reported; nor is any worktree session by repository id, since a worktree outside the
    /// folder lies under no binding of its own.
    #[cfg(unix)]
    #[test]
    fn the_capture_check_answers_what_capture_would_upload() {
        if !isolated("the_capture_check_answers_what_capture_would_upload") {
            return;
        }
        const RUNTIME: &str = "claude-code";
        const SESSION: &str = "eeeeeeee-0000-4000-8000-000000000005";
        let work = tempfile::tempdir().unwrap();
        let work = work.path().canonicalize().unwrap();
        let app = work.join("app");
        let nested = app.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let mut policy = project(&app);
        save(&policy).unwrap();
        let state = |cwd: &Path| capture_state(RUNTIME, SESSION, cwd);

        assert_eq!(state(&app), Capture::Uploads(policy.identity.clone()));
        assert_eq!(state(&work), Capture::Unbound);

        policy.excluded.insert(key(RUNTIME, SESSION));
        save(&policy).unwrap();
        assert_eq!(state(&app), Capture::Withheld);
        policy.excluded.clear();
        // A session from before the folder was on is neither captured nor reported either.
        policy.earlier.insert(key(RUNTIME, SESSION));
        save(&policy).unwrap();
        assert_eq!(state(&app), Capture::Withheld);
        policy.earlier.clear();
        policy.enabled = false;
        save(&policy).unwrap();
        assert_eq!(state(&app), Capture::Withheld);
        policy.enabled = true;
        save(&policy).unwrap();

        let claim = link::link_path(&Store::at(config::store_root().unwrap()), RUNTIME, SESSION);
        fs::create_dir_all(claim.parent().unwrap()).unwrap();
        fs::write(&claim, b"{ not a claim").unwrap();
        assert_eq!(state(&app), Capture::Withheld);
        fs::remove_file(&claim).unwrap();
        assert_eq!(state(&app), Capture::Uploads(policy.identity.clone()));

        const BEFORE: &str = "aaaaaaaa-0000-4000-8000-000000000051";
        const AFTER: &str = "cccccccc-0000-4000-8000-000000000053";
        git_checkout(&policy.root);
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        let tree = worktree(&policy.root, &outside.join("tree"));
        adopt_worktree(&policy.root, &tree, None).unwrap();
        let now = chrono::Utc::now();
        let hours = |offset: i64| (now + chrono::Duration::hours(offset)).to_rfc3339();
        transcript(BEFORE, &tree, &hours(-1));
        transcript(AFTER, &tree, &hours(1));
        assert_eq!(capture_state(RUNTIME, BEFORE, &tree), Capture::Withheld);
        assert_eq!(
            capture_state(RUNTIME, AFTER, &tree),
            Capture::Uploads(policy.identity.clone())
        );
        assert_eq!(sole_upload(RUNTIME, BEFORE, &tree, &tree), None);
        assert_eq!(sole_upload(RUNTIME, AFTER, &tree, &tree), None);

        let mut child = project(&nested);
        child.repository = "alice/nested".into();
        child.auto_upload = false;
        save(&child).unwrap();
        assert_eq!(state(&nested), Capture::Withheld);
        assert_eq!(state(&app), Capture::Uploads(policy.identity));
    }

    /// Pins when a folder counts as bound by exactly one binding, which is all a team activity
    /// report of a session there may rest on. The one binding over a folder is found; a nested
    /// binding, a workspace binding above it, a spelling through a symlink, an unreadable policy
    /// or an unreadable workspace binding leaves the folder with none, never the nearest one.
    /// And a folder with one binding uploads nowhere unless agit would upload from it now: this
    /// policy's Hub is not the one the CLI is configured for.
    #[cfg(unix)]
    #[test]
    fn a_folder_is_bound_for_reporting_only_by_exactly_one_binding() {
        if !isolated("a_folder_is_bound_for_reporting_only_by_exactly_one_binding") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let work = work.path().canonicalize().unwrap();
        let app = work.join("app");
        let src = app.join("src");
        let nested = app.join("nested");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&nested).unwrap();
        let link = work.join("link");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        save(&project(&app)).unwrap();
        let sole = |cwd: &Path, spelled: &Path| {
            sole_binding(cwd, spelled).map(|project| project.map(|project| project.root))
        };

        assert_eq!(sole(&src, &src).unwrap(), Some(app.clone()));
        assert_eq!(sole(&nested, &nested).unwrap(), Some(app.clone()));
        assert_eq!(sole(&src, &link).unwrap(), None);
        assert_eq!(
            sole_upload(
                "claude-code",
                "dddddddd-0000-4000-8000-000000000004",
                &src,
                &src
            ),
            None
        );

        let mut child = project(&nested);
        child.repository = "alice/nested".into();
        save(&child).unwrap();
        assert_eq!(sole(&nested, &nested).unwrap(), None);
        assert_eq!(sole(&src, &src).unwrap(), Some(app.clone()));

        fs::write(state_path(&work).unwrap(), b"not a policy").unwrap();
        assert!(sole(&src, &src).is_err());
        fs::remove_file(state_path(&work).unwrap()).unwrap();
        let binding = workspace::path_for(&work).unwrap();
        fs::create_dir_all(binding.parent().unwrap()).unwrap();
        fs::write(&binding, b"not a binding").unwrap();
        assert_eq!(sole(&src, &src).unwrap(), None);
    }

    /// A turn a Stop hands to the project's worker is still to save until the worker has captured
    /// it: queued, then marked while captured, with no moment between the two where it shows as
    /// saved. The end of a launched session reads this to say its last turn is still being saved;
    /// a check of the branch lock alone would miss a turn waiting behind another session's upload.
    /// A mark left by a worker that ended mid-capture does not count.
    #[test]
    fn a_turn_handed_to_the_project_worker_is_pending_until_captured() {
        if !isolated("a_turn_handed_to_the_project_worker_is_pending_until_captured") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let folder = project(work.path());
        save(&folder).unwrap();
        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: "dddddddd-0000-4000-8000-000000000005".into(),
            cwd: folder.root.clone(),
        };
        let pending = || capture_pending(&candidate.runtime, &candidate.session_id, &folder.root);
        assert!(!pending());
        enqueue(&folder.root, &candidate).unwrap();
        assert!(pending(), "a queued turn is pending");
        let (taken, capturing) = take_pending(&folder.root).unwrap().unwrap();
        assert_eq!(taken.session_id, candidate.session_id);
        assert!(markers(&folder.root).unwrap().is_empty());
        assert!(pending(), "a turn being captured is pending");
        *CAPTURING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(capturing);
        landed();
        assert!(
            !pending(),
            "a turn on its branch is not, whatever its upload still does"
        );

        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let pid = ended.id();
        ended.wait().unwrap();
        fs::write(
            Capturing::path(&folder.root).unwrap(),
            serde_json::to_vec(&CapturingRecord {
                session: hash(&key(&candidate.runtime, &candidate.session_id)),
                pid,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(!pending(), "an ended worker's mark is not pending");
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

    /// An explicit save of the parent folder leaves a session of a directory below that is set up
    /// on its own to that binding, also while its first reply has not finished: claiming it would
    /// file it in the parent's repository against the binding's own choices. The refusal names
    /// the directory, and nothing is claimed.
    #[test]
    fn a_save_leaves_a_nested_bindings_session_to_it() {
        if !isolated("a_save_leaves_a_nested_bindings_session_to_it") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let parent = project(work.path());
        save(&parent).unwrap();
        let nested = parent.root.join("nested");
        fs::create_dir(&nested).unwrap();
        let mut child = project(&nested);
        child.repository = "alice/nested".into();
        save(&child).unwrap();
        let id = "cccccccc-0000-4000-8000-000000000003";
        let saved = save_now(
            &parent.root,
            &[("claude-code".into(), id.into(), nested.clone())],
            true,
            false,
        )
        .unwrap();
        let [Saved::Refused(why)] = saved.as_slice() else {
            panic!("the nested session was not refused");
        };
        assert!(why.contains("set up on its own"), "{why}");
        assert!(why.contains("nested"), "{why}");
        let store = Store::at(config::store_root().unwrap());
        assert!(link::get(&store, "claude-code", id).is_none());
    }

    /// `agit project drop` is the person stopping one conversation, while `--all` and `--today` ask
    /// for the sessions from before the folder was on. A save asking for earlier sessions lets
    /// those in and refuses a dropped one, which stays withheld from automatic upload; only a save
    /// that names it lets it back. A policy written before the two sets were apart has every entry
    /// in `excluded`, so they all count as dropped. One that let any excluded session in on
    /// `--all` would upload a conversation the person said must not leave the computer.
    #[test]
    fn asking_for_earlier_sessions_never_lets_a_dropped_one_in() {
        if !isolated("asking_for_earlier_sessions_never_lets_a_dropped_one_in") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        let dropped = "dddddddd-0000-4000-8000-000000000041";
        let before = "eeeeeeee-0000-4000-8000-000000000042";
        policy.excluded.insert(key("claude-code", dropped));
        policy.earlier.insert(key("claude-code", before));
        save(&policy).unwrap();
        let session = |id: &str| {
            (
                "claude-code".to_string(),
                id.to_string(),
                policy.root.clone(),
            )
        };

        // What `--all` and `--today` pass: earlier sessions wanted, none named.
        let saved = save_now(
            &policy.root,
            &[session(dropped), session(before)],
            true,
            false,
        )
        .unwrap();
        let Saved::Refused(why) = &saved[0] else {
            panic!("a dropped session was saved");
        };
        assert!(why.contains("agit project drop"), "{why}");
        assert!(why.contains(&format!("agit save {dropped}")), "{why}");
        let after = read(&policy.root).unwrap().unwrap();
        assert!(after.excluded.contains(&key("claude-code", dropped)));
        assert!(!after.earlier.contains(&key("claude-code", before)));
        let origin = meta::CaptureOrigin {
            runtime: "claude-code".into(),
            session: dropped.into(),
        };
        assert_eq!(
            judge(std::slice::from_ref(&after), "any", &[], Some(&origin)),
            Some(Withholding::Excluded)
        );
        assert!(!left_out(
            &policy.root,
            "claude-code",
            dropped,
            &policy.root
        ));
        assert!(is_dropped(&policy.root, "claude-code", dropped));

        // A policy written before `earlier` existed reads with every entry dropped.
        let mut legacy = serde_json::to_value(&after).unwrap();
        assert!(
            legacy.get("earlier").is_none(),
            "an empty set is not written"
        );
        legacy["excluded"] = serde_json::json!([key("claude-code", before)]);
        let legacy: Project = serde_json::from_value(legacy).unwrap();
        assert!(legacy.earlier.is_empty() && legacy.leaves_out(&key("claude-code", before)));

        // Naming it lets it back in.
        save_now(&policy.root, &[session(dropped)], false, true).unwrap();
        assert!(
            !read(&policy.root)
                .unwrap()
                .unwrap()
                .leaves_out(&key("claude-code", dropped))
        );
    }

    /// A session uploaded and then continued while its Stop hook did not run (an untrusted hook,
    /// a crash) still has the branch it pushed, so a check of the branch alone calls it uploaded
    /// and a folder's `agit save` skips the new turns. Its transcript changed after the recorded
    /// upload, so it is waiting. A session uploaded before any record counts by its branch, and
    /// the summary the runtime writes after its Stop hook uploaded the turn changes nothing.
    #[test]
    fn a_turn_the_hook_missed_is_still_waiting() {
        if !isolated("a_turn_the_hook_missed_is_still_waiting") {
            return;
        }
        use crate::commands::front::{FolderSession, Upload};
        let work = tempfile::tempdir().unwrap();
        let project = project(work.path());
        save(&project).unwrap();
        let id = "eeeeeeee-0000-4000-8000-000000000005";
        let transcript = adapter::claude_code::session_path(id, &project.root).unwrap();
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, "{}\n").unwrap();
        let repo = Repo::init(&config::repo_dir("alice", "app").unwrap()).unwrap();
        repo.git(&["checkout", "-q", "-B", "kept"]).unwrap();
        fs::write(repo.root().join("turn"), "1").unwrap();
        repo.add_all().unwrap();
        repo.commit("turn").unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/kept", "kept"])
            .unwrap();
        let session = FolderSession {
            runtime: "claude-code",
            id: id.into(),
            path: transcript.clone(),
            cwd: project.root.clone(),
            modified: std::time::SystemTime::now(),
            title: None,
            saved: Some(("alice/app".into(), "kept".into())),
        };
        assert_eq!(session.upload(), Some(Upload::Uploaded));

        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: id.into(),
            cwd: project.root.clone(),
        };
        record_caught(
            &project.root,
            &candidate,
            transcript_modified(&candidate).unwrap(),
        )
        .unwrap();
        record_caught_length(
            &project.root,
            &candidate,
            transcript_length(&candidate).unwrap(),
        )
        .unwrap();
        assert_eq!(session.upload(), Some(Upload::Uploaded));

        // The runtime writes its own summary after the Stop hook uploaded the turn.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        file.write_all(b"{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}\n")
            .unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert!(!changed_since_upload("claude-code", id, &project.root));
        assert_eq!(session.upload(), Some(Upload::Uploaded));

        file.write_all(b"{}\n").unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();
        assert!(changed_since_upload("claude-code", id, &project.root));
        assert_eq!(session.upload(), Some(Upload::Waiting));
    }

    /// An automatic upload of a branch is refused when the folder its conversation ran in leaves
    /// that conversation out, however the branch names it, and while the folder is off or doesn't
    /// upload by itself, whatever repository the folder is bound to. A branch that recorded no
    /// directory has no folder to ask.
    #[test]
    fn the_folder_decides_whether_its_conversation_uploads_automatically() {
        if !isolated("the_folder_decides_whether_its_conversation_uploads_automatically") {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("folder");
        fs::create_dir(&work).unwrap();
        let mut policy = project(&work);
        policy.repository = "carol/elsewhere".into();
        save(&policy).unwrap();
        let repo = Repo::init(&temp.path().join("repo")).unwrap();
        repo.git(&["checkout", "-b", "work"]).unwrap();
        const SESSION: &str = "019a0000-0000-7000-8000-0000000000e1";
        let commit = |metadata: &Meta| {
            meta::write(repo.root(), metadata).unwrap();
            repo.add_all().unwrap();
            repo.commit("snapshot").unwrap();
        };
        let mut metadata = Meta::new(
            format!("agit-{}", "e".repeat(40)),
            "codex".into(),
            policy.root.to_string_lossy().into_owned(),
        );
        metadata.runtime_instances = vec![SESSION.into()];
        commit(&metadata);
        let refused = || folder_refuses_upload(&repo, "alice/app", "work", true).unwrap();
        assert!(!refused());

        policy.excluded.insert(key("codex", SESSION));
        save(&policy).unwrap();
        assert!(refused(), "named by its runtime instance");
        metadata.runtime_instances.clear();
        commit(&metadata);
        assert!(!refused());
        metadata.capture = Some(meta::CaptureOrigin {
            runtime: "codex".into(),
            session: SESSION.into(),
        });
        commit(&metadata);
        assert!(refused(), "named by its capture origin");
        assert!(
            folder_refuses_upload(&repo, "alice/app", "work", false).unwrap(),
            "a left-out conversation is held back from a push that doesn't name it"
        );
        policy.excluded.clear();
        policy.earlier.insert(key("codex", SESSION));
        save(&policy).unwrap();
        assert!(refused(), "from before the folder was on");

        policy.earlier.clear();
        policy.auto_upload = false;
        save(&policy).unwrap();
        assert!(refused(), "the folder doesn't upload by itself");
        assert!(
            !folder_refuses_upload(&repo, "alice/app", "work", false).unwrap(),
            "the person's own push isn't held back by the folder's upload setting"
        );
        policy.auto_upload = true;
        policy.enabled = false;
        save(&policy).unwrap();
        assert!(refused(), "the folder is off");
        policy.enabled = true;
        save(&policy).unwrap();
        assert!(!refused());

        metadata.cwd = String::new();
        commit(&metadata);
        assert!(!refused(), "no directory, no folder");
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
    /// push releases it; nor is one it refused for holding more event entries than it reads, while
    /// its tip still holds more than the limit it named. Checking only the project policy would
    /// start an automatic push on every turn and warn each time that it failed, after the refusal
    /// was already reported once; holding back a branch whose tip is within the limit would keep a
    /// session the Hub now accepts off it for good.
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

        // One event: `events/` then holds its four fan-out directories and the event itself.
        let session = Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            temp.path().to_string_lossy().into_owned(),
        );
        let log = crate::domain::transcript::wrap_lines(
            &format!(
                "{}\n",
                serde_json::json!({"type":"user","message":{"role":"user","content":"one turn"}})
            ),
            "claude-code",
            &session.session,
        );
        crate::domain::storage::write_snapshot(repo.root(), &log, &log).unwrap();
        meta::write(repo.root(), &session).unwrap();
        repo.add_all().unwrap();
        repo.commit("One turn").unwrap();
        crate::domain::privacy_git::mark_oversized(&repo, &identity, &refused, 4).unwrap();
        assert!(blocks_auto_push(&repo, "alice/app", "work").unwrap());
        crate::domain::privacy_git::mark_oversized(&repo, &identity, &refused, 5).unwrap();
        assert!(!blocks_auto_push(&repo, "alice/app", "work").unwrap());
        crate::domain::privacy_git::mark_oversized(&repo, &identity, &refused, 4).unwrap();
        crate::domain::privacy_git::clear_oversized(&repo, &identity, &refused).unwrap();
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

    /// An excluded branch says which exclusion keeps it, and names the session to save where
    /// saving lets it in: one from before the folder was on, or one the person dropped. A refusal
    /// that only said "excluded" would leave the person no way forward.
    #[test]
    fn an_exclusion_says_which_and_names_the_session_to_save() {
        if !isolated("an_exclusion_says_which_and_names_the_session_to_save") {
            return;
        }
        const SESSION: &str = "dddddddd-0000-4000-8000-000000000004";
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        let branch = capture_branch("claude-code", SESSION);
        policy.earlier.insert(key("claude-code", SESSION));
        save(&policy).unwrap();
        assert_eq!(
            withheld("alice/app", &branch).unwrap(),
            Some(Withholding::Excluded)
        );
        assert_eq!(
            exclusion("alice/app", &branch).unwrap(),
            Some(Exclusion::Earlier {
                folder: policy.root.clone(),
                session: SESSION.into(),
            })
        );
        // Dropped as well: the person's own drop is the one told.
        policy.excluded.insert(key("claude-code", SESSION));
        save(&policy).unwrap();
        assert_eq!(
            exclusion("alice/app", &branch).unwrap(),
            Some(Exclusion::Dropped {
                folder: policy.root.clone(),
                session: SESSION.into(),
            })
        );
        policy.excluded.clear();
        policy.earlier.clear();
        save(&policy).unwrap();
        assert_eq!(exclusion("alice/app", &branch).unwrap(), None);
        // A capture branch no claim records into is still withheld, for its unknown start.
        assert_eq!(
            withheld("alice/app", &branch).unwrap(),
            Some(Withholding::UnknownStart)
        );
    }

    /// `agit on` says nothing from Codex is saved until Codex runs AgentGit's hooks, and a person who
    /// stops trusting them makes that true again. A catch-up, a sync and a folder's `agit save`
    /// take a Codex session only when AgentGit saw a hook of that very session: hooks run for A,
    /// then are untrusted, and B runs; A is taken and B stays out until the person names it.
    /// `--all` and `--today` never let in a session the person dropped, hooks or not. A gate on
    /// the folder having seen hooks once takes B.
    #[test]
    fn a_codex_session_is_taken_only_when_its_own_hooks_ran() {
        if !isolated("a_codex_session_is_taken_only_when_its_own_hooks_ran") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut policy = project(work.path());
        save(&policy).unwrap();
        let root = policy.root.clone();
        const A: &str = "aaaaaaaa-0000-4000-8000-0000000000a1";
        const B: &str = "aaaaaaaa-0000-4000-8000-0000000000b2";
        const DROPPED: &str = "aaaaaaaa-0000-4000-8000-0000000000c3";
        let candidate = |id: &str| Candidate {
            runtime: "codex".into(),
            session_id: id.into(),
            cwd: root.clone(),
        };
        for id in [A, B] {
            assert!(!codex_capture_allowed(&root, id));
            assert!(codex_unhooked(&policy, &candidate(id)));
            assert!(left_out(&root, "codex", id, &root));
        }
        // A's hooks run; then the person untrusts them, and B runs without.
        codex_hooks_seen(A);
        assert!(codex_capture_allowed(&root, A));
        assert!(!codex_unhooked(&policy, &candidate(A)));
        assert!(!left_out(&root, "codex", A, &root));
        assert!(!codex_capture_allowed(&root, B));
        assert!(left_out(&root, "codex", B, &root));
        // A conversation's own hook saves it; that check is not this one.
        assert!(!withholds(&policy, &candidate(B), false));
        // An id that is not a plain name is never recorded, and never allowed.
        codex_hooks_seen("../escape");
        assert!(!codex_capture_allowed(&root, "../escape"));
        // Claude Code is not held back by Codex's hooks.
        assert!(!codex_unhooked(
            &policy,
            &Candidate {
                runtime: "claude-code".into(),
                session_id: B.into(),
                cwd: root.clone(),
            }
        ));

        // A dropped session stays out under `--all`, its hooks having run or not.
        codex_hooks_seen(DROPPED);
        policy.excluded.insert(key("codex", DROPPED));
        save(&policy).unwrap();
        let saved = save_now(
            &root,
            &[("codex".into(), DROPPED.into(), root.clone())],
            true,
            false,
        )
        .unwrap();
        let [Saved::Refused(why)] = saved.as_slice() else {
            panic!("a dropped Codex session was saved");
        };
        assert!(why.contains("agit project drop"), "{why}");
        assert!(
            read(&root)
                .unwrap()
                .unwrap()
                .excluded
                .contains(&key("codex", DROPPED))
        );
    }

    /// A push suggests turning a folder on once, only where no project ever governed it, never
    /// for agit's own automatic push or under --quiet, and never again after the person's no. The
    /// no changes nothing but its record: a paused folder keeps its binding and local saving, and
    /// a folder's drops and earlier sessions stay as they were, so turning it on later keeps them
    /// out. An offer due from the automatic entry would put the question to nobody who asked.
    #[test]
    fn a_push_offers_turning_a_folder_on_once_and_a_no_changes_nothing_else() {
        if !isolated("a_push_offers_turning_a_folder_on_once_and_a_no_changes_nothing_else") {
            return;
        }
        let due = crate::commands::front::offer_due;
        let fresh = tempfile::tempdir().unwrap();
        let fresh = fresh.path().canonicalize().unwrap();
        assert!(never_bound(&fresh));
        // A main checkout names the linked worktree `agit on` there would also cover.
        let checkout = tempfile::tempdir().unwrap();
        let main = checkout.path().join("main");
        git_checkout(&main);
        let tree = worktree(&main, &checkout.path().join("tree"));
        let main = main.canonicalize().unwrap();
        assert!(never_bound(&main));
        assert_eq!(worktrees_covered_from(&main), vec![tree]);
        assert!(!due(&fresh, true, false), "never for the automatic entry");
        assert!(!due(&fresh, false, true), "never under --quiet");
        assert!(due(&fresh, false, false));
        assert!(claim_automatic_upload_offer(&fresh).unwrap());
        assert!(!due(&fresh, false, false), "once per folder");
        assert!(!automatic_upload_declined(&fresh));
        // Two pushes that both found the offer due: only the first to claim it says it.
        assert!(!claim_automatic_upload_offer(&fresh).unwrap());
        let racing = tempfile::tempdir().unwrap();
        let racing = racing.path().canonicalize().unwrap();
        let claims: Vec<bool> = std::thread::scope(|scope| {
            let pushes: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| claim_automatic_upload_offer(&racing).unwrap()))
                .collect();
            pushes
                .into_iter()
                .map(|push| push.join().unwrap())
                .collect()
        });
        assert_eq!(claims.iter().filter(|claimed| **claimed).count(), 1);

        // A no is recorded and outlasts a later offer; turning the folder on forgets it.
        let declined = tempfile::tempdir().unwrap();
        let declined = declined.path().canonicalize().unwrap();
        // A push found the offer due, then the no was recorded while it waited on the Hub.
        assert!(due(&declined, false, false));
        record_automatic_upload_declined(&declined).unwrap();
        assert!(!claim_automatic_upload_offer(&declined).unwrap());
        assert!(automatic_upload_declined(&declined));
        assert!(!due(&declined, false, false));
        forget_automatic_upload_offer(&declined);
        assert!(!automatic_upload_offered(&declined));

        // A paused folder and a turned-off one are bound: no offer, and a no leaves them as they
        // were, drops and earlier sessions included.
        for (enabled, auto_upload) in [(true, false), (false, false)] {
            let work = tempfile::tempdir().unwrap();
            let mut policy = project(work.path());
            policy.enabled = enabled;
            policy.auto_upload = auto_upload;
            policy.excluded.insert(key("claude-code", "dropped"));
            policy.earlier.insert(key("claude-code", "before"));
            save(&policy).unwrap();
            assert!(!never_bound(&policy.root));
            assert!(!due(&policy.root, false, false));
            let before = serde_json::to_value(read(&policy.root).unwrap().unwrap()).unwrap();
            record_automatic_upload_declined(&policy.root).unwrap();
            assert!(automatic_upload_declined(&policy.root));
            let after = serde_json::to_value(read(&policy.root).unwrap().unwrap()).unwrap();
            assert_eq!(before, after);
        }
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
        // The folder it started in becoming a git worktree the project covers does not lift a
        // record already made: the judgement is not made again.
        git_checkout(&project.root);
        let outside = worktree(&project.root, &outside.path().canonicalize().unwrap());
        assert!(includes(&project, &outside).unwrap());
        assert_eq!(
            withheld("alice/app", strayed.branch.as_deref().unwrap()).unwrap(),
            Some(Withholding::StartedOutside)
        );

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

    /// An upload prepared under publication rules that changed before it was sent is run once more
    /// from the start, whichever side refused it: the Hub at the preview or at the receive, or
    /// this computer at the receive. A second such refusal is told in the person's words, as an
    /// upload outcome of its own; any other failure is passed on as it is. An implementation that
    /// retried only the preview refusal leaves a turn refused at the receive on this computer
    /// without a word; one that wrapped every second failure blames the rules for a network error.
    #[test]
    fn an_upload_whose_rules_changed_runs_once_more_and_then_says_so() {
        for refusal in [
            "publication did not complete (publication preview policy digest is stale; create a new preview)",
            "publication did not complete (HTTP 409 conflict: publication strategy changed; create a new preview)",
            crate::domain::privacy_git::POLICY_CHANGED,
        ] {
            let stale = || anyhow::anyhow!("{refusal}");
            let mut calls = 0;
            assert_eq!(
                retry_stale(|| {
                    calls += 1;
                    if calls == 1 {
                        Err(stale())
                    } else {
                        Ok("uploaded")
                    }
                })
                .unwrap(),
                "uploaded",
                "{refusal}"
            );
            let mut calls = 0;
            let twice = retry_stale(|| -> Result<()> {
                calls += 1;
                Err(stale())
            })
            .unwrap_err();
            assert_eq!(calls, 2, "{refusal}");
            assert!(twice.is::<RulesChanged>(), "{refusal}");
            let said = format!("{twice:#}");
            assert!(
                said.contains("changed while it was uploading, so the upload didn't finish")
                    && said.contains("the next turn or `agit save` uploads what's left"),
                "{said}"
            );
            assert!(!said.contains("nothing was sent"), "{said}");
            assert!(
                !said.contains("digest") && !said.contains("preview"),
                "{said}"
            );
            let other = retry_stale(|| -> Result<()> {
                calls += 1;
                if calls == 3 {
                    Err(stale())
                } else {
                    Err(anyhow::anyhow!("the Hub did not answer"))
                }
            })
            .unwrap_err();
            assert_eq!(format!("{other:#}"), "the Hub did not answer", "{refusal}");
        }
    }

    /// A failure saved while git's own summary of a refused push still led the message is read
    /// with the reason first, whether it was saved as text or as the child's diagnostics; that
    /// summary names only the URL.
    #[test]
    fn a_saved_failure_reads_with_its_reason_first() {
        assert_eq!(
            readable(
                r#"push failed: [{"level":"error","message":"failed to push some refs to 'https://hub/x.git'"},{"level":"error","message":"publication did not complete (HTTP 422 too many entries)"}]"#
            ),
            "push failed: publication did not complete (HTTP 422 too many entries)"
        );
        assert_eq!(
            readable(
                "push failed: failed to push some refs to 'https://hub/x.git'; publication did not complete (HTTP 422 too many entries); retry later"
            ),
            "push failed: publication did not complete (HTTP 422 too many entries); retry later"
        );
        assert_eq!(
            without_git_summary("push failed: timeout"),
            "push failed: timeout"
        );
    }

    /// A policy exit from the child is told by what it was. A Hub refusal is told as one, and a
    /// save offers no retry for it; a branch the folder keeps back is told as kept back; and only a
    /// privacy check that left a wait `agit status` lists is told as a privacy wait, since that
    /// line sends the person to status. A class read from the exit code alone tells a kept-back
    /// branch as a privacy wait that status never shows.
    #[test]
    fn a_policy_exit_is_told_by_what_it_was() {
        if !isolated("a_policy_exit_is_told_by_what_it_was") {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let policy = project(work.path());
        let candidate = Candidate {
            runtime: "claude-code".into(),
            session_id: "dddddddd-0000-4000-8000-000000000007".into(),
            cwd: policy.root.clone(),
        };
        let refused = format!(
            "publication did not complete (HTTP 422 provenance_rejected: x); no branch or tag refs were updated; {}",
            super::super::push::REFUSED_AGAIN
        );
        let child = |message: &str| {
            anyhow::Error::new(ChildFailed {
                exit_code: Some(ExitCode::Policy.as_i32()),
                message: message.into(),
            })
        };
        assert_eq!(
            UploadFailure::of(&child(&refused), &policy, &candidate),
            UploadFailure::Refused
        );
        let kept_back = format!(
            "`alice/app@talk` {}: agit cannot verify where the conversation started",
            super::super::push::WITHHELD_HERE
        );
        assert_eq!(
            UploadFailure::of(&child(&kept_back), &policy, &candidate),
            UploadFailure::Withheld
        );
        let pending = child("the privacy review did not finish");
        assert_eq!(
            UploadFailure::of(&pending, &policy, &candidate),
            UploadFailure::Other
        );
        crate::domain::privacy::pending::record(&candidate.session_id, "deadline", 1, None);
        assert_eq!(
            UploadFailure::of(&pending, &policy, &candidate),
            UploadFailure::PrivacyIncomplete
        );
        assert!(matches!(Saved::upload_failed(refused), Saved::Refused(_)));
        assert!(matches!(
            Saved::upload_failed(super::super::push::ENCRYPTED_REFUSED.into()),
            Saved::Refused(_)
        ));
        assert!(matches!(
            Saved::upload_failed("the Hub did not answer".into()),
            Saved::Failed(_)
        ));
    }
}
