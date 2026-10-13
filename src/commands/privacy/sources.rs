//! Parse repository locations for original-access authentication.

use crate::domain::repo::Repo;
use crate::hub::identity::{self, RemoteIdentity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    version: u32,
    identity: RemoteIdentity,
    repository: String,
}

/// A verified copy retains its source route independently of mutable upstream configuration.
pub(crate) fn retain(repo: &Repo, source: &RemoteIdentity, url: &str) -> Result<()> {
    let (hub, repository) = remote_scope(url)?;
    ensure!(
        RemoteIdentity::new(&source.hub, &source.agent_id)? == *source && hub == source.hub,
        "copy source route differs from its immutable identity"
    );
    let pin = identity::read(repo)?.context("copy checkout has no immutable identity")?;
    ensure!(pin.hub == hub, "copy source belongs to another Hub");
    let parent = repo.common_dir()?.join("agit/privacy-sources");
    crate::infra::config::create_state_dir(&parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(&parent)?;
    staged.write_all(&serde_json::to_vec(&Source {
        version: 1,
        identity: source.clone(),
        repository,
    })?)?;
    staged.as_file().sync_all()?;
    staged
        .persist(parent.join(format!("{}.json", source.agent_id)))
        .map_err(|error| error.error)?;
    Ok(())
}

pub(crate) fn retained(repo: &Repo, source: &RemoteIdentity) -> Result<Option<String>> {
    ensure!(
        RemoteIdentity::new(&source.hub, &source.agent_id)? == *source,
        "invalid source publication identity"
    );
    let path = repo
        .common_dir()?
        .join("agit/privacy-sources")
        .join(format!("{}.json", source.agent_id));
    let bytes = match crate::domain::storage::read_bytes_capped(&path, 4096) {
        Ok(bytes) => bytes,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let saved: Source = serde_json::from_slice(&bytes)?;
    ensure!(
        saved.version == 1 && saved.identity == *source,
        "retained publication source differs from the snapshot authority"
    );
    checked_slug(&saved.repository)?;
    Ok(Some(saved.repository))
}

pub(crate) fn remote_scope(url: &str) -> Result<(String, String)> {
    let url = identity::normalize_hub(url)?;
    let (base, name) = url
        .rsplit_once('/')
        .context("invalid privacy source remote")?;
    let (hub, owner) = base
        .rsplit_once('/')
        .context("invalid privacy source remote")?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    let slug = format!("{owner}/{name}");
    checked_slug(&slug)?;
    Ok((identity::normalize_hub(hub)?, slug))
}

fn checked_slug(slug: &str) -> Result<(String, String)> {
    let (owner, name) = super::super::parse_slug(slug)?;
    ensure!(
        [&owner, &name].into_iter().all(|part| {
            crate::domain::privacy_envelope::valid_token(part) && part != "." && part != ".."
        }) && format!("{owner}/{name}") == slug,
        "invalid privacy source repository"
    );
    Ok((owner, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_scopes_preserve_hub_mounts_and_reject_ambiguous_authorities() {
        assert_eq!(
            remote_scope("https://HUB.example/Mount/team/repo.git").unwrap(),
            ("https://hub.example/Mount".into(), "team/repo".into())
        );
        for url in [
            "git@hub.example:team/repo.git",
            "https://user:password@hub.example/team/repo.git",
            "https://hub.example/team/repo.git?redirect=other",
            "https://hub.example/repo.git",
            "https://hub.example/team/..",
            "https://hub.example/team/escaped%2frepo.git",
        ] {
            assert!(remote_scope(url).is_err());
        }
    }
}
