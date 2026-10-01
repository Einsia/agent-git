//! An owned machine confirms a project destination without changing native control ownership.

use super::*;
use crate::{
    commands::push::AutoConsent,
    hub::identity::RemoteIdentity,
    protocol::{ProjectPublicationBind, ProjectPublicationBindResult},
    rc::{authority::Guard, cloud::store, local_repository},
};
use anyhow::{Context, ensure};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug)]
pub(in crate::rc) enum Stage {
    OwnedBinding,
    Repository,
    RemoteBinding,
    Destination,
    Consent,
    Confirmation,
    Interrupted,
}

impl Stage {
    pub(in crate::rc) fn name(self) -> &'static str {
        match self {
            Self::OwnedBinding => "owned_binding",
            Self::Repository => "local_repository",
            Self::RemoteBinding => "remote_binding",
            Self::Destination => "destination_selection",
            Self::Consent => "publication_consent",
            Self::Confirmation => "destination_confirmation",
            Self::Interrupted => "interrupted",
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

impl std::error::Error for Stage {}

pub(super) async fn dispatch(
    daemon: Arc<Mutex<Daemon>>,
    frame: &Frame,
    epoch: u64,
    outbound: crate::rc::outbound::OutboundTx,
    tasks: &mut tokio::task::JoinSet<()>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let Some(id) = frame.id.clone() else { return };
    let prepared = {
        let state = daemon.lock().await;
        if !connection_epoch_is_current(&state.settlement, epoch) {
            return;
        }
        if tasks.len() >= 32 {
            Err(RpcError::new(
                ErrorCode::SessionBusy,
                "project publication is busy; retry shortly",
            ))
        } else {
            Prepared::new(&state, frame)
        }
    };
    match prepared {
        Ok(prepared) => {
            tasks.spawn(async move {
                let response = match prepared.execute(daemon, &mut stop).await {
                    Ok(value) => Frame::response(id, value),
                    Err(error) => Frame::error_response(id, error),
                };
                let _ = outbound.send(response);
            });
        }
        Err(error) => {
            let _ = outbound.send(Frame::error_response(id, error));
        }
    }
}

#[derive(Clone)]
struct Prepared {
    authority: Guard,
    request: ProjectPublicationBind,
    directory: PathBuf,
    credential_dir: PathBuf,
    destination: RemoteIdentity,
}

pub(in crate::rc) async fn reconcile_owned(
    hub: String,
    credential_dir: PathBuf,
    device: agit_peer::cloud::Device,
    target: crate::hub::project_publication::Target,
) -> crate::Result<()> {
    let canceled = Cancel(Arc::new(AtomicBool::new(false)));
    let cancellation = canceled.0.clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            let request = target.request();
            let directory = PathBuf::from(&request.local_path);
            let destination =
                RemoteIdentity::new(&hub, &request.repository_id).context(Stage::RemoteBinding)?;
            let client = crate::hub::Client::for_stored_hub_with_timeout(
                &hub,
                std::time::Duration::from_secs(15),
            );
            let local_check = || {
                check_owned_binding(&credential_dir, &device, &request, &cancellation)
                    .context(Stage::OwnedBinding)
            };
            local_check()?;
            let repository = local_repository::ensure_repository(&request.project_id, &directory)
                .context(Stage::Repository)?;
            let lineage = crate::rc::lineage::AgitSession::new(
                &repository.slug,
                &repository.agent_id,
                "main",
            )?;
            let repo = local_repository::require(&lineage).context(Stage::Repository)?;
            if local_repository::publication::Destination::load(&repo)?.is_some_and(|saved| {
                saved.identity == destination && saved.repository == request.repository
            }) && repo.creation_encryption()? == Some(request.encryption_enabled)
                && repo.auto_push_enabled()?
                && crate::commands::push::automatic_publication_consent_with_client(
                    &repo,
                    &request.repository,
                    &destination,
                    &client,
                )
                .context(Stage::Consent)?
            {
                return Ok(());
            }
            let current = || -> crate::Result<()> {
                local_check()?;
                let current = client
                    .current_project_publication(&target)
                    .context(Stage::RemoteBinding)?;
                ensure!(
                    super::publication::same_device(&current.device, &device)
                        && current.entry == target,
                    "owned project publication binding changed"
                );
                local_check()
            };
            current()?;
            let selection = local_repository::publication::Selection::prepare(
                &repo,
                Some(&request.repository),
                &hub,
            )
            .context(Stage::Destination)?
            .context("project repository is not device-local")?;
            let consent = prepare_project_publication_consent(
                &repo,
                &request.repository,
                &destination,
                &device.owner.account_id,
                request.encryption_enabled,
            )
            .context(Stage::Consent)?;
            current()?;
            confirm_repository(
                &repo,
                &selection,
                &destination,
                &consent,
                request.encryption_enabled,
            )
            .context(Stage::Confirmation)?;
            current()
        }),
    )
    .await
    .context(Stage::Interrupted)??
}

