//! Parse repository locations for viewing-key authentication.

use crate::hub::identity;
use anyhow::{Context, Result, ensure};

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
