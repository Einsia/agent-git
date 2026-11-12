//! A confirmed Hub destination does not replace the device's local repository identity.

use super::require_identity;
use crate::{
    domain::repo::Repo,
    hub::identity::{self, RemoteIdentity},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

const CONFIG: &str = "agit.desktopPublication";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Destination {
    version: u32,
    local_agent_id: String,
    pub repository: String,
    pub identity: RemoteIdentity,
}

fn local_id(repo: &Repo) -> crate::Result<Option<String>> {
    let Some(id) = repo.git_opt(&["config", "--local", "--get", "agit.desktopIdentity"]) else {
        return Ok(None);
    };
    let id = id.trim().to_owned();
    uuid::Uuid::parse_str(&id).context("invalid local repository identity")?;
    require_identity(
        repo,
        &id,
        &crate::rc::identity::desktop_identity()?.machine_fingerprint,
    )?;
    Ok(Some(id))
}

pub(crate) fn is_device_local(repo: &Repo) -> crate::Result<bool> {
    Ok(local_id(repo)?.is_some())
}

fn slug(value: &str) -> crate::Result<(String, String)> {
    let (owner, name) = crate::commands::parse_slug(value)?;
    ensure!(
        format!("{owner}/{name}") == value,
        "destination requires explicit owner/repository"
    );
    crate::rc::lineage::AgitSession::new(value, "00000000-0000-0000-0000-000000000001", "main")?;
    Ok((owner, name))
}

impl Destination {
    pub(crate) fn local_agent_id(&self) -> &str {
        &self.local_agent_id
    }

    pub(crate) fn load(repo: &Repo) -> crate::Result<Option<Self>> {
        let Some(raw) = repo.git_opt(&["config", "--local", "--get", CONFIG]) else {
            return Ok(None);
        };
        ensure!(
            raw.len() <= 8192,
            "local publication binding exceeds its limit"
        );
        let saved: Self =
            serde_json::from_str(&raw).context("invalid local publication binding")?;
        ensure!(saved.version == 1, "unsupported local publication binding");
        ensure!(
            local_id(repo)?.as_deref() == Some(&saved.local_agent_id),
            "local publication identity changed"
        );
        slug(&saved.repository)?;
        ensure!(
            RemoteIdentity::new(&saved.identity.hub, &saved.identity.agent_id)? == saved.identity
                && identity::read(repo)?.as_ref() == Some(&saved.identity),
            "local publication destination identity changed"
        );
        Ok(Some(saved))
    }
}

pub(crate) struct Selection {
    local_agent_id: String,
    previous: Option<Destination>,
    hub: String,
    pub repository: String,
}

impl Selection {
    pub(crate) fn prepare(
        repo: &Repo,
        requested: Option<&str>,
        hub: &str,
    ) -> crate::Result<Option<Self>> {
        let previous = Destination::load(repo)?;
        let Some(local_agent_id) = local_id(repo)? else {
            ensure!(
                requested.is_none(),
                "--to requires a device-local RC repository"
            );
            return Ok(None);
        };
        let hub = identity::normalize_hub(hub)?;
        let repository = requested
            .or_else(|| previous.as_ref().map(|saved| saved.repository.as_str()))
            .context("this local RC repository has no publication target; review an explicit push with --to owner/repo first")?
            .to_owned();
        slug(&repository)?;
        if let Some(saved) = &previous {
            ensure!(
                saved.repository == repository && saved.identity.hub == hub,
                "the confirmed RC publication destination cannot be replaced by this push"
            );
        }
        Ok(Some(Self {
            local_agent_id,
            previous,
            hub,
            repository,
        }))
    }

    /// Whether this gives the repository its first destination.
    pub(crate) fn is_first(&self) -> bool {
        self.previous.is_none()
    }

    pub(crate) fn checkout(
        &self,
        path: &std::path::Path,
    ) -> crate::Result<crate::commands::clone::Checkout> {
        let (owner, name) = slug(&self.repository)?;
        Ok(crate::commands::clone::Checkout {
            owner,
            name,
            path: path.into(),
        })
    }

    pub(crate) fn verify(&self, repo: &Repo) -> crate::Result<()> {
        ensure!(
            local_id(repo)?.as_deref() == Some(&self.local_agent_id)
                && Destination::load(repo)? == self.previous,
            "local RC publication binding changed during review"
        );
        Ok(())
    }

    pub(crate) fn bind(&self, repo: &Repo, observed: &RemoteIdentity) -> crate::Result<()> {
        self.verify(repo)?;
        ensure!(observed.hub == self.hub, "local RC publication Hub changed");
        if let Some(saved) = &self.previous {
            ensure!(
                &saved.identity == observed,
                "local RC publication identity changed"
            );
            return Ok(());
        }
        identity::pin(repo, observed)?;
        let saved = Destination {
            version: 1,
            local_agent_id: self.local_agent_id.clone(),
            repository: self.repository.clone(),
            identity: observed.clone(),
        };
        repo.git(&["config", "--local", CONFIG, &serde_json::to_string(&saved)?])?;
        Ok(())
    }
}

/// The folders whose conversations device-local `repo` holds, and its projects.
/// The destination chosen for `root` under the account signed in to `hub`, with its key. A folder
/// without a record yet takes the person's own terminal choice for it, when that choice is this
/// account's. `None` with nobody signed in, or nothing chosen.
fn folder_record(
    root: &std::path::Path,
    hub: &str,
) -> crate::Result<
    Option<(
        crate::rc::folder_destination::Key,
        crate::rc::folder_destination::Chosen,
    )>,
> {
    use crate::rc::folder_destination as folder;
    let Some(key) = folder::Key::signed_in(root, hub)? else {
        return Ok(None);
    };
    if folder::chosen(&key)?.is_none()
        && let Some((repository, identity, account)) =
            crate::commands::project::folder_choice(key.root())
        && account == key.account()
        && identity.hub == identity::normalize_hub(hub)?
    {
        folder::adopt(&key, &repository, &identity.agent_id)?;
    }
    Ok(folder::chosen(&key)?.map(|chosen| (key, chosen)))
}

fn folders_and_projects(repo: &Repo) -> crate::Result<(Vec<std::path::PathBuf>, Vec<String>)> {
    let mut folders = Vec::new();
    let mut projects = Vec::new();
    for (project, folder) in super::projects_of(repo)? {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
        projects.push(project);
    }
    Ok((folders, projects))
}

/// Gives device-local `repo` the destination chosen for its folder under the account signed in to
/// `hub`, when it has none yet. A folder without a record yet takes the person's own terminal
/// choice for it, when that choice is this account's. A repository that has a destination keeps
/// it: its conversations keep the destination they started with, and a folder whose destination
/// changed starts new conversations in a repository of their own
/// ([`super::ensure_repository`]). Nothing changes for a repository no daemon here registered, or
/// with nobody signed in.
pub(crate) fn follow_folder(repo: &Repo, hub: &str) -> crate::Result<()> {
    let Some(local_agent_id) = local_id(repo)? else {
        return Ok(());
    };
    let (folders, _) = folders_and_projects(repo)?;
    let [root] = folders.as_slice() else {
        return Ok(());
    };
    let Some((_, chosen)) = folder_record(root, hub)? else {
        return Ok(());
    };
    if Destination::load(repo)?.is_some() {
        return Ok(());
    }
    let target = RemoteIdentity::new(hub, &chosen.repository_id)?;
    slug(&chosen.repository)?;
    identity::pin(repo, &target)?;
    let saved = Destination {
        version: 1,
        local_agent_id,
        repository: chosen.repository,
        identity: target,
    };
    repo.git(&["config", "--local", CONFIG, &serde_json::to_string(&saved)?])?;
    Ok(())
}

/// Whether the conversation on `branch` of device-local `repo` may upload to `saved`. One whose
/// first upload went there keeps uploading there, under the account that sent it. Any other is
/// judged at its start: `saved` must be the destination chosen for its folder under the account
/// signed in there, for the project that started it (see
/// [`crate::rc::folder_destination::allows`]). A conversation recorded outside a device-local
/// repository was started at the terminal and keeps the terminal's destination. A repository no
/// daemon here registered is judged as before; with nobody signed in, the sign-in check that
/// follows decides.
pub(crate) fn confirmed(
    repo: &Repo,
    branch: &str,
    saved: &RemoteIdentity,
    project: Option<&str>,
) -> crate::Result<bool> {
    use crate::rc::folder_destination as folder;
    if local_id(repo)?.is_none() {
        return Ok(true);
    }
    let (folders, projects) = folders_and_projects(repo)?;
    if folders.is_empty() {
        return Ok(true);
    }
    let account = crate::infra::credentials::account_key(&saved.hub);
    if let Some(pinned) = super::session_destination(repo, branch)? {
        return Ok(account.is_none() || pinned == pin_value(saved, account.as_deref()));
    }
    let project = super::session_project(repo, branch)?.or(project.map(str::to_owned));
    for root in &folders {
        let Some(key) = folder::Key::signed_in(root, &saved.hub)? else {
            return Ok(true);
        };
        if !folder::allows(&key, project.as_deref(), &projects, &saved.agent_id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn pin_value(destination: &RemoteIdentity, account: Option<&str>) -> String {
    format!("{} {}", destination.agent_id, account.unwrap_or_default())
}

/// Records that the conversation on `branch` of device-local `repo` uploads to `destination`
/// from now on, under the account signed in there, once its first upload is allowed.
pub(crate) fn keep_destination(
    repo: &Repo,
    branch: &str,
    destination: &RemoteIdentity,
) -> crate::Result<()> {
    if local_id(repo)?.is_none() {
        return Ok(());
    }
    let Some(account) = crate::infra::credentials::account_key(&destination.hub) else {
        return Ok(());
    };
    super::pin_session_destination(repo, branch, &pin_value(destination, Some(&account)))
}

/// For a one-off `agit push --to` that gave device-local `repo` its first destination: makes it
/// the folder's too, when the folder has none yet, and returns the line that says so.
pub(crate) fn first_choice(
    repo: &Repo,
    repository: &str,
    identity: &RemoteIdentity,
) -> crate::Result<Option<String>> {
    use crate::rc::folder_destination as folder;
    let (folders, _) = folders_and_projects(repo)?;
    let [root] = folders.as_slice() else {
        return Ok(None);
    };
    let Some(key) = folder::Key::signed_in(root, &identity.hub)? else {
        return Ok(None);
    };
    Ok(folder::choose_first(&key, repository, &identity.agent_id)?
        .then(|| format!("this folder now uploads to {repository}")))
}