fn check_owned_binding(
    credential_dir: &std::path::Path,
    device: &agit_peer::cloud::Device,
    request: &ProjectPublicationBind,
    canceled: &AtomicBool,
) -> crate::Result<()> {
    ensure!(
        !canceled.load(Ordering::Acquire),
        "project publication was canceled"
    );
    ensure!(
        request.workspace_id == crate::rc::endpoint::WORKSPACE,
        "publication plan addresses another local authority"
    );
    let enrollment = store::load_in(credential_dir, &device.owner.issuer)?
        .context("device enrollment is missing")?;
    ensure!(
        enrollment.inbound_enabled
            && super::publication::same_device(&enrollment.credential.device, device),
        "project publication enrollment changed"
    );
    ensure!(
        store::policy_in(credential_dir)?.access(&device.owner, None, None)
            == agit_peer::access::Access::Admin,
        "project publication requires current owned-machine access"
    );
    let directory =
        crate::rc::policy::require_bindable_dir(std::path::Path::new(&request.local_path))?;
    ensure!(
        directory.to_str() == Some(request.local_path.as_str())
            && Mirror::load_in(credential_dir)?
                .project_path(&request.workspace_id, &request.project_id)
                .as_ref()
                == Some(&directory),
        "project publication folder binding changed"
    );
    Ok(())
}

impl Prepared {
    fn new(daemon: &Daemon, frame: &Frame) -> Result<Self, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        let request: ProjectPublicationBind = frame.params_as()?;
        let grant = frame.authority.owned_machine_grant()?;
        if !daemon.opts.local_owner || caller.workspace_id != crate::rc::endpoint::WORKSPACE {
            return Err(forbidden());
        }
        let directory = daemon
            .mirror
            .project_path(&caller.workspace_id, &request.project_id)
            .ok_or_else(forbidden)?;
        if directory.to_str() != Some(request.local_path.as_str()) {
            return Err(forbidden());
        }
        let destination = RemoteIdentity::new(&grant.target.owner.issuer, &request.repository_id)
            .map_err(|_| forbidden())?;
        crate::rc::lineage::AgitSession::new(&request.repository, &destination.agent_id, "main")
            .map_err(|_| forbidden())?;
        Ok(Self {
            authority: frame.authority.clone(),
            request,
            directory,
            credential_dir: store::directory().map_err(|_| unavailable())?,
            destination,
        })
    }

    fn check(&self, daemon: &Daemon) -> Result<(), RpcError> {
        self.authority.owned_machine_grant()?;
        if !daemon.opts.local_owner
            || daemon
                .mirror
                .project_path(crate::rc::endpoint::WORKSPACE, &self.request.project_id)
                .as_ref()
                != Some(&self.directory)
        {
            return Err(forbidden());
        }
        Ok(())
    }

    async fn execute(
        self,
        daemon: Arc<Mutex<Daemon>>,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<serde_json::Value, RpcError> {
        let canceled = Cancel(Arc::new(AtomicBool::new(false)));
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => Err(unavailable()),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), self.bind(daemon, canceled.0.clone())) => {
                result.unwrap_or_else(|_| Err(unavailable()))
            }
        }
    }

    async fn bind(
        &self,
        daemon: Arc<Mutex<Daemon>>,
        canceled: Arc<AtomicBool>,
    ) -> Result<serde_json::Value, RpcError> {
        self.check(&*daemon.lock().await)?;
        let preparation = self.clone();
        let cancellation = canceled.clone();
        let verified =
            tokio::task::spawn_blocking(move || preparation.verify_remote(&cancellation))
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
        self.check(&*daemon.lock().await)?;
        let preparation = self.clone();
        let result = tokio::task::spawn_blocking(move || preparation.confirm(verified, &canceled))
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        // Destination settings grant no runtime access; current project authority fences acceptance.
        self.check(&*daemon.lock().await)?;
        serde_json::to_value(result).map_err(|_| unavailable())
    }

    fn current_device(
        &self,
        canceled: &AtomicBool,
    ) -> crate::Result<agit_peer::cloud::ConnectionGrant> {
        ensure!(
            !canceled.load(Ordering::Acquire),
            "project publication was canceled"
        );
        let grant = self.authority.owned_machine_grant()?;
        let enrollment = store::load_in(&self.credential_dir, &self.destination.hub)?
            .context("device enrollment is missing")?;
        ensure!(
            enrollment.inbound_enabled
                && super::publication::same_device(&enrollment.credential.device, &grant.target),
            "project publication enrollment changed"
        );
        ensure!(
            crate::rc::policy::require_bindable_dir(&self.directory)? == self.directory,
            "project directory changed"
        );
        Ok(grant)
    }

    fn verify_remote(&self, canceled: &AtomicBool) -> crate::Result<Verified> {
        let grant = self.current_device(canceled)?;
        let repository =
            local_repository::ensure_repository(&self.request.project_id, &self.directory)?;
        let lineage =
            crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")?;
        let repo = local_repository::require(&lineage)?;
        let selection = local_repository::publication::Selection::prepare(
            &repo,
            Some(&self.request.repository),
            &self.destination.hub,
        )?
        .context("project repository is not device-local")?;
        let consent = prepare_project_publication_consent(
            &repo,
            &self.request.repository,
            &self.destination,
            &grant.target.owner.account_id,
            self.request.encryption_enabled,
        )?;
        self.current_device(canceled)?;
        selection.verify(&repo)?;
        Ok(Verified {
            repository,
            lineage,
            selection,
            consent,
        })
    }

    fn confirm(
        &self,
        verified: Verified,
        canceled: &AtomicBool,
    ) -> crate::Result<ProjectPublicationBindResult> {
        self.current_device(canceled)?;
        let repo = local_repository::require(&verified.lineage)?;
        verified.selection.verify(&repo)?;
        confirm_repository(
            &repo,
            &verified.selection,
            &self.destination,
            &verified.consent,
            self.request.encryption_enabled,
        )?;
        self.current_device(canceled)?;
        Ok(ProjectPublicationBindResult {
            project_id: self.request.project_id.clone(),
            local_path: self.request.local_path.clone(),
            repository: self.request.repository.clone(),
            repository_id: self.destination.agent_id.clone(),
            local_repository_id: verified.repository.agent_id,
            auto_push_enabled: repo.auto_push_enabled()?,
            consent_ready: verified.consent.matches(&repo)?,
        })
    }
}

