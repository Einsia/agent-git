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

/// Why a project bind from the website failed, as the device says it: a fixed class, never an
/// error's text, a path or a repository name (#307).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::rc) enum Failure {
    /// This computer holds no sign-in or enrollment for the project's Hub.
    LoginOtherHub,
    /// The sign-in here belongs to another account than the project's owner.
    LoginOtherAccount,
    /// The destination repository is not private.
    NotPrivate,
    /// The destination repository is not the one the project names, or not the owner's.
    NotOwned,
    /// The Hub refuses this account write access to the destination.
    NotWritable,
    TimedOut,
    Interrupted,
    Other,
}

impl Failure {
    pub(in crate::rc) fn name(self) -> &'static str {
        match self {
            Self::LoginOtherHub => "login_other_hub",
            Self::LoginOtherAccount => "login_other_account",
            Self::NotPrivate => "not_private",
            Self::NotOwned => "not_owned",
            Self::NotWritable => "not_writable",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
            Self::Other => "other",
        }
    }

    /// Whether retrying unchanged can succeed. The others need the person to change something.
    fn retryable(self) -> bool {
        matches!(self, Self::TimedOut | Self::Interrupted | Self::Other)
    }

    /// The class of `error`: one a check raised itself, else what the Hub's status says.
    fn of(error: &anyhow::Error) -> Self {
        if let Some(failure) = error.downcast_ref::<Failure>() {
            return *failure;
        }
        for cause in error.chain() {
            if let Some(failure) = cause.downcast_ref::<Failure>() {
                return *failure;
            }
            if let Some(crate::hub::WriteRefused(access)) =
                cause.downcast_ref::<crate::hub::WriteRefused>()
            {
                return match access {
                    crate::hub::PushAccess::ReadOnly => Self::NotWritable,
                    crate::hub::PushAccess::Missing => Self::NotOwned,
                    crate::hub::PushAccess::Writable => Self::Other,
                };
            }
            if let Some(api) = cause.downcast_ref::<crate::hub::client::ApiError>() {
                match api.status {
                    401 => return Self::LoginOtherHub,
                    403 => return Self::NotWritable,
                    404 => return Self::NotOwned,
                    _ => {}
                }
            }
        }
        Self::Other
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

impl std::error::Error for Failure {}

/// The stage a failed step was in, from the context it carries. A context is found by
/// downcasting the error itself: walking its chain yields the context's message, not the value.
fn stage_of(error: &anyhow::Error) -> Option<Stage> {
    error.downcast_ref::<Stage>().copied()
}

/// Automatic publication is explicitly off for the repository on this computer. Only a request
/// that names that setting lifts it, so an enrolment from the Hub leaves the repository, and the
/// folder it would publish, as the person set them: lifting it would upload what they stopped.
#[derive(Debug, thiserror::Error)]
#[error("automatic upload is turned off for this repository on this computer")]
pub(in crate::rc) struct AutoPushOff;

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
            }) && repo.auto_push_enabled()?
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
            )
            .context(Stage::Consent)?;
            current()?;
            match confirm_repository(
                &repo,
                &selection,
                &destination,
                &consent,
                &request.repository,
            ) {
                // Reconciling never lifts it, and retrying cannot change it.
                Err(error) if error.is::<AutoPushOff>() => return Ok(()),
                result => result.context(Stage::Confirmation)?,
            }
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
    ensure!(
        request.ordinary_destination(),
        "an archived encrypted repository cannot receive project publications"
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
        if !request.ordinary_destination() {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "an archived encrypted repository cannot receive project publications",
            ));
        }
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
            _ = stop.wait_for(|stopped| *stopped) => Err(bind_failed(Stage::Interrupted, Failure::Interrupted)),
            result = tokio::time::timeout(std::time::Duration::from_secs(30), self.bind(daemon, canceled.0.clone())) => {
                result.unwrap_or_else(|_| Err(bind_failed(Stage::Interrupted, Failure::TimedOut)))
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
                .map_err(|_| bind_failed(Stage::Interrupted, Failure::Interrupted))?
                .map_err(|error| step_failed(Stage::RemoteBinding, &error))?;
        self.check(&*daemon.lock().await)?;
        let preparation = self.clone();
        let result = tokio::task::spawn_blocking(move || preparation.confirm(verified, &canceled))
            .await
            .map_err(|_| bind_failed(Stage::Interrupted, Failure::Interrupted))?
            .map_err(|error| {
                if error.is::<AutoPushOff>() || error.chain().any(|cause| cause.is::<AutoPushOff>())
                {
                    auto_push_off()
                } else {
                    step_failed(Stage::Confirmation, &error)
                }
            })?;
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
            .ok_or(Failure::LoginOtherHub)?;
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
        let grant = self.current_device(canceled).context(Stage::OwnedBinding)?;
        let repository =
            local_repository::ensure_repository(&self.request.project_id, &self.directory)
                .context(Stage::Repository)?;
        let lineage =
            crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
                .context(Stage::Repository)?;
        let repo = local_repository::require(&lineage).context(Stage::Repository)?;
        let selection = local_repository::publication::Selection::prepare(
            &repo,
            Some(&self.request.repository),
            &self.destination.hub,
        )
        .context(Stage::Destination)?
        .context("project repository is not device-local")
        .context(Stage::Destination)?;
        let consent = prepare_project_publication_consent(
            &repo,
            &self.request.repository,
            &self.destination,
            &grant.target.owner.account_id,
        )
        .context(Stage::Consent)?;
        self.current_device(canceled).context(Stage::OwnedBinding)?;
        selection.verify(&repo).context(Stage::Destination)?;
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
            &self.request.repository,
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
    repository: &str,
) -> crate::Result<()> {
    selection.verify(repo)?;
    ensure!(
        consent.hub == destination.hub && consent.agent_id == destination.agent_id,
        "publication enrollment names another destination"
    );
    if crate::commands::auto_push::publication_explicitly_off(repository)?
        || repo.uploads_turned_off()?
    {
        return Err(AutoPushOff.into());
    }
    selection.bind(repo, destination)?;
    consent.save(repo)?;
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
) -> crate::Result<AutoConsent> {
    let client = crate::hub::Client::for_stored_hub_with_timeout(
        &expected.hub,
        std::time::Duration::from_secs(15),
    );
    if client.me()?.account_id.as_deref() != Some(owner_account) {
        return Err(Failure::LoginOtherAccount.into());
    }
    let (owner, name) = crate::commands::parse_slug(repository)?;
    let remote = client.get_agent(&owner, &name)?;
    if remote.agent_id != expected.agent_id || remote.owner != owner || remote.name != name {
        return Err(Failure::NotOwned.into());
    }
    if remote.visibility != "private" {
        return Err(Failure::NotPrivate.into());
    }
    crate::commands::push::publication_consent_with_client(repo, repository, expected, &client)
}

