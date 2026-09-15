//! Local repository identity is pinned independently of Hub names and credentials.
use crate::{domain::repo::Repo, rc::lineage::AgitSession};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    pub slug: String,
    pub agent_id: String,
    pub project_id: String,
    pub directory: String,
    pub authority: String,
}

pub fn require(lineage: &AgitSession) -> crate::Result<Repo> {
    let repository = Repo::open(&lineage.repo_dir()?).context("local repository is missing")?;
    let id = repository.git(&["config", "--local", "agit.desktopIdentity"])?;
    let authority = repository.git(&["config", "--local", "agit.desktopAuthority"])?;
    let machine = super::identity::identity()?;
    ensure!(
        id.trim() == lineage.agent_id(),
        "local repository identity changed"
    );
    ensure!(
        authority.trim() == format!("local:{}", machine.machine_fingerprint),
        "repository belongs to a different local authority"
    );
    Ok(repository)
}

pub fn ensure_repository(project_id: &str, directory: &Path) -> crate::Result<Repository> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(super::rc_dir()?.join("repositories.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let path = super::rc_dir()?.join("repositories.json");
    let mut repositories: BTreeMap<String, Repository> = match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("local repository registry is invalid")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(error.into()),
    };
    let directory = directory.canonicalize()?.to_string_lossy().to_string();
    if let Some(repository) = repositories.get(project_id) {
        ensure!(
            repository.directory == directory,
            "project directory changed; select a new project identity"
        );
        require(&AgitSession::new(
            &repository.slug,
            &repository.agent_id,
            "main",
        )?)?;
        return Ok(repository.clone());
    }
    let machine = super::identity::identity()?;
    let id = uuid::Uuid::new_v4().to_string();
    let owner = format!("desktop-{}", machine.machine_fingerprint);
    let slug = format!("{owner}/project-{id}");
    let lineage = AgitSession::new(&slug, &id, "main")?;
    let repo = Repo::open_or_init(&lineage.repo_dir()?)?;
    repo.set_auto_push(Some(false))?;
    let authority = format!("local:{}", machine.machine_fingerprint);
    repo.git(&["config", "--local", "agit.desktopIdentity", &id])?;
    repo.git(&["config", "--local", "agit.desktopAuthority", &authority])?;
    let repository = Repository {
        slug,
        agent_id: id,
        project_id: project_id.into(),
        directory,
        authority,
    };
    repositories.insert(project_id.into(), repository.clone());
    super::save_json("repositories.json", &repositories)?;
    Ok(repository)
}