fn confirm_repository(
    repo: &crate::domain::repo::Repo,
    selection: &local_repository::publication::Selection,
    destination: &RemoteIdentity,
    consent: &AutoConsent,
    encryption_enabled: bool,
) -> crate::Result<()> {
    selection.verify(repo)?;
    if let Some(mode) = repo.creation_encryption()? {
        ensure!(
            mode == encryption_enabled,
            "local publication encryption mode changed"
        );
    }
    selection.bind(repo, destination)?;
    consent.save(repo)?;
    repo.set_creation_encryption(encryption_enabled)?;
    repo.set_auto_push(Some(true))?;
    Ok(())
}

struct Verified {
    repository: local_repository::Repository,
    lineage: crate::rc::lineage::AgitSession,
    selection: local_repository::publication::Selection,
    consent: AutoConsent,
}

struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn prepare_project_publication_consent(
    repo: &crate::domain::repo::Repo,
    repository: &str,
    expected: &RemoteIdentity,
    owner_account: &str,
    encryption_enabled: bool,
) -> crate::Result<AutoConsent> {
    let client = crate::hub::Client::for_stored_hub_with_timeout(
        &expected.hub,
        std::time::Duration::from_secs(15),
    );
    ensure!(
        client.me()?.account_id.as_deref() == Some(owner_account),
        "publication credentials belong to another account"
    );
    let (owner, name) = crate::commands::parse_slug(repository)?;
    let remote = client.get_agent(&owner, &name)?;
    ensure!(
        remote.agent_id == expected.agent_id
            && remote.owner == owner
            && remote.name == name
            && remote.visibility == "private"
            && remote.require_encryption_enabled()? == encryption_enabled,
        "the private publication destination or encryption mode changed"
    );
    crate::commands::push::publication_consent_with_client(repo, repository, expected, &client)
}

fn forbidden() -> RpcError {
    RpcError::new(
        ErrorCode::Forbidden,
        "project publication authority or folder binding changed",
    )
}

