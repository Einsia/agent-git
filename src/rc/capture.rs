//! Capture authority identifies local history independently of its publication destination.

use crate::{
    domain::{link, repo::Repo, store::Store},
    hub::identity::{self, RemoteIdentity},
    rc::lineage::AgitSession,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) const CAPTURE_ENV: &str = "AGIT_RC_CAPTURE_KIND";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RepositoryKind {
    DeviceLocal { agent_id: String },
    HubLinked { identity: RemoteIdentity },
}

impl RepositoryKind {
    pub(crate) fn discover(lineage: &AgitSession) -> Result<Self> {
        let repo = Repo::open(&lineage.repo_dir()?).context("capture repository is missing")?;
        if repo
            .git_opt(&["config", "--local", "--get", "agit.desktopIdentity"])
            .is_some()
        {
            super::local_repository::require(lineage)?;
            Ok(Self::DeviceLocal {
                agent_id: lineage.agent_id().into(),
            })
        } else {
            let identity =
                identity::read(&repo)?.context("capture repository has no immutable identity")?;
            ensure!(
                identity.agent_id == lineage.agent_id(),
                "capture repository identity changed"
            );
            Ok(Self::HubLinked { identity })
        }
    }

    pub(crate) fn require(&self, lineage: &AgitSession) -> Result<Repo> {
        ensure!(
            Self::discover(lineage)? == *self,
            "capture repository kind or identity changed"
        );
        Repo::open(&lineage.repo_dir()?).context("capture repository is missing")
    }
}

pub(crate) fn require(lineage: &AgitSession) -> Result<Repo> {
    match &lineage.capture {
        Some(kind) => kind.require(lineage),
        None => super::local_repository::require(lineage),
    }
}

pub(crate) fn from_environment(mut lineage: AgitSession) -> Result<AgitSession> {
    if let Some(value) = std::env::var_os(CAPTURE_ENV) {
        lineage.capture = Some(serde_json::from_str(
            value.to_str().context("invalid capture environment")?,
        )?);
    }
    require(&lineage)?;
    Ok(lineage)
}

