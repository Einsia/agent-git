//! Capture authority identifies local history independently of its publication destination.

use crate::{
    domain::{link, repo::Repo, store::Store},
    hub::identity::{self, RemoteIdentity},
    rc::{lineage::AgitSession, local_repository::Authority},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) const CAPTURE_ENV: &str = "AGIT_RC_CAPTURE_KIND";

/// Another writer changed the claim while it was read; reading it again settles the race.
#[derive(Debug, thiserror::Error)]
#[error("capture claim changed during inspection")]
pub(crate) struct ClaimChanged;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RepositoryKind {
    DeviceLocal { agent_id: String },
    HubLinked { identity: RemoteIdentity },
}

impl RepositoryKind {
    pub(crate) fn discover(lineage: &AgitSession) -> Result<Self> {
        Self::discover_by(lineage, Authority::Process)
    }

    /// The kind of `lineage`'s capture repository, a device-local one owned by `authority`.
    fn discover_by(lineage: &AgitSession, authority: Authority) -> Result<Self> {
        let repo = Repo::open(&lineage.repo_dir()?).context("capture repository is missing")?;
        if repo
            .git_opt(&["config", "--local", "--get", "agit.desktopIdentity"])
            .is_some()
        {
            super::local_repository::require_by(lineage, authority)?;
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

    fn require_by(&self, lineage: &AgitSession, authority: Authority) -> Result<Repo> {
        ensure!(
            Self::discover_by(lineage, authority)? == *self,
            "capture repository kind or identity changed"
        );
        Repo::open(&lineage.repo_dir()?).context("capture repository is missing")
    }
}

/// Whether RC may save and upload the conversation `native` of `runtime`, running or watched at
/// `cwd`, without the person asking. The agit folder governing `cwd` decides (see
/// [`crate::commands::project::folder_decision`]). With no such folder, the RC workspace project
/// the conversation runs in is the consent: `bound` when the caller knows it was started in one,
/// otherwise a project this computer's workspaces bind around `cwd`. Anywhere else, nothing is
/// saved.
pub(crate) fn consented(cwd: &std::path::Path, runtime: &str, native: &str, bound: bool) -> bool {
    match crate::commands::project::folder_decision(cwd, runtime, native) {
        Some(decided) => decided,
        None => {
            bound
                || crate::rc::mirror::Mirror::load()
                    .all_roots()
                    .iter()
                    .any(|root| cwd.starts_with(root))
        }
    }
}

pub(crate) fn require(lineage: &AgitSession) -> Result<Repo> {
    require_by(lineage, Authority::Process)
}

/// `lineage`'s capture repository, verified against `authority` as the owner of a device-local
/// one.
pub(crate) fn require_by(lineage: &AgitSession, authority: Authority) -> Result<Repo> {
    match &lineage.capture {
        Some(kind) => kind.require_by(lineage, authority),
        None => super::local_repository::require_by(lineage, authority),
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
    select(runtime, native, cwd, prior, saved, true)
}

/// Read-only history checks the claim image without waiting for an active settlement writer.
/// A changed image cannot prove an archive boundary and leaves the native history visible.
pub(crate) fn inspect(
    runtime: &str,
    native: &str,
    cwd: &Path,
    prior: Option<&AgitSession>,
    saved: Option<&RepositoryKind>,
) -> Result<Option<AgitSession>> {
    select(runtime, native, cwd, prior, saved, false)
}

fn select(
    runtime: &str,
    native: &str,
    cwd: &Path,
    prior: Option<&AgitSession>,
    saved: Option<&RepositoryKind>,
    serialize: bool,
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
    let owner = claim.owner.clone().context("capture claim has no owner")?;
    let name = claim
        .agent
        .clone()
        .context("capture claim has no repository")?;
    let branch = claim
        .branch
        .clone()
        .context("capture claim has no branch")?;
    let (owner, name, branch) = (owner.as_str(), name.as_str(), branch.as_str());
    let probe = AgitSession::new(
        &format!("{owner}/{name}"),
        "00000000-0000-0000-0000-000000000001",
        branch,
    )?;
    let _locks = if serialize {
        Some((
            link::lock_branch(&store, &probe.slug(), branch)?,
            link::lock(&store, runtime, native)?,
        ))
    } else {
        None
    };
    let current = link::get_checked(&store, runtime, native)?.ok_or(ClaimChanged)?;
    if current.to_json()? != claim.to_json()? {
        return Err(ClaimChanged.into());
    }
    // Under the link lock, a claim saved before a generation bump of its still-valid store
    // records the current generation, so later readers and landings agree on it.
    let mut claim = claim;
    if serialize {
        let mut refreshed = claim.clone();
        if refreshed.refresh_native_generation().unwrap_or(false) {
            link::write(&store, &refreshed)?;
            claim = refreshed;
        }
    }
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
    let transcript_available = claim.resolve().is_some();
    // An authenticated native writer can own a capture route before its first message exists.
    #[cfg(unix)]
    let transcript_available = transcript_available
        || (runtime == "claude-code"
            && super::native_claude::Client::unwritten_transcript(native, cwd).is_some());
    ensure!(transcript_available, "capture transcript is missing");
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
    if link::get_checked(&store, runtime, native)?
        .map(|current| current.to_json())
        .transpose()?
        .as_ref()
        != Some(&claim.to_json()?)
    {
        return Err(ClaimChanged.into());
    }
    lineage.capture = Some(kind);
    Ok(Some(lineage))
}

/// Where a capture publishes. Its `identity` names the one Hub its uploads go to, whichever Hub
/// this process defaults to: a computer signed in to several Hubs keeps each capture on the Hub
/// its destination records, and an upload that went to the default Hub instead would put a
/// conversation recorded for one Hub on another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublicationDestination {
    pub repository: String,
    pub identity: RemoteIdentity,
}

impl PublicationDestination {
    /// This destination as a session's publication status names it.
    pub(crate) fn target(&self) -> crate::protocol::PublicationTarget {
        crate::protocol::PublicationTarget {
            repository: self.repository.clone(),
            agent_id: self.identity.agent_id.clone(),
        }
    }

    /// Refuses unless this computer holds a stored sign-in for the destination's Hub. There is no
    /// other Hub to fall back to, so without one nothing is sent anywhere. Callers judge the
    /// person's own off switches first: a turned-off destination waits as turned off, whatever
    /// its sign-in.
    pub(crate) fn require_sign_in(&self) -> Result<()> {
        let hub = &self.identity.hub;
        ensure!(
            crate::infra::credentials::load_checked(hub)?.is_some(),
            "not uploaded: this computer isn't signed in to {hub}; `AGIT_HUB_URL={hub} agit login` signs in without changing the default Hub"
        );
        Ok(())
    }
}

pub(crate) fn destination(
    repo: &Repo,
    lineage: &AgitSession,
) -> Result<Option<PublicationDestination>> {
    destination_by(repo, lineage, Authority::Process)
}

/// Where `lineage`'s turns upload from this computer, read from its capture repository: `None`
/// when it has no destination, or one can't be read.
pub(crate) fn upload_target(lineage: &AgitSession) -> Option<crate::protocol::PublicationTarget> {
    let repo = Repo::open(lineage.repo_dir().ok()?)?;
    Some(destination(&repo, lineage).ok()??.target())
}

/// Where `lineage`'s capture publishes, its device-local repository verified against `authority`.
pub(crate) fn destination_by(
    repo: &Repo,
    lineage: &AgitSession,
    authority: Authority,
) -> Result<Option<PublicationDestination>> {
    let kind = lineage
        .capture
        .clone()
        .map(Ok)
        .unwrap_or_else(|| RepositoryKind::discover_by(lineage, authority))?;
    kind.require_by(lineage, authority)?;
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
    Ok(Some(destination))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::adapter::Adapter;

    /// A claim saved under an older generation of the same store keeps its capture route, and
    /// selection records the current generation in it. A replaced home is still refused. An
    /// implementation that compares whole source references reports the transcript as missing
    /// after any bump, and one that ignores home identity adopts a copied store.
    #[test]
    fn a_source_generation_bump_keeps_the_capture_and_refreshes_its_claim() {
        if crate::rc::in_isolated_test(
            "rc::capture::tests::a_source_generation_bump_keeps_the_capture_and_refreshes_its_claim",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let cwd = root.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let home = root.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let native = uuid::Uuid::now_v7().to_string();
        let transcript = home.join(format!("rollout-{native}.jsonl"));
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":native,"cwd":cwd}})
            ),
        )
        .unwrap();
        let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, archived INTEGER, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1,?2,?3,0,'question','cli',1)",
            rusqlite::params![native, transcript.to_str(), cwd.to_str()],
        )
        .unwrap();
        let registry = crate::rc::runtime_sources::Registry::open().unwrap();
        let source = registry.register(&home, None, None, None).unwrap();
        let binding = link::NativeBinding {
            source: crate::protocol::NativeSourceRef {
                source_id: source.source_id.clone(),
                generation: source.generation,
            },
            thread_id: native.clone(),
        };
        let key = binding.key().unwrap();
        let store = Store::open_or_init().unwrap();
        let mut claim = link::Link::from_native(binding, &cwd).unwrap();
        claim.owner = Some("alice".into());
        claim.agent = Some("imported".into());
        claim.branch = Some("work".into());
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
        identity::pin(
            &repo,
            &RemoteIdentity::new("https://hub.invalid", route.agent_id()).unwrap(),
        )
        .unwrap();

