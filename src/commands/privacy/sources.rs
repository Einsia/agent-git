//! Bind session publication to local policy and authenticated account/source restrictions.

use crate::domain::privacy::{PrivacyPolicy, mandatory::MandatoryPolicy};
use crate::domain::repo::Repo;
use crate::hub::{Client, client::ApiError, identity, privacy::sources::PolicySources};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(PartialEq, Eq)]
struct RepositoryBinding {
    identity: Option<identity::RemoteIdentity>,
    publication: Option<crate::rc::local_repository::publication::Destination>,
    origin: Option<String>,
    upstream: Option<String>,
}

impl RepositoryBinding {
    fn read(repo: &Repo) -> Result<Self> {
        Ok(Self {
            identity: identity::read(repo)?,
            publication: crate::rc::local_repository::publication::Destination::load(repo)?,
            origin: repo.remote_url(),
            upstream: repo.upstream_url(),
        })
    }
}

enum PolicyClient<'a> {
    Selected(&'a Client),
    Source(Box<Client>),
}

impl PolicyClient<'_> {
    fn client(&self) -> &Client {
        match self {
            Self::Selected(client) => client,
            Self::Source(client) => client,
        }
    }
}

struct BoundRules<'a> {
    client: PolicyClient<'a>,
    rules: PolicySources,
}

pub(crate) struct Sources<'a> {
    repository: Option<(PathBuf, RepositoryBinding)>,
    local_digest: String,
    remote: Vec<BoundRules<'a>>,
}

impl<'a> Sources<'a> {
    /// Local-only exports may omit an account. Remote identities still require fresh rules.
    pub(crate) fn resolve(
        repository: Option<(&Repo, &str)>,
        account: Option<&'a Client>,
    ) -> Result<Self> {
        Self::resolve_scopes(repository, account, true, &[])
    }

    /// Push already resolves its destination and immediate copy source. Retained remotes
    /// still constrain later publication, without adding a second copy of a checked scope.
    pub(crate) fn bound_repository(
        repo: &Repo,
        slug: &str,
        client: &'a Client,
        resolved: &[(&str, Option<&str>)],
    ) -> Result<Self> {
        Self::resolve_scopes(Some((repo, slug)), Some(client), false, resolved)
    }

    fn resolve_scopes(
        repository: Option<(&Repo, &str)>,
        account: Option<&'a Client>,
        include_account: bool,
        resolved: &[(&str, Option<&str>)],
    ) -> Result<Self> {
        let repo = repository.map(|(repo, _)| repo);
        let mut sources = Self {
            repository: repo
                .map(|repo| -> Result<_> {
                    Ok((repo.root().to_owned(), RepositoryBinding::read(repo)?))
                })
                .transpose()?,
            local_digest: repo
                .map_or_else(PrivacyPolicy::load_default, PrivacyPolicy::load)?
                .digest()?,
            remote: Vec::new(),
        };
        if include_account && let Some(client) = account {
            sources.remote.push(BoundRules {
                client: PolicyClient::Selected(client),
                rules: super::super::remote_request(client.privacy_account_policy_sources())?,
            });
        }
        if let Some((_, slug)) = repository {
            let binding = &sources.repository.as_ref().expect("repository binding").1;
            let slug = binding
                .publication
                .as_ref()
                .map_or(slug, |target| target.repository.as_str());
            let mut scopes = BTreeMap::new();
            for url in [&binding.origin, &binding.upstream].into_iter().flatten() {
                scopes.insert(remote_scope(url)?, (None, false));
            }
            if let Some(pin) = &binding.identity {
                // The local name and immutable pin retain the source even if origin is redirected.
                scopes.insert(
                    (pin.hub.clone(), slug.to_owned()),
                    (Some(pin.agent_id.clone()), false),
                );
            }
            if scopes.is_empty()
                && include_account
                && let Some(client) = account
            {
                scopes.insert(
                    (identity::normalize_hub(client.base())?, slug.to_owned()),
                    (None, true),
                );
            }
            for ((hub, slug), (pinned_id, allow_absent)) in scopes {
                if let Some(client) = account
                    && identity::normalize_hub(client.base())? == hub
                    && resolved.iter().any(|(known, id)| {
                        *known == slug && pinned_id.as_deref().is_none_or(|pin| Some(pin) == *id)
                    })
                {
                    continue;
                }
                let client = match account {
                    Some(client) if identity::normalize_hub(client.base())? == hub => {
                        PolicyClient::Selected(client)
                    }
                    _ => {
                        let credential = crate::infra::credentials::load_checked(&hub)?
                            .ok_or_else(|| super::super::LoginRequired::new(&hub))?;
                        PolicyClient::Source(Box::new(Client::for_credential(&hub, &credential)))
                    }
                };
                let agent_id = match pinned_id {
                    Some(id) => Some(id),
                    None => lookup_id(client.client(), &slug, allow_absent)?,
                };
                let rules = super::super::remote_request(
                    client
                        .client()
                        .privacy_policy_sources(&slug, agent_id.as_deref()),
                )?;
                sources.remote.push(BoundRules { client, rules });
            }
        }
        sources.verify_local()?;
        Ok(sources)
    }

    pub(crate) fn additional_rules(&self) -> Result<Vec<MandatoryPolicy>> {
        let mut rules = Vec::new();
        for scope in &self.remote {
            rules.extend(scope.rules.additional_rules()?);
        }
        Ok(rules)
    }

    pub(crate) fn verify(&self) -> Result<()> {
        self.verify_local()?;
        self.verify_remote()?;
        self.verify_local()
    }

    /// Caller-controlled repository promotion cannot drop previously resolved source rules.
    pub(crate) fn verify_remote(&self) -> Result<()> {
        for scope in &self.remote {
            super::super::remote_request(scope.rules.refresh(scope.client.client()))?;
        }
        Ok(())
    }

    fn verify_local(&self) -> Result<()> {
        let policy = match &self.repository {
            Some((root, binding)) => {
                let repo = Repo::open(root).context("the source repository is unavailable")?;
                ensure!(
                    &RepositoryBinding::read(&repo)? == binding,
                    "the source repository identity or remotes changed; prepare a fresh preview"
                );
                PrivacyPolicy::load(&repo)?
            }
            None => PrivacyPolicy::load_default()?,
        };
        ensure!(
            policy.digest()? == self.local_digest,
            "privacy policy changed during preparation; prepare a fresh preview"
        );
        Ok(())
    }
}

fn lookup_id(client: &Client, slug: &str, allow_absent: bool) -> Result<Option<String>> {
    let (owner, name) = checked_slug(slug)?;
    match super::super::remote_request(client.get_agent(&owner, &name)) {
        Ok(remote) => {
            ensure!(
                remote.slug() == slug,
                "the Hub returned another source repository"
            );
            Ok(Some(
                identity::RemoteIdentity::new(client.base(), &remote.agent_id)?.agent_id,
            ))
        }
        Err(error)
            if allow_absent
                && error
                    .downcast_ref::<ApiError>()
                    .is_some_and(|api| api.status == 404) =>
        {
            // The authenticated resolver must prove absence and namespace authority separately.
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn remote_scope(url: &str) -> Result<(String, String)> {
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
