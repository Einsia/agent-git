//! A separately confirmed destination retains its own identity without rebinding the source.

use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, PartialEq, Eq)]
struct SourceBinding {
    identity: Option<RemoteIdentity>,
    origin: Option<String>,
    upstream: Option<String>,
    local_publication: Option<crate::rc::local_repository::publication::Destination>,
    desktop_identity: Option<String>,
    desktop_authority: Option<String>,
}

impl SourceBinding {
    fn read(repo: &Repo) -> Result<Self> {
        Ok(Self {
            identity: identity::read(repo)?,
            origin: repo.remote_url(),
            upstream: repo.upstream_url(),
            local_publication: crate::rc::local_repository::publication::Destination::load(repo)?,
            desktop_identity: repo.git_opt(&[
                "config",
                "--local",
                "--no-includes",
                "--get",
                "agit.desktopIdentity",
            ]),
            desktop_authority: repo.git_opt(&[
                "config",
                "--local",
                "--no-includes",
                "--get",
                "agit.desktopAuthority",
            ]),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    repository: String,
    identity: RemoteIdentity,
    encryption_enabled: bool,
}

#[derive(Debug)]
pub(super) struct Selection {
    hub: String,
    pub repository: String,
    path: PathBuf,
    previous: Option<Binding>,
    source: SourceBinding,
}

impl Selection {
    pub(super) fn prepare(
        repo: &Repo,
        source_slug: &str,
        requested: &str,
        hub: &str,
    ) -> Result<Self> {
        ensure!(
            std::env::var_os(crate::commands::auto_push::AUTOMATIC_ENV).is_none()
                && std::env::var_os(crate::domain::privacy_receipt::SUPERVISOR_RESULT_ENV)
                    .is_none()
                && std::env::var_os(identity::EXPECTED_AGENT_ID_ENV).is_none(),
            "a separate destination requires an explicit unsupervised push; automatic and RC publication retain their confirmed destination"
        );
        let (owner, name) = crate::commands::parse_slug(requested)?;
        crate::commands::canonical_owner(&owner)?;
        crate::domain::repo::valid_name(&owner)?;
        crate::domain::repo::valid_name(&name)?;
        ensure!(
            requested == format!("{owner}/{name}") && requested != source_slug,
            "--to requires a different explicit owner/repository; use ordinary push for the source destination"
        );
        let hub = identity::normalize_hub(hub)?;
        let key = hex::encode(Sha256::digest(serde_json::to_vec(&(&hub, requested))?));
        let path = repo
            .common_dir()?
            .join("agit/publication-targets")
            .join(format!("{key}.json"));
        let previous = read(&path)?;
        if let Some(saved) = &previous {
            ensure!(
                saved.repository == requested && saved.identity.hub == hub,
                "the retained publication target belongs to another destination"
            );
        }
        Ok(Self {
            hub,
            repository: requested.into(),
            path,
            previous,
            source: SourceBinding::read(repo)?,
        })
    }

    pub(super) fn checkout(&self, path: &Path) -> Result<Checkout> {
        let (owner, name) = crate::commands::parse_slug(&self.repository)?;
        Ok(Checkout {
            owner,
            name,
            path: path.into(),
        })
    }

    pub(super) fn encryption_for_creation(
        &self,
        repo: &Repo,
        explicit: Option<bool>,
    ) -> Result<bool> {
        if self.source.identity.is_none() && self.source.origin.is_none() {
            repo.encryption_for_creation(explicit)
        } else {
            match explicit {
                Some(enabled) => Ok(enabled),
                None => config::encryption_default(),
            }
        }
    }

    pub(super) fn verify_lookup(&self, remote: Option<&RemoteAgent>) -> Result<()> {
        if let Some(saved) = &self.previous {
            let remote = remote.context(
                "the confirmed publication target is unavailable; refusing to create a replacement",
            )?;
            ensure!(
                RemoteIdentity::new(&self.hub, &remote.agent_id)? == saved.identity
                    && remote.require_encryption_enabled()? == saved.encryption_enabled,
                "the confirmed publication target changed identity or fixed mode; choose a different repository"
            );
        }
        if let Some(remote) = remote {
            ensure!(
                self.source.identity.as_ref()
                    != Some(&RemoteIdentity::new(&self.hub, &remote.agent_id)?)
                    && self.source.origin.as_deref() != Some(remote.clone_url.as_str()),
                "--to identifies the source repository; a different mode requires a different repository identity"
            );
        }
        Ok(())
    }

    pub(super) fn verify_source(&self, repo: &Repo) -> Result<()> {
        ensure!(
            SourceBinding::read(repo)? == self.source,
            "the source repository identity or remotes changed during publication"
        );
        Ok(())
    }

    pub(super) fn verify(&self, repo: &Repo) -> Result<()> {
        self.verify_source(repo)?;
        ensure!(
            read(&self.path)? == self.previous,
            "the separate publication binding changed during review"
        );
        Ok(())
    }

    pub(super) fn bind(&self, repo: &Repo, remote: &Remote) -> Result<()> {
        self.verify(repo)?;
        ensure!(
            remote.identity.hub == self.hub
                && format!("{}/{}", remote.owner, remote.name) == self.repository,
            "the separate publication target differs from the confirmed destination"
        );
        let saved = Binding {
            version: 1,
            repository: self.repository.clone(),
            identity: remote.identity.clone(),
            encryption_enabled: remote.encryption_enabled,
        };
        if let Some(previous) = &self.previous {
            ensure!(
                *previous == saved,
                "the separate publication identity or fixed mode changed"
            );
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .context("publication binding has no directory")?;
        config::create_state_dir(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec(&saved)?)?;
        file.as_file().sync_all()?;
        if let Err(error) = file.persist_noclobber(&self.path) {
            if error.error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.error.into());
            }
            ensure!(
                read(&self.path)?.as_ref() == Some(&saved),
                "the separate publication identity changed concurrently"
            );
        }
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn read(path: &Path) -> Result<Option<Binding>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "publication binding is not a regular file"
        ),
    }
    let saved: Binding =
        serde_json::from_slice(&crate::domain::storage::read_bytes_capped(path, 8192)?)?;
    ensure!(
        saved.version == 1
            && RemoteIdentity::new(&saved.identity.hub, &saved.identity.agent_id)?
                == saved.identity,
        "invalid separate publication binding"
    );
    Ok(Some(saved))
}
