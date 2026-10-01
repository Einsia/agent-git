//! Local repository identity is pinned independently of Hub names and credentials.
use crate::{domain::repo::Repo, rc::lineage::AgitSession};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

pub(crate) mod publication;

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
    let machine = super::identity::identity()?;
    require_identity(
        &repository,
        lineage.agent_id(),
        &machine.machine_fingerprint,
    )?;
    Ok(repository)
}

fn require_identity(repository: &Repo, agent_id: &str, machine_id: &str) -> crate::Result<()> {
    let config = repository.git(&[
        "config",
        "--local",
        "--null",
        "--get-regexp",
        r"^agit\.desktop(identity|authority)$",
    ])?;
    let mut id = None;
    let mut authority = None;
    // Git emits normalized keys and NUL-framed values; the last value wins.
    for record in config.split('\0') {
        match record.split_once('\n') {
            Some(("agit.desktopidentity", value)) => id = Some(value.trim()),
            Some(("agit.desktopauthority", value)) => authority = Some(value.trim()),
            _ => {}
        }
    }
    ensure!(id == Some(agent_id), "local repository identity changed");
    ensure!(
        authority == Some(format!("local:{machine_id}").as_str()),
        "repository belongs to a different local authority"
    );
    Ok(())
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
        // The launch boundary validates repository authority before spawning.
        return Ok(repository.clone());
    }
    let machine = super::identity::identity()?;
    let mut existing = repositories
        .values()
        .filter(|repo| repo.directory == directory);
    if let Some(candidate) = existing.next() {
        ensure!(
            existing.all(|repo| repo.agent_id == candidate.agent_id
                && repo.slug == candidate.slug
                && repo.authority == candidate.authority),
            "this directory has conflicting local repositories; reconcile their histories before binding another project"
        );
        let lineage = AgitSession::new(&candidate.slug, &candidate.agent_id, "main")?;
        let repo =
            Repo::open(&lineage.repo_dir()?).context("shared project repository is missing")?;
        require_identity(&repo, &candidate.agent_id, &machine.machine_fingerprint)?;
        let mut repository = candidate.clone();
        repository.project_id = project_id.into();
        repositories.insert(project_id.into(), repository.clone());
        super::save_json("repositories.json", &repositories)?;
        return Ok(repository);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Workspace aliases share an immutable repository; conflicting legacy histories are not selected by ordering.
    #[cfg(unix)]
    #[test]
    fn canonical_directory_aliases_share_publication_identity_without_selecting_conflicting_history()
     {
        if crate::rc::in_isolated_test(
            "rc::local_repository::tests::canonical_directory_aliases_share_publication_identity_without_selecting_conflicting_history",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let cwd = root.path().join("project");
        let alias = root.path().join("alias");
        std::fs::create_dir(&cwd).unwrap();
        std::os::unix::fs::symlink(&cwd, &alias).unwrap();
        let first = ensure_repository("workspace-a", &cwd).unwrap();
        let lineage = AgitSession::new(&first.slug, &first.agent_id, "main").unwrap();
        let repo = require(&lineage).unwrap();
        let target = crate::hub::identity::RemoteIdentity::new(
            "https://hub.example",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        publication::Selection::prepare(&repo, Some("owner/project"), &target.hub)
            .unwrap()
            .unwrap()
            .bind(&repo, &target)
            .unwrap();
        let second = ensure_repository("workspace-b", &alias).unwrap();
        assert_eq!(second.agent_id, first.agent_id);
        assert_eq!(second.slug, first.slug);
        assert_eq!(second.project_id, "workspace-b");
        let shared =
            require(&AgitSession::new(&second.slug, &second.agent_id, "main").unwrap()).unwrap();
        assert_eq!(
            publication::Destination::load(&shared)
                .unwrap()
                .unwrap()
                .identity,
            target
        );
        assert!(
            publication::Selection::prepare(&shared, Some("owner/other"), &target.hub).is_err()
        );
        let registry = crate::rc::rc_dir().unwrap().join("repositories.json");
        let mut records: BTreeMap<String, Repository> =
            serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
        let mut conflicting = first.clone();
        conflicting.project_id = "legacy".into();
        conflicting.agent_id = uuid::Uuid::new_v4().to_string();
        records.insert("legacy".into(), conflicting);
        std::fs::write(&registry, serde_json::to_vec(&records).unwrap()).unwrap();
        assert!(ensure_repository("workspace-c", &cwd).is_err());
        assert_eq!(
            ensure_repository("workspace-a", &cwd).unwrap().agent_id,
            first.agent_id
        );
    }

    #[test]
    fn local_identity_uses_last_values_without_accepting_embedded_keys() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        repo.git(&["config", "--local", "agit.desktopIdentity", "retired"])
            .unwrap();
        repo.git(&[
            "config",
            "--local",
            "--add",
            "agit.desktopIdentity",
            "agent",
        ])
        .unwrap();
        repo.git(&[
            "config",
            "--local",
            "agit.desktopAuthority",
            "local:machine",
        ])
        .unwrap();
        require_identity(&repo, "agent", "machine").unwrap();

        repo.git(&["config", "--local", "agit.desktopAuthority", "local:other"])
            .unwrap();
        assert!(require_identity(&repo, "agent", "machine").is_err());
        repo.git(&[
            "config",
            "--local",
            "agit.desktopAuthority",
            "local:machine",
        ])
        .unwrap();
        repo.git(&[
            "config",
            "--local",
            "--add",
            "agit.desktopIdentity",
            "agent\nagit.desktopauthority\nlocal:machine",
        ])
        .unwrap();
        assert!(require_identity(&repo, "agent", "machine").is_err());
        repo.git(&["config", "--local", "--unset-all", "agit.desktopIdentity"])
            .unwrap();
        assert!(require_identity(&repo, "agent", "machine").is_err());
    }
}
