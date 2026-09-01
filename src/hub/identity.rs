//! The repository-local immutable remote identity.
//!
//! `owner/name` is routing, not identity: once the remote is deleted, the same name can be
//! recreated. Keeping only the URL in `origin` lets an old checkout silently write into the new
//! repository on its next push. Every repository fetched or published through a hub therefore
//! pins a `(hub, agent_id)` in `.git/config`.
//!
//! The two fields are stored as **one** JSON config value, not as two git config keys.
//! `git config` updates a single value with `config.lock` + rename; whatever point a process
//! dies at, a reader sees either the old pair or the new pair, never a torn "new hub + old id"
//! state.

use crate::domain::repo::Repo;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

const CONFIG_KEY: &str = "agit.remoteIdentity";
pub const EXPECTED_AGENT_ID_HEADER: &str = "X-AgentGit-Expected-Agent-Id";
pub const EXPECTED_AGENT_ID_ENV: &str = "AGIT_EXPECTED_AGENT_ID";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteIdentity {
    pub hub: String,
    pub agent_id: String,
}

impl RemoteIdentity {
    pub fn new(hub: &str, agent_id: &str) -> crate::Result<Self> {
        let agent_id = agent_id.trim();
        let parsed = uuid::Uuid::parse_str(agent_id)
            .map_err(|_| anyhow::anyhow!("the hub returned an invalid agent_id `{agent_id}`"))?;
        Ok(Self {
            hub: normalize_hub(hub)?,
            agent_id: parsed.to_string(),
        })
    }
}

/// Normalize a hub base URL: scheme / authority are case-insensitive, and a trailing slash is
/// not part of the identity.
///
/// The path keeps its case because a reverse-proxy mount point may distinguish it; a query /
/// fragment is not a valid base URL.
pub fn normalize_hub(hub: &str) -> crate::Result<String> {
    let hub = hub.trim().trim_end_matches('/');
    let (scheme, rest) = hub
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("hub URL must include http:// or https://"))?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        bail!("hub URL must use http or https (got `{scheme}`)");
    }
    if rest.contains(['?', '#']) {
        bail!("hub URL must not contain a query or fragment");
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.trim().is_empty() {
        bail!("hub URL has no host");
    }
    if authority.contains('@') {
        bail!("hub URL must not contain credentials");
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    let path = path.trim_end_matches('/');
    Ok(if path.is_empty() {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}://{authority}/{path}")
    })
}

/// Read the pin. A corrupt value is a hard error; it must not fall through as "no pin" and adopt
/// the current slug.
pub fn read(repo: &Repo) -> crate::Result<Option<RemoteIdentity>> {
    let Some(raw) = repo.git_opt(&["config", "--local", "--get", CONFIG_KEY]) else {
        return Ok(None);
    };
    let parsed: RemoteIdentity = serde_json::from_str(raw.trim()).with_context(|| {
        format!(
            "the remote identity in {} is malformed; refusing to guess which remote this checkout belongs to",
            repo.root().join(".git/config").display()
        )
    })?;
    // A non-canonical stored form is normalized on read; no value is silently accepted under a
    // second comparison rule.
    RemoteIdentity::new(&parsed.hub, &parsed.agent_id).map(Some)
}

/// Write the pin for the first time; re-running with the same value is a no-op, and a different
/// value must go through [`rebind`].
pub fn pin(repo: &Repo, identity: &RemoteIdentity) -> crate::Result<()> {
    match read(repo)? {
        Some(existing) if existing == *identity => return Ok(()),
        Some(existing) => bail!(
            "this checkout is pinned to {} / {}, not {} / {}; refusing to replace its remote identity",
            existing.hub,
            existing.agent_id,
            identity.hub,
            identity.agent_id
        ),
        None => {}
    }
    write(repo, identity)
}

/// Explicit identity migration (only for the promotion flow where the server has just created a
/// copy and returned a new id).
pub fn rebind(
    repo: &Repo,
    expected: &RemoteIdentity,
    replacement: &RemoteIdentity,
) -> crate::Result<()> {
    let actual = read(repo)?.ok_or_else(|| {
        anyhow::anyhow!(
            "this is a legacy checkout with no immutable remote identity; re-clone it before changing ownership"
        )
    })?;
    if actual != *expected {
        bail!(
            "this checkout is pinned to {} / {}, not the source identity {} / {}; refusing to rebind it",
            actual.hub,
            actual.agent_id,
            expected.hub,
            expected.agent_id
        );
    }
    write(repo, replacement)
}

/// Take the pin before a network request to the current hub, and reject a legacy checkout /
/// cross-hub use.
pub fn require_current(repo: &Repo, hub: &str) -> crate::Result<RemoteIdentity> {
    let identity = read(repo)?.ok_or_else(|| {
        anyhow::anyhow!(
            "this is a legacy checkout with no immutable remote identity; re-run `agit clone <owner>/<agent>` into a fresh checkout before network access"
        )
    })?;
    let current = normalize_hub(hub)?;
    if identity.hub != current {
        bail!(
            "this checkout belongs to {}, but the current hub is {}; refusing to send it to a different hub",
            identity.hub,
            current
        );
    }
    Ok(identity)
}