fn unavailable() -> RpcError {
    RpcError::new(
        ErrorCode::Internal,
        "project publication is not ready; retain the pending archive and retry",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::privacy_receipt::PublicationMode;

    /// A background plan cannot outlive local folder, enrollment, or owner authority.
    #[test]
    fn unattended_reconciliation_checks_live_device_and_local_folder_authority() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::unattended_reconciliation_checks_live_device_and_local_folder_authority",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let identity = agit_peer::Identity::generate().unwrap();
        let enrollment = store::Enrollment {
            credential: agit_peer::cloud::DeviceCredential {
                device: agit_peer::cloud::Device {
                    id: "device".into(),
                    owner: agit_peer::access::Principal {
                        issuer: "https://cloud.example".into(),
                        account_id: "account".into(),
                    },
                    machine_id: "machine".into(),
                    display_name: "Fixture".into(),
                    certificate: identity.certificate().clone(),
                    credential_epoch: 1,
                },
                token: agit_peer::cloud::Secret::new("fixture".into()),
            },
            identity,
            inbound_enabled: true,
        };
        store::save(&enrollment).unwrap();
        store::grant_enrolling_owner(&enrollment).unwrap();
        let project = home.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut mirror = Mirror::default();
        let project = mirror
            .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
            .unwrap();
        mirror.save().unwrap();
        let request = ProjectPublicationBind {
            workspace_id: crate::rc::endpoint::WORKSPACE.into(),
            project_id: "project".into(),
            local_path: project.to_str().unwrap().into(),
            repository: "owner/project".into(),
            repository_id: "00000000-0000-0000-0000-000000000001".into(),
            encryption_enabled: false,
        };
        let canceled = AtomicBool::new(false);
        let directory = store::directory().unwrap();
        let device = &enrollment.credential.device;
        check_owned_binding(&directory, device, &request, &canceled).unwrap();
        canceled.store(true, Ordering::Release);
        assert!(check_owned_binding(&directory, device, &request, &canceled).is_err());
        canceled.store(false, Ordering::Release);
        let mut stale = device.clone();
        stale.credential_epoch += 1;
        assert!(check_owned_binding(&directory, &stale, &request, &canceled).is_err());
        mirror.unbind(crate::rc::endpoint::WORKSPACE, "project");
        mirror.save().unwrap();
        assert!(check_owned_binding(&directory, device, &request, &canceled).is_err());
        mirror
            .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
            .unwrap();
        mirror.save().unwrap();
        store::save_policy(&agit_peer::access::Policy::default()).unwrap();
        assert!(check_owned_binding(&directory, device, &request, &canceled).is_err());
    }

    /// Reconciliation repairs automatic delivery without changing the confirmed destination.
    #[test]
    fn project_reconciliation_repairs_delivery_and_preserves_destination_identity() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::project_reconciliation_repairs_delivery_and_preserves_destination_identity",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let directory = home.path().join("project");
        std::fs::create_dir(&directory).unwrap();
        let repository = local_repository::ensure_repository("project", &directory).unwrap();
        let lineage =
            crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
                .unwrap();
        let repo = local_repository::require(&lineage).unwrap();
        let destination = RemoteIdentity::new(
            "https://hub.example",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap();
        let consent = AutoConsent {
            version: 2,
            mode: PublicationMode::Ordinary,
            hub: destination.hub.clone(),
            account: "owner".into(),
            account_id: Some("owner-account".into()),
            agent_id: destination.agent_id.clone(),
            url: "https://hub.example/owner/project.git".into(),
            visibility: "private".into(),
            policy_digest: None,
            recipient: None,
        };
        let prepare = || {
            local_repository::publication::Selection::prepare(
                &repo,
                Some("owner/project"),
                &destination.hub,
            )
            .unwrap()
            .unwrap()
        };
        confirm_repository(&repo, &prepare(), &destination, &consent, false).unwrap();
        assert!(repo.auto_push_enabled().unwrap());
        assert!(consent.matches(&repo).unwrap());
        assert_eq!(repo.creation_encryption().unwrap(), Some(false));
        repo.set_auto_push(Some(false)).unwrap();
        std::fs::remove_file(
            repo.common_dir()
                .unwrap()
                .join("agit/privacy-auto-consent.json"),
        )
        .unwrap();
        confirm_repository(&repo, &prepare(), &destination, &consent, false).unwrap();
        assert!(repo.auto_push_enabled().unwrap());
        assert!(consent.matches(&repo).unwrap());
        let mut replacement = destination.clone();
        replacement.agent_id = "00000000-0000-0000-0000-000000000002".into();
        assert!(confirm_repository(&repo, &prepare(), &replacement, &consent, false).is_err());
        assert!(confirm_repository(&repo, &prepare(), &destination, &consent, true).is_err());
        assert_eq!(
            local_repository::publication::Destination::load(&repo)
                .unwrap()
                .unwrap()
                .identity,
            destination
        );
        assert_eq!(repo.creation_encryption().unwrap(), Some(false));
    }
}
