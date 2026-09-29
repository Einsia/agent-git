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