fn forbidden() -> RpcError {
    RpcError::new(
        ErrorCode::Forbidden,
        "project publication authority or folder binding changed",
    )
}

fn auto_push_off() -> RpcError {
    super::classified(
        RpcError::new(
            ErrorCode::Forbidden,
            "automatic upload is turned off for this repository on the device",
        )
        .with_hint("turn it on there with `agit config --repo OWNER/NAME push.auto true`"),
        false,
        "auto_push_off_on_this_computer",
    )
}

/// A failed bind as the website sees it: the stage and the class, nothing else.
fn bind_failed(stage: Stage, failure: Failure) -> RpcError {
    let mut error = super::classified(
        RpcError::new(
            ErrorCode::Internal,
            format!(
                "project publication is not ready ({}: {}); retain the pending archive and retry",
                stage.name(),
                failure.name()
            ),
        ),
        failure.retryable(),
        failure.name(),
    );
    if let Some(fields) = error
        .data
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    {
        fields.insert("stage".into(), serde_json::json!(stage.name()));
    }
    error
}

/// [`bind_failed`] for a step's error, in the stage it carries (else `fallback`).
fn step_failed(fallback: Stage, error: &anyhow::Error) -> RpcError {
    bind_failed(stage_of(error).unwrap_or(fallback), Failure::of(error))
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

    /// A failed bind from the website names its stage and a fixed class, one per cause, and
    /// nothing else: not the error's text, a path or a repository name (#307).
    #[test]
    fn a_failed_bind_names_only_its_stage_and_class() {
        const CANARY: &str = "/Users/canary/private-folder canary-owner/canary-repo";
        let failed = |error: anyhow::Error| step_failed(Stage::RemoteBinding, &error);
        let cases = [
            (
                anyhow::Error::from(Failure::LoginOtherHub)
                    .context(CANARY)
                    .context(Stage::OwnedBinding),
                "owned_binding",
                "login_other_hub",
                false,
            ),
            (
                anyhow::Error::from(Failure::LoginOtherAccount).context(Stage::Consent),
                "publication_consent",
                "login_other_account",
                false,
            ),
            (
                anyhow::Error::from(Failure::NotPrivate).context(Stage::Consent),
                "publication_consent",
                "not_private",
                false,
            ),
            (
                anyhow::Error::from(Failure::NotOwned).context(Stage::Consent),
                "publication_consent",
                "not_owned",
                false,
            ),
            (
                anyhow::Error::from(crate::hub::client::ApiError::for_test(401, CANARY))
                    .context(Stage::Consent),
                "publication_consent",
                "login_other_hub",
                false,
            ),
            (
                anyhow::Error::from(crate::hub::client::ApiError::for_test(403, CANARY))
                    .context(Stage::Consent),
                "publication_consent",
                "not_writable",
                false,
            ),
            (
                anyhow::Error::from(crate::hub::client::ApiError::for_test(404, CANARY))
                    .context(Stage::Destination),
                "destination_selection",
                "not_owned",
                false,
            ),
            (
                anyhow::Error::from(crate::hub::WriteRefused(crate::hub::PushAccess::ReadOnly))
                    .context(CANARY)
                    .context(Stage::Consent),
                "publication_consent",
                "not_writable",
                false,
            ),
            (
                anyhow::Error::from(crate::hub::WriteRefused(crate::hub::PushAccess::Missing))
                    .context(Stage::Consent),
                "publication_consent",
                "not_owned",
                false,
            ),
            (
                anyhow::anyhow!("{CANARY}").context(Stage::Repository),
                "local_repository",
                "other",
                true,
            ),
            (anyhow::anyhow!("{CANARY}"), "remote_binding", "other", true),
        ];
        for (error, stage, class, retryable) in cases {
            let rpc = failed(error);
            let data = rpc.data.clone().unwrap();
            assert_eq!(data["stage"], stage, "{class}");
            assert_eq!(data["reason"], class);
            assert_eq!(data["retryable"], retryable, "{class}");
            assert_eq!(data.as_object().unwrap().len(), 3, "{data}");
            let text = serde_json::to_string(&rpc).unwrap();
            assert!(!text.contains("canary"), "{text}");
            assert!(text.contains(&format!("({stage}: {class})")), "{text}");
        }
        for (failure, class) in [
            (Failure::TimedOut, "timed_out"),
            (Failure::Interrupted, "interrupted"),
        ] {
            let data = bind_failed(Stage::Interrupted, failure).data.unwrap();
            assert_eq!(
                (data["stage"].as_str(), data["reason"].as_str()),
                (Some("interrupted"), Some(class))
            );
            assert_eq!(data["retryable"], true);
        }
    }

    /// A background plan cannot outlive local folder, enrollment, or owner authority, and cannot
    /// bind an archived encrypted repository while accepting the ordinary mode a Hub reports.
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
            encryption_enabled: None,
        };
        let canceled = AtomicBool::new(false);
        let directory = store::directory().unwrap();
        let device = &enrollment.credential.device;
        check_owned_binding(&directory, device, &request, &canceled).unwrap();
        // Hubs serving released clients send the archived repository mode in the same request.
        let mut wire = serde_json::to_value(&request).unwrap();
        wire["encryption_enabled"] = false.into();
        let ordinary: ProjectPublicationBind = serde_json::from_value(wire.clone()).unwrap();
        check_owned_binding(&directory, device, &ordinary, &canceled).unwrap();
        wire["encryption_enabled"] = true.into();
        let archived: ProjectPublicationBind = serde_json::from_value(wire).unwrap();
        assert!(check_owned_binding(&directory, device, &archived, &canceled).is_err());
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

    /// Reconciliation repairs automatic delivery without changing the confirmed destination, and
    /// never lifts automatic publication the person turned off for the destination repository on
    /// this computer, in its checkout or for every repository: it changes nothing and reports that
    /// instead. Repairing whatever the person chose would upload what they stopped; judging by the
    /// device-local repository's own off, which is agit's until enrolment, would refuse every
    /// repair.
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
            version: 3,
            hub: destination.hub.clone(),
            account: "owner".into(),
            account_id: "owner-account".into(),
            agent_id: destination.agent_id.clone(),
            url: "https://hub.example/owner/project.git".into(),
            visibility: "private".into(),
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
        let confirm = |destination: &RemoteIdentity| {
            confirm_repository(&repo, &prepare(), destination, &consent, "owner/project")
        };
        confirm(&destination).unwrap();
        assert!(repo.auto_push_enabled().unwrap());
        assert!(consent.matches(&repo).unwrap());
        let consent_file = repo
            .common_dir()
            .unwrap()
            .join("agit/privacy-auto-consent.json");
        repo.set_auto_push(Some(false)).unwrap();
        std::fs::remove_file(&consent_file).unwrap();
        confirm(&destination).unwrap();
        assert!(repo.auto_push_enabled().unwrap());
        assert!(consent.matches(&repo).unwrap());

        let checkout_dir = crate::infra::config::repo_dir("owner", "project").unwrap();
        std::fs::create_dir_all(&checkout_dir).unwrap();
        let checkout = crate::domain::repo::Repo::init(&checkout_dir).unwrap();
        checkout.set_auto_push(Some(false)).unwrap();
        repo.set_auto_push(Some(false)).unwrap();
        std::fs::remove_file(&consent_file).unwrap();
        assert!(confirm(&destination).unwrap_err().is::<AutoPushOff>());
        assert_eq!(repo.auto_push_override().unwrap(), Some(false));
        assert!(!consent_file.exists(), "a refused repair changes nothing");
        checkout.set_auto_push(None).unwrap();
        crate::infra::config::set_global("push.auto", Some("false")).unwrap();
        assert!(
            confirm(&destination).unwrap_err().is::<AutoPushOff>(),
            "a user-wide off with no repository preference is as explicit"
        );
        crate::infra::config::set_global("push.auto", None).unwrap();
        repo.turn_uploads_off(true).unwrap();
        assert!(
            confirm(&destination).unwrap_err().is::<AutoPushOff>(),
            "a folder turned off with `agit off` stays off"
        );
        assert_eq!(repo.auto_push_override().unwrap(), Some(false));
        repo.turn_uploads_off(false).unwrap();
        confirm(&destination).unwrap();
        assert!(repo.auto_push_enabled().unwrap());
        assert!(consent.matches(&repo).unwrap());
        let mut replacement = destination.clone();
        replacement.agent_id = "00000000-0000-0000-0000-000000000002".into();
        assert!(confirm(&replacement).is_err());
        assert_eq!(
            local_repository::publication::Destination::load(&repo)
                .unwrap()
                .unwrap()
                .identity,
            destination
        );
    }
}