/// Only the exact active native claim may select a capture route; workspace names cannot.
pub(crate) fn resolve(
    runtime: &str,
    native: &str,
    cwd: &Path,
    prior: Option<&AgitSession>,
    saved: Option<&RepositoryKind>,
) -> Result<Option<AgitSession>> {
    let Some(store) = Store::open()? else {
        ensure!(
            prior.is_none() && saved.is_none(),
            "recorded capture link is missing"
        );
        return Ok(None);
    };
    let Some(claim) = link::get_checked(&store, runtime, native)? else {
        ensure!(
            prior.is_none() && saved.is_none(),
            "recorded capture link is missing"
        );
        return Ok(None);
    };
    ensure!(claim.is_active(), "capture claim is superseded or archived");
    if claim.owner.is_none() && claim.agent.is_none() && claim.branch.is_none() {
        ensure!(
            prior.is_none() && saved.is_none(),
            "recorded capture route is missing"
        );
        return Ok(None);
    }
    let owner = claim
        .owner
        .as_deref()
        .context("capture claim has no owner")?;
    let name = claim
        .agent
        .as_deref()
        .context("capture claim has no repository")?;
    let branch = claim
        .branch
        .as_deref()
        .context("capture claim has no branch")?;
    let probe = AgitSession::new(
        &format!("{owner}/{name}"),
        "00000000-0000-0000-0000-000000000001",
        branch,
    )?;
    let _branch = link::lock_branch(&store, &probe.slug(), branch)?;
    let _claim = link::lock(&store, runtime, native)?;
    let current =
        link::get_checked(&store, runtime, native)?.context("capture claim disappeared")?;
    ensure!(
        current.to_json()? == claim.to_json()?,
        "capture claim changed during selection"
    );
    ensure!(
        Path::new(
            claim
                .cwd
                .as_deref()
                .context("capture claim has no workspace")?
        )
        .canonicalize()?
            == cwd.canonicalize()?,
        "capture claim belongs to another workspace"
    );
    ensure!(claim.resolve().is_some(), "capture transcript is missing");
    let repo = Repo::open(&probe.repo_dir()?).context("capture repository is missing")?;
    ensure!(
        repo.has_ref(&format!("refs/heads/{branch}")),
        "capture branch is missing"
    );
    let id = match repo.git_opt(&["config", "--local", "--get", "agit.desktopIdentity"]) {
        Some(id) => id.trim().to_owned(),
        None => {
            identity::read(&repo)?
                .context("capture repository has no immutable identity")?
                .agent_id
        }
    };
    let mut lineage = AgitSession::new(&probe.slug(), &id, branch)?;
    let kind = RepositoryKind::discover(&lineage)?;
    if let Some(prior) = prior {
        ensure!(
            prior.to_string() == lineage.to_string() && prior.agent_id() == lineage.agent_id(),
            "capture claim conflicts with the recorded route"
        );
    }
    ensure!(
        saved.is_none_or(|saved| saved == &kind),
        "capture repository identity changed since takeover"
    );
    let active = link::active_for_branch(&store, owner, name, branch);
    ensure!(
        active.len() == 1 && active[0].instance() == claim.instance(),
        "capture branch has conflicting runtime claims"
    );
    lineage.capture = Some(kind);
    Ok(Some(lineage))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublicationDestination {
    pub repository: String,
    pub identity: RemoteIdentity,
}

pub(crate) fn destination(
    repo: &Repo,
    lineage: &AgitSession,
    hub: &str,
) -> Result<Option<PublicationDestination>> {
    let kind = lineage
        .capture
        .clone()
        .map(Ok)
        .unwrap_or_else(|| RepositoryKind::discover(lineage))?;
    kind.require(lineage)?;
    let destination = match kind {
        RepositoryKind::DeviceLocal { .. } => {
            let Some(destination) = super::local_repository::publication::Destination::load(repo)?
            else {
                return Ok(None);
            };
            ensure!(
                destination.local_agent_id() == lineage.agent_id(),
                "publication source identity changed"
            );
            PublicationDestination {
                repository: destination.repository,
                identity: destination.identity,
            }
        }
        RepositoryKind::HubLinked { identity } => PublicationDestination {
            repository: lineage.slug(),
            identity,
        },
    };
    ensure!(
        destination.identity.hub == identity::normalize_hub(hub)?,
        "publication Hub changed"
    );
    Ok(Some(destination))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::adapter::Adapter;

    #[test]
    fn native_capture_uses_the_exact_link_and_retains_repository_kind() {
        if crate::rc::in_isolated_test(
            "rc::capture::tests::native_capture_uses_the_exact_link_and_retains_repository_kind",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
            std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
        }
        crate::rc::select_local_authority();
        let cwd = root.path().canonicalize().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let adapter = crate::adapter::claude_code::ClaudeCode;
        adapter.install("{}\n", &id, &cwd).unwrap();
        assert!(
            resolve("claude-code", &id, &cwd, None, None)
                .unwrap()
                .is_none()
        );
        let store = Store::open_or_init().unwrap();
        let mut claim = link::Link::new("claude-code", &id, Some(&cwd));
        claim.owner = Some("alice".into());
        claim.agent = Some("imported".into());
        claim.branch = Some("work".into());
        claim.baseline_bytes = Some(3);
        claim.baseline_hash = Some("baseline".into());
        link::write(&store, &claim).unwrap();
        let route = AgitSession::new(
            "alice/imported",
            "00000000-0000-0000-0000-000000000001",
            "work",
        )
        .unwrap();
        let repo = Repo::init(&route.repo_dir().unwrap()).unwrap();
        repo.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ])
        .unwrap();
        repo.git(&["branch", "work"]).unwrap();
        let pin = RemoteIdentity::new("https://hub.invalid", route.agent_id()).unwrap();
        identity::pin(&repo, &pin).unwrap();
        let binding = resolve("claude-code", &id, &cwd, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(binding.to_string(), "alice/imported@work");
        assert_eq!(
            binding.capture,
            Some(RepositoryKind::HubLinked {
                identity: pin.clone()
            })
        );
        assert_eq!(
            link::get_checked(&store, "claude-code", &id)
                .unwrap()
                .unwrap()
                .to_json()
                .unwrap(),
            claim.to_json().unwrap()
        );
        assert_eq!(
            resolve(
                "claude-code",
                &id,
                &cwd,
                Some(&binding),
                binding.capture.as_ref()
            )
            .unwrap(),
            Some(binding.clone())
        );
        let wrong = AgitSession::new("alice/imported", route.agent_id(), "other").unwrap();
        assert!(resolve("claude-code", &id, &cwd, Some(&wrong), None).is_err());
        let competing_id = uuid::Uuid::now_v7().to_string();
        adapter.install("{}\n", &competing_id, &cwd).unwrap();
        let mut competing = link::Link::new("claude-code", &competing_id, Some(&cwd));
        competing.owner = claim.owner.clone();
        competing.agent = claim.agent.clone();
        competing.branch = claim.branch.clone();
        link::write(&store, &competing).unwrap();
        assert!(resolve("claude-code", &id, &cwd, None, None).is_err());
        competing.superseded_by = Some(format!("claude-code/{id}"));
        link::write(&store, &competing).unwrap();
        assert!(
            resolve("claude-code", &id, &cwd, None, None)
                .unwrap()
                .is_some()
        );
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(resolve("claude-code", &id, &other, None, None).is_err());
        repo.git(&["branch", "-D", "work"]).unwrap();
        assert!(resolve("claude-code", &id, &cwd, None, None).is_err());
        repo.git(&["branch", "work"]).unwrap();
        let replacement = RemoteIdentity::new(
            "https://hub.invalid",
            "00000000-0000-0000-0000-000000000002",
        )
        .unwrap();
        identity::rebind(&repo, &pin, &replacement).unwrap();
        assert!(
            resolve(
                "claude-code",
                &id,
                &cwd,
                Some(&binding),
                binding.capture.as_ref()
            )
            .is_err()
        );
        identity::rebind(&repo, &replacement, &pin).unwrap();
        claim.superseded_by = Some("claude-code/successor".into());
        link::write(&store, &claim).unwrap();
        assert!(resolve("claude-code", &id, &cwd, None, None).is_err());
        std::fs::write(link::link_path(&store, "claude-code", &id), "malformed").unwrap();
        assert!(resolve("claude-code", &id, &cwd, None, None).is_err());
        claim.superseded_by = None;
        link::write(&store, &claim).unwrap();
        let machine = crate::rc::identity::identity().unwrap();
        repo.git(&["config", "agit.desktopIdentity", route.agent_id()])
            .unwrap();
        repo.git(&[
            "config",
            "agit.desktopAuthority",
            &format!("local:{}", machine.machine_fingerprint),
        ])
        .unwrap();
        assert!(
            resolve(
                "claude-code",
                &id,
                &cwd,
                Some(&binding),
                binding.capture.as_ref()
            )
            .is_err()
        );
        let local = resolve("claude-code", &id, &cwd, None, None)
            .unwrap()
            .unwrap();
        assert!(matches!(
            local.capture,
            Some(RepositoryKind::DeviceLocal { .. })
        ));
    }
}