/// Read the repository pin and, when an RC lineage supplied an expected ID in
/// the environment, require the two identities to be byte-for-byte the same.
///
/// The environment value only narrows authority: ordinary CLI processes do
/// not set it and keep using the repo pin, while RC land/hook/push cannot be
/// redirected by swapping the checkout underneath a running session.
pub fn require_current_expected(repo: &Repo, hub: &str) -> crate::Result<RemoteIdentity> {
    let pinned = require_current(repo, hub)?;
    constrain_expected(
        pinned,
        hub,
        std::env::var_os(EXPECTED_AGENT_ID_ENV)
            .as_deref()
            .map(std::ffi::OsStr::to_string_lossy)
            .as_deref(),
    )
}

fn constrain_expected(
    pinned: RemoteIdentity,
    hub: &str,
    expected_agent_id: Option<&str>,
) -> crate::Result<RemoteIdentity> {
    let Some(raw) = expected_agent_id else {
        return Ok(pinned);
    };
    let expected = RemoteIdentity::new(hub, raw)?;
    if pinned != expected {
        bail!(
            "this RC session expects agent {}, but the checkout is pinned to {}; refusing to settle or push it",
            expected.agent_id,
            pinned.agent_id
        );
    }
    Ok(pinned)
}

/// Before reading or writing the remote a slug points at, prove it is still the one object the
/// repo pin names.
///
/// The Expected-Agent-Id header is the main line of server-side fencing; this up-front comparison
/// stays necessary because a hub still mid-rollout may run an upload-pack that does not enforce
/// the header yet. In that window the client must not fetch the history of a same-named new
/// repository into an old checkout, and must not use its advertised heads to narrow the secret
/// scan surface.
pub fn verify_slug(
    repo: &Repo,
    client: &super::Client,
    owner: &str,
    name: &str,
) -> crate::Result<super::RemoteAgent> {
    let pinned = require_current(repo, client.base())?;
    let remote = client.get_agent(owner, name)?;
    let observed = RemoteIdentity::new(client.base(), &remote.agent_id)?;
    if observed != pinned {
        bail!(
            "{owner}/{name} now identifies agent {}, but this checkout is pinned to {}; refusing a reused remote name",
            observed.agent_id,
            pinned.agent_id
        );
    }
    Ok(remote)
}

fn write(repo: &Repo, identity: &RemoteIdentity) -> crate::Result<()> {
    let value = serde_json::to_string(identity)?;
    // One key carries the whole pair; git itself replaces the config file atomically through
    // config.lock.
    repo.git(&["config", "--local", CONFIG_KEY, &value])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> String {
        uuid::Uuid::from_u128(n).to_string()
    }

    #[test]
    fn hub_normalization_is_stable_but_preserves_mount_path_case() {
        assert_eq!(
            normalize_hub(" HTTPS://Hub.Example.COM/AgentGit/// ").unwrap(),
            "https://hub.example.com/AgentGit"
        );
        assert!(normalize_hub("ssh://hub/x").is_err());
        assert!(normalize_hub("https://hub/x?q=1").is_err());
    }

    #[test]
    fn the_pair_is_one_atomic_config_value_and_cannot_be_silently_replaced() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let first = RemoteIdentity::new("HTTPS://HUB.test/", &id(1)).unwrap();
        pin(&repo, &first).unwrap();
        assert_eq!(read(&repo).unwrap(), Some(first.clone()));

        let raw = repo
            .git_opt(&["config", "--local", "--get", CONFIG_KEY])
            .unwrap();
        assert_eq!(
            raw.lines().count(),
            1,
            "the pair must live in one config value"
        );
        assert_eq!(serde_json::from_str::<RemoteIdentity>(&raw).unwrap(), first);

        let other = RemoteIdentity::new("https://hub.test", &id(2)).unwrap();
        assert!(pin(&repo, &other).is_err());
        assert_eq!(read(&repo).unwrap(), Some(first));
    }

    #[test]
    fn explicit_rebind_requires_the_previous_immutable_identity() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        let old = RemoteIdentity::new("https://hub.test", &id(1)).unwrap();
        let wrong = RemoteIdentity::new("https://hub.test", &id(2)).unwrap();
        let new = RemoteIdentity::new("https://hub.test", &id(3)).unwrap();
        pin(&repo, &old).unwrap();
        assert!(rebind(&repo, &wrong, &new).is_err());
        rebind(&repo, &old, &new).unwrap();
        assert_eq!(read(&repo).unwrap(), Some(new));
    }

    #[test]
    fn current_hub_mismatch_and_missing_pin_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let repo = Repo::init(d.path()).unwrap();
        assert!(require_current(&repo, "https://hub.test").is_err());
        pin(
            &repo,
            &RemoteIdentity::new("https://one.test", &id(1)).unwrap(),
        )
        .unwrap();
        assert!(require_current(&repo, "https://two.test").is_err());
    }

    #[test]
    fn an_rc_expected_id_can_only_narrow_the_repo_pin() {
        let pinned = RemoteIdentity::new("https://hub.test", &id(1)).unwrap();
        assert_eq!(
            constrain_expected(pinned.clone(), "https://hub.test", None).unwrap(),
            pinned
        );
        assert!(constrain_expected(pinned.clone(), "https://hub.test", Some(&id(1))).is_ok());
        assert!(constrain_expected(pinned, "https://hub.test", Some(&id(2))).is_err());
    }
}