        registry.remove(&source.source_id).unwrap();
        let current = registry.register(&home, None, None, None).unwrap();
        assert!(current.generation > source.generation);
        let generation = |store: &Store| {
            link::get_checked(store, "codex", &key)
                .unwrap()
                .unwrap()
                .native_binding
                .unwrap()
                .source
                .generation
        };
        assert_eq!(generation(&store), source.generation);
        assert!(inspect("codex", &key, &cwd, None, None).unwrap().is_some());
        assert_eq!(
            generation(&store),
            source.generation,
            "read-only inspection never rewrites a claim"
        );
        let selected = resolve("codex", &key, &cwd, None, None).unwrap().unwrap();
        assert_eq!(selected.to_string(), "alice/imported@work");
        assert_eq!(generation(&store), current.generation);

        std::fs::rename(&home, root.path().join("moved")).unwrap();
        std::fs::create_dir(&home).unwrap();
        assert!(resolve("codex", &key, &cwd, None, None).is_err());
    }

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

#[cfg(test)]
mod consent_tests {
    /// Pins that the drop rule is advertised only by a build whose every background save checks
    /// it: watching (take-in and each collection), a turn RC drives (its archive and its landing),
    /// and recovery of a queued turn. The rule itself: the folder decides, a project an RC
    /// workspace binds is the consent where no folder speaks, and nothing else is.
    #[test]
    fn the_drop_rule_is_advertised_only_with_every_gate_checking_it() {
        if crate::rc::in_isolated_test(
            "rc::capture::consent_tests::the_drop_rule_is_advertised_only_with_every_gate_checking_it",
        ) {
            return;
        }
        let advertised = crate::rc::build_identity::DaemonIdentity::current().unwrap();
        assert!(
            advertised
                .rpc_features
                .iter()
                .any(|feature| feature == "observed-capture-consent-v1")
        );
        let source = |file: &str| {
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
                .unwrap()
        };
        for (file, check, gates) in [
            ("src/rc/daemon/observed_capture.rs", "!capture_allowed(", 2),
            (
                "src/rc/supervisor/mod.rs",
                "!crate::rc::capture::consented(",
                1,
            ),
            (
                "src/rc/supervisor/archive_recovery.rs",
                "!crate::rc::capture::consented(",
                2,
            ),
        ] {
            assert_eq!(source(file).matches(check).count(), gates, "{file}");
        }
        assert!(
            source("src/rc/daemon/observed_capture.rs")
                .contains("crate::commands::project::folder_decision(root, runtime, native)")
        );

        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        let folder = home.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let elsewhere = elsewhere.canonicalize().unwrap();
        crate::commands::project::policy_for_test(&folder, true, true, &[("codex", "dropped")]);
        assert!(super::consented(&folder, "codex", "kept", false));
        assert!(!super::consented(&folder, "codex", "dropped", true));
        crate::commands::project::policy_for_test(&folder, true, false, &[]);
        assert!(!super::consented(&folder, "codex", "kept", true));
        crate::commands::project::policy_for_test(&folder, false, true, &[]);
        assert!(!super::consented(&folder, "codex", "kept", true));
        assert!(!super::consented(&elsewhere, "codex", "kept", false));
        assert!(super::consented(&elsewhere, "codex", "kept", true));
    }
}
