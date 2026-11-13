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
            let key = crate::rc::folder_destination::Key::signed_in(&directory, &hub)?
                .ok_or(Failure::LoginOtherHub)
                .context(Stage::Consent)?;
            let decision = decide(&repo, &key, &target, &destination)?;
            if decision == Decision::Hold {
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
            if decision == Decision::Own {
                current()?;
                prepare_project_publication_consent(
                    &repo,
                    &request.repository,
                    &destination,
                    &device.owner.account_id,
                )
                .context(Stage::Consent)?;
                choose_own_binding(
                    &key,
                    &request.project_id,
                    &request.repository,
                    &destination,
                    move_terminal,
                )
                .context(Stage::Destination)?;
            }
            // A folder whose destination changed starts new conversations in a repository of
            // their own; it is that one this binding configures.
            let repo = project_repository(&request.project_id, &directory)?.1;
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
            current()?;
            let consent = prepare_project_publication_consent(
                &repo,
                &request.repository,
                &destination,
                &device.owner.account_id,
            )
            .context(Stage::Consent)?;
            let selection = local_repository::publication::Selection::prepare(
                &repo,
                Some(&request.repository),
                &hub,
            )
            .context(Stage::Destination)?
            .context("project repository is not device-local")?;
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
        let consent = prepare_project_publication_consent(
            &repo,
            &self.request.repository,
            &self.destination,
            &grant.target.owner.account_id,
        )
        .context(Stage::Consent)?;
        // The device owner confirms this binding from the web themselves, so it is this
        // account's own choice for the folder.
        let key =
            crate::rc::folder_destination::Key::signed_in(&self.directory, &self.destination.hub)?
                .ok_or(Failure::LoginOtherHub)
                .context(Stage::Consent)?;
        choose_own_binding(
            &key,
            &self.request.project_id,
            &self.request.repository,
            &self.destination,
            move_terminal,
        )
        .context(Stage::Destination)?;
        let (repository, repo) = project_repository(&self.request.project_id, &self.directory)?;
        let lineage =
            crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
                .context(Stage::Repository)?;
        let selection = local_repository::publication::Selection::prepare(
            &repo,
            Some(&self.request.repository),
            &self.destination.hub,
        )
        .context(Stage::Destination)?
        .context("project repository is not device-local")
        .context(Stage::Destination)?;
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

/// What a reconcile does with a Hub binding for the folder's project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// The device's own account made it: it becomes the folder's destination.
    Own,
    /// It is already the folder's destination: it is confirmed as it is.
    Current,
    /// Someone else made it: the project's conversations stay here, and the folder as it is.
    Hold,
}

/// Judges `target`, the Hub's binding of a project over the folder of device-local `repo`, for the
/// account `key` names. The binding is remembered, so the project's conversations upload only to
/// it. A binding the device's own account made, or that is already the folder's destination, is
/// confirmed; any other is held. A destination this computer already uploads the folder's only
/// project to stays its destination, and nothing else is taken from an earlier choice. A held
/// binding older than the Hub's record of who made it is asked about once, at a terminal.
fn decide(
    repo: &crate::domain::repo::Repo,
    key: &crate::rc::folder_destination::Key,
    target: &crate::hub::project_publication::Target,
    destination: &RemoteIdentity,
) -> crate::Result<Decision> {
    use crate::rc::folder_destination as folder;
    // A folder whose record predates this one takes the person's own terminal choice first.
    local_repository::publication::follow_folder(repo, &destination.hub)?;
    let request = target.request();
    let binding = folder::Binding {
        repository: request.repository.clone(),
        repository_id: request.repository_id.clone(),
    };
    folder::note_binding(key, &request.project_id, &binding)?;
    let chosen = folder::chosen(key)?;
    // The device's own account's binding is taken once. One older than the Hub's record of who
    // made it is taken only by a folder with no destination yet, in a workspace where no other
    // account can bind.
    if target.bound_by_account(key.account())
        && (target.bound_by.is_some() || chosen.is_none())
        && !folder::applied(key, &request.project_id, &request.repository_id)?
    {
        return Ok(Decision::Own);
    }
    if chosen
        .as_ref()
        .is_some_and(|chosen| chosen.repository_id == request.repository_id)
    {
        return Ok(Decision::Current);
    }
    if chosen.is_none()
        && local_repository::publication::Destination::load(repo)?
            .is_some_and(|saved| &saved.identity == destination)
        && local_repository::projects_of(repo)?.len() == 1
    {
        folder::adopt(key, &request.repository, &request.repository_id)?;
        return Ok(Decision::Current);
    }
    if target.bound_by.is_none() {
        folder::ask_later(key, &request.project_id, &binding)?;
    }
    Ok(Decision::Hold)
}

/// Makes `repository`, which the device's own account bound `project` to, the folder's
/// destination: new conversations of the folder go there, and so does the folder's own terminal
/// policy of the same account, moved by `move_terminal` the way `agit on --repo` moves it. A
/// folder with no policy of its own gets none, so terminal capture is never turned on from the
/// web.
fn choose_own_binding(
    key: &crate::rc::folder_destination::Key,
    project: &str,
    repository: &str,
    destination: &RemoteIdentity,
    move_terminal: impl FnOnce(&std::path::Path, &str) -> crate::Result<()>,
) -> crate::Result<()> {
    crate::rc::folder_destination::choose(
        key,
        repository,
        &destination.agent_id,
        crate::rc::folder_destination::ChosenFor::Project(project.to_owned()),
    )?;
    // A terminal policy turned off stays off and where it was: moving it runs `agit on`, which
    // would turn uploads back on that the person turned off themselves.
    if let Some((policy, account)) = crate::commands::project::own_policy(key.root())
        && policy != repository
        && account == key.account()
        && crate::commands::project::uploads_automatically(key.root()) == Some(true)
    {
        move_terminal(key.root(), repository)?;
    }
    Ok(())
}

/// The device-local repository `project`'s new conversations in `directory` are recorded in.
fn project_repository(
    project: &str,
    directory: &std::path::Path,
) -> crate::Result<(local_repository::Repository, crate::domain::repo::Repo)> {
    let repository =
        local_repository::ensure_repository(project, directory).context(Stage::Repository)?;
    let lineage =
        crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
            .context(Stage::Repository)?;
    let repo = local_repository::require(&lineage).context(Stage::Repository)?;
    Ok((repository, repo))
}

/// Moves `folder`'s terminal uploads to `repository` with `agit on --repo`.
fn move_terminal(folder: &std::path::Path, repository: &str) -> crate::Result<()> {
    let output = crate::infra::background::command(crate::infra::own_image::own_image()?)
        .args(["on", "--repo", repository])
        .arg(folder)
        .env("AGIT_YES", "1")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_MERGE_TX")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    ensure!(
        output.status.success(),
        "the folder's terminal uploads could not be moved"
    );
    Ok(())
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

    const HUB: &str = "https://hub.example";
    const OWN: &str = "00000000-0000-0000-0000-00000000000a";
    const TEAM: &str = "00000000-0000-0000-0000-00000000000b";
    const ORG: &str = "00000000-0000-0000-0000-00000000000c";

    /// A device home signed in to the fixture Hub, with a folder bound as project `p`.
    struct Folder {
        _home: tempfile::TempDir,
        directory: PathBuf,
        /// Each conversation started here, by branch, with the repository that records it.
        conversations:
            std::cell::RefCell<std::collections::BTreeMap<String, local_repository::Repository>>,
    }

    fn sign_in(account: &str) {
        crate::infra::credentials::save(
            HUB,
            &crate::infra::credentials::HubCredential {
                account_id: Some(account.into()),
                username: account.into(),
                email: None,
                hub: Some(HUB.into()),
                access_token: "fixture".into(),
                refresh_token: "fixture".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    fn fixture(account: &str) -> Folder {
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
            std::env::set_var("AGIT_HUB_URL", HUB);
        }
        crate::rc::select_local_authority();
        sign_in(account);
        let directory = home.path().join("burrow");
        std::fs::create_dir(&directory).unwrap();
        let directory = directory.canonicalize().unwrap();
        local_repository::ensure_repository("p", &directory).unwrap();
        Folder {
            _home: home,
            directory,
            conversations: Default::default(),
        }
    }

    fn open(repository: &local_repository::Repository) -> crate::domain::repo::Repo {
        local_repository::require(
            &crate::rc::lineage::AgitSession::new(&repository.slug, &repository.agent_id, "main")
                .unwrap(),
        )
        .unwrap()
    }

    impl Folder {
        fn key(&self, account: &str) -> crate::rc::folder_destination::Key {
            crate::rc::folder_destination::Key::new(&self.directory, HUB, account).unwrap()
        }

        /// The repository `p`'s new conversations are recorded in now.
        fn repository(&self) -> local_repository::Repository {
            local_repository::ensure_repository("p", &self.directory).unwrap()
        }

        /// Starts a conversation of `p` on the web, on `branch`.
        fn start(&self, branch: &str) {
            let repository = self.repository();
            local_repository::note_session_project(&open(&repository), branch, "p").unwrap();
            self.conversations
                .borrow_mut()
                .insert(branch.to_owned(), repository);
        }

        /// Saves `repository` as the destination of `p`'s repository, as a first `agit push --to`
        /// does, and returns the line it says.
        fn push_to(&self, repository: &str, id: &str) -> Option<String> {
            let repo = open(&self.repository());
            let identity = RemoteIdentity::new(HUB, id).unwrap();
            let selection =
                local_repository::publication::Selection::prepare(&repo, Some(repository), HUB)
                    .unwrap()
                    .unwrap();
            let first = selection.is_first();
            selection.bind(&repo, &identity).unwrap();
            first
                .then(|| {
                    local_repository::publication::first_choice(&repo, repository, &identity)
                        .unwrap()
                })
                .flatten()
        }

        fn target(
            &self,
            repository: &str,
            id: &str,
            extra: serde_json::Value,
        ) -> crate::hub::project_publication::Target {
            let mut entry = serde_json::json!({
                "web_workspace_id": "w", "web_project_id": "wp",
                "workspace_id": crate::rc::endpoint::WORKSPACE, "project_id": "p",
                "local_path": self.directory.to_str().unwrap(),
                "repository": repository, "repository_id": id,
            });
            for (name, value) in extra.as_object().unwrap() {
                entry[name] = value.clone();
            }
            serde_json::from_value(entry).unwrap()
        }

        /// The Hub's binding reaching the device, as reconcile takes it once the destination's
        /// checks pass: judged, chosen when it is the device's own account's, and configured in
        /// the repository the project's new conversations are recorded in. Returns the decision
        /// and the folders whose terminal uploads moved.
        fn reconcile(
            &self,
            target: &crate::hub::project_publication::Target,
        ) -> (Decision, Vec<(PathBuf, String)>) {
            let account = crate::infra::credentials::account_key(HUB).unwrap();
            let key = self.key(&account);
            let destination = RemoteIdentity::new(HUB, &target.repository_id).unwrap();
            let decision = decide(&open(&self.repository()), &key, target, &destination).unwrap();
            let mut moved = Vec::new();
            if decision == Decision::Own {
                choose_own_binding(
                    &key,
                    &target.project_id,
                    &target.repository,
                    &destination,
                    |folder, repository| {
                        moved.push((folder.to_owned(), repository.to_owned()));
                        Ok(())
                    },
                )
                .unwrap();
            }
            if decision != Decision::Hold {
                let (_, repo) = project_repository("p", &self.directory).unwrap();
                local_repository::publication::Selection::prepare(
                    &repo,
                    Some(&target.repository),
                    HUB,
                )
                .unwrap()
                .unwrap()
                .bind(&repo, &destination)
                .unwrap();
            }
            (decision, moved)
        }

        /// Where the conversation on `branch` uploads at its next settle, `None` when it holds.
        fn uploads(&self, branch: &str) -> Option<String> {
            let repository = self.conversations.borrow()[branch].clone();
            let lineage = crate::rc::lineage::AgitSession::new(
                &repository.slug,
                &repository.agent_id,
                branch,
            )
            .unwrap();
            match crate::rc::capture::confirmed_destination(&open(&repository), &lineage, None)
                .unwrap()
            {
                crate::rc::capture::Confirmed::Ready(destination) => Some(destination.repository),
                _ => None,
            }
        }

        /// Where archive recovery sends the conversation on `branch`, `None` when it waits.
        fn recovers(&self, branch: &str) -> Option<String> {
            let repository = self.conversations.borrow()[branch].clone();
            let lineage = crate::rc::lineage::AgitSession::new(
                &repository.slug,
                &repository.agent_id,
                branch,
            )
            .unwrap();
            open(&repository).set_auto_push(Some(true)).unwrap();
            crate::rc::supervisor::archive_recovery::publication_target(
                &open(&repository),
                &lineage,
                "claude-code",
                branch,
            )
            .ok()
            .map(|destination| destination.repository)
        }

        /// Where archive recovery sends the conversation on `branch` as the folder's settings
        /// stand, `None` when it waits.
        fn recovers_as_set(&self, branch: &str) -> Option<String> {
            let repository = self.conversations.borrow()[branch].clone();
            let lineage = crate::rc::lineage::AgitSession::new(
                &repository.slug,
                &repository.agent_id,
                branch,
            )
            .unwrap();
            crate::rc::supervisor::archive_recovery::publication_target(
                &open(&repository),
                &lineage,
                "claude-code",
                branch,
            )
            .ok()
            .map(|destination| destination.repository)
        }

        fn chosen(&self, account: &str) -> String {
            crate::rc::folder_destination::chosen(&self.key(account))
                .unwrap()
                .unwrap()
                .repository
        }
    }

    /// Yuki's case: the folder first uploaded to her own repository by her own choice, and a
    /// workspace project bound later over it to a team repository. Bound by another account (a
    /// collaborator with full workspace access), the binding changes nothing: a new conversation
    /// of the project holds, and the folder keeps her choice. Bound by her own account, it becomes
    /// the folder's destination, its terminal uploads included, for conversations that start
    /// after it. A one-off push never changes a destination the folder has.
    #[test]
    fn a_web_binding_retargets_a_folder_only_when_the_devices_own_account_made_it() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_web_binding_retargets_a_folder_only_when_the_devices_own_account_made_it",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        assert_eq!(
            folder.push_to("yuki/burrow-rc", OWN).as_deref(),
            Some("this folder now uploads to yuki/burrow-rc")
        );
        crate::commands::project::save_policy_for_test(
            &folder.directory,
            "yuki/burrow-rc",
            &RemoteIdentity::new(HUB, OWN).unwrap(),
            "account-yuki",
        );

        let by_hao = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(folder.reconcile(&by_hao), (Decision::Hold, vec![]));
        folder.start("desktop-b");
        assert_eq!(
            folder.uploads("desktop-b"),
            None,
            "held, not sent to the folder's destination"
        );
        assert_eq!(
            folder.chosen("account-yuki"),
            "yuki/burrow-rc",
            "the folder keeps her choice"
        );
        assert_eq!(
            crate::rc::folder_destination::unasked(&folder.key("account-yuki")).unwrap(),
            None,
            "a binding the Hub says someone else made is not asked about"
        );

        let by_yuki = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(
            folder.reconcile(&by_yuki),
            (
                Decision::Own,
                vec![(folder.directory.clone(), "simco/burrow-agents".to_owned())]
            )
        );
        folder.start("desktop-c");
        assert_eq!(
            folder.uploads("desktop-c").as_deref(),
            Some("simco/burrow-agents")
        );
        assert_eq!(
            folder.uploads("desktop-b"),
            None,
            "a held conversation doesn't follow"
        );
        assert_eq!(folder.push_to("simco/burrow-agents", TEAM), None);
        assert!(
            local_repository::publication::Selection::prepare(
                &open(&folder.repository()),
                Some("yuki/elsewhere"),
                HUB
            )
            .is_err(),
            "a one-off push doesn't retarget the folder"
        );
        assert_eq!(folder.chosen("account-yuki"), "simco/burrow-agents");
    }

    /// A conversation keeps the destination it started with. After the project's destination
    /// changes, its next settle and archive recovery still go to the earlier repository, a new
    /// conversation goes to the new one, and nothing of the first goes to the new one. A
    /// conversation whose start rule 1 forbade, like Yuki's workspace sessions to burrow-rc,
    /// doesn't continue there and doesn't move: it holds.
    #[test]
    fn a_conversation_keeps_the_destination_it_started_with() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_conversation_keeps_the_destination_it_started_with",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        let first = folder.target(
            "yuki/burrow-rc",
            OWN,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&first).0, Decision::Own);
        folder.start("desktop-a");
        assert_eq!(
            folder.uploads("desktop-a").as_deref(),
            Some("yuki/burrow-rc")
        );

        let moved = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&moved).0, Decision::Own);
        folder.start("desktop-b");
        assert_eq!(
            folder.uploads("desktop-a").as_deref(),
            Some("yuki/burrow-rc"),
            "the next settle"
        );
        assert_eq!(
            folder.recovers("desktop-a").as_deref(),
            Some("yuki/burrow-rc"),
            "archive recovery"
        );
        assert_eq!(
            folder.uploads("desktop-b").as_deref(),
            Some("simco/burrow-agents")
        );
        assert_eq!(
            folder.recovers("desktop-b").as_deref(),
            Some("simco/burrow-agents")
        );
        assert_ne!(
            folder.conversations.borrow()["desktop-a"].agent_id,
            folder.conversations.borrow()["desktop-b"].agent_id,
            "the first conversation's turns aren't in the repository the new destination gets"
        );

        // Yuki's workspace sessions: going to her own repository, which the folder took from an
        // earlier choice while the project is bound to the team's.
        let forbidden = fixture("account-yuki");
        forbidden.push_to("yuki/burrow-rc", OWN);
        forbidden.start("desktop-hao");
        let team = forbidden.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(forbidden.reconcile(&team).0, Decision::Hold);
        assert_eq!(forbidden.uploads("desktop-hao"), None);
        assert_eq!(forbidden.recovers("desktop-hao"), None);
    }

    /// Lin's case: a personal project bound over a folder whose device-local repository first
    /// uploaded to an organization repository. Her own binding is used for new conversations;
    /// the organization's destination, inherited from that earlier choice, is not.
    #[test]
    fn a_personal_project_over_a_folder_first_sent_to_an_org_uses_its_own_binding() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_personal_project_over_a_folder_first_sent_to_an_org_uses_its_own_binding",
        ) {
            return;
        }
        let folder = fixture("account-lin");
        let repo = open(&folder.repository());
        local_repository::publication::Selection::prepare(&repo, Some("simco/lumen-agents"), HUB)
            .unwrap()
            .unwrap()
            .bind(&repo, &RemoteIdentity::new(HUB, ORG).unwrap())
            .unwrap();
        folder.start("desktop-a");
        assert_eq!(
            folder.uploads("desktop-a"),
            None,
            "an earlier destination with no record isn't used"
        );
        let personal = folder.target(
            "lin/lumen",
            OWN,
            serde_json::json!({"bound_by": "account-lin"}),
        );
        assert_eq!(folder.reconcile(&personal), (Decision::Own, vec![]));
        folder.start("desktop-b");
        assert_eq!(folder.uploads("desktop-b").as_deref(), Some("lin/lumen"));
        assert_eq!(folder.uploads("desktop-a"), None);
    }

    /// A binding from before the Hub recorded who made it is taken only when the folder has no
    /// destination and the device's account is the only one that can bind in the workspace;
    /// otherwise it holds and is asked about once. Under another sign-in nothing chosen for this
    /// account is used.
    #[test]
    fn an_older_binding_is_taken_only_from_a_sole_binder_and_otherwise_asked_about() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::an_older_binding_is_taken_only_from_a_sole_binder_and_otherwise_asked_about",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        folder.start("desktop-a");
        let shared = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"sole_binder": false}),
        );
        assert_eq!(folder.reconcile(&shared), (Decision::Hold, vec![]));
        assert_eq!(folder.uploads("desktop-a"), None);
        let key = folder.key("account-yuki");
        let (project, binding) = crate::rc::folder_destination::unasked(&key)
            .unwrap()
            .unwrap();
        assert_eq!(
            (project.as_str(), binding.repository.as_str()),
            ("p", "simco/burrow-agents")
        );
        assert!(!crate::rc::folder_destination::answer(&key, &project, &binding, false).unwrap());
        assert_eq!(folder.reconcile(&shared), (Decision::Hold, vec![]));
        assert_eq!(
            crate::rc::folder_destination::unasked(&key).unwrap(),
            None,
            "asked once"
        );
        let unrecorded = folder.target("simco/burrow-agents", TEAM, serde_json::json!({}));
        assert_eq!(
            folder.reconcile(&unrecorded),
            (Decision::Hold, vec![]),
            "a Hub without the field"
        );

        let sole = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"sole_binder": true}),
        );
        assert_eq!(folder.reconcile(&sole), (Decision::Own, vec![]));
        folder.start("desktop-b");
        assert_eq!(
            folder.uploads("desktop-b").as_deref(),
            Some("simco/burrow-agents")
        );

        sign_in("account-other");
        folder.start("desktop-c");
        assert_eq!(
            folder.uploads("desktop-c"),
            None,
            "another account's choice isn't used"
        );
        assert_eq!(
            folder.uploads("desktop-b"),
            None,
            "nor its conversations, under another sign-in"
        );
        sign_in("account-yuki");
        assert_eq!(
            folder.uploads("desktop-b").as_deref(),
            Some("simco/burrow-agents")
        );
    }

    /// A reconcile that confirms nothing leaves the project's new conversations here, and they
    /// upload once the device's own binding is confirmed. A destination this computer already
    /// uploads the folder's only project to stays its destination at upgrade; with a second
    /// project over the folder it is not taken.
    #[test]
    fn a_failed_reconcile_holds_and_an_upgrade_keeps_only_an_uninherited_destination() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_failed_reconcile_holds_and_an_upgrade_keeps_only_an_uninherited_destination",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        folder.push_to("yuki/burrow-rc", OWN);
        // The binding reached the device, but its checks failed before it was confirmed.
        let key = folder.key("account-yuki");
        let by_yuki = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(
            decide(
                &open(&folder.repository()),
                &key,
                &by_yuki,
                &RemoteIdentity::new(HUB, TEAM).unwrap()
            )
            .unwrap(),
            Decision::Own
        );
        folder.start("desktop-a");
        assert_eq!(
            folder.uploads("desktop-a"),
            None,
            "nothing goes to the old destination"
        );
        assert_eq!(folder.reconcile(&by_yuki).0, Decision::Own);
        folder.start("desktop-b");
        assert_eq!(
            folder.uploads("desktop-b").as_deref(),
            Some("simco/burrow-agents")
        );

        let only = fixture("account-yuki");
        let repo = open(&only.repository());
        local_repository::publication::Selection::prepare(&repo, Some("yuki/burrow-rc"), HUB)
            .unwrap()
            .unwrap()
            .bind(&repo, &RemoteIdentity::new(HUB, OWN).unwrap())
            .unwrap();
        let same = only.target(
            "yuki/burrow-rc",
            OWN,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(
            only.reconcile(&same).0,
            Decision::Current,
            "kept: this project's own"
        );
        only.start("desktop-a");
        assert_eq!(only.uploads("desktop-a").as_deref(), Some("yuki/burrow-rc"));

        let shared = fixture("account-yuki");
        let repo = open(&shared.repository());
        local_repository::publication::Selection::prepare(&repo, Some("yuki/burrow-rc"), HUB)
            .unwrap()
            .unwrap()
            .bind(&repo, &RemoteIdentity::new(HUB, OWN).unwrap())
            .unwrap();
        local_repository::ensure_repository("q", &shared.directory).unwrap();
        let same = shared.target(
            "yuki/burrow-rc",
            OWN,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(
            shared.reconcile(&same).0,
            Decision::Hold,
            "two projects share the folder"
        );
        shared.start("desktop-a");
        assert_eq!(shared.uploads("desktop-a"), None);
    }

    /// The person's terminal choice for a folder is where its workspace conversations upload
    /// too, when their project's binding names it, and only then.
    #[test]
    fn terminal_and_workspace_conversations_of_a_folder_follow_one_destination() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::terminal_and_workspace_conversations_of_a_folder_follow_one_destination",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        crate::commands::project::save_policy_for_test(
            &folder.directory,
            "yuki/burrow-rc",
            &RemoteIdentity::new(HUB, OWN).unwrap(),
            "account-yuki",
        );
        folder.start("desktop-a");
        assert_eq!(
            folder.uploads("desktop-a").as_deref(),
            Some("yuki/burrow-rc"),
            "the folder's terminal choice"
        );
        let elsewhere = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(folder.reconcile(&elsewhere).0, Decision::Hold);
        folder.start("desktop-b");
        assert_eq!(
            folder.uploads("desktop-b"),
            None,
            "its project is bound elsewhere"
        );
    }

    /// Lin's case, one destination per folder: a terminal policy for an organization repository
    /// and a workspace project her own account binds to her personal one. Whichever of the two
    /// she chose last is the folder's destination; a web-started conversation of the project
    /// whose binding names the other holds, and none ever goes to the other project's
    /// repository. A binding already applied isn't applied again, so a later terminal choice
    /// stays.
    #[test]
    fn a_folder_has_one_destination_the_latest_own_choice_and_nothing_crosses() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_folder_has_one_destination_the_latest_own_choice_and_nothing_crosses",
        ) {
            return;
        }
        let org = RemoteIdentity::new(HUB, ORG).unwrap();
        // The terminal choice first, then her web binding: the folder and its terminal uploads
        // move to the personal repository.
        let terminal_first = fixture("account-lin");
        crate::commands::project::save_policy_for_test(
            &terminal_first.directory,
            "simco/lumen-agents",
            &org,
            "account-lin",
        );
        let personal = terminal_first.target(
            "lin/lumen",
            OWN,
            serde_json::json!({"bound_by": "account-lin"}),
        );
        assert_eq!(
            terminal_first.reconcile(&personal),
            (
                Decision::Own,
                vec![(terminal_first.directory.clone(), "lin/lumen".to_owned())]
            )
        );
        terminal_first.start("desktop-a");
        assert_eq!(
            terminal_first.uploads("desktop-a").as_deref(),
            Some("lin/lumen")
        );
        assert_eq!(terminal_first.chosen("account-lin"), "lin/lumen");

        // Her terminal choice turned off with `agit off`: the web binding is the folder's, and the
        // terminal policy stays off and where it was. Moving it runs `agit on`, which would turn
        // her uploads back on.
        let turned_off = fixture("account-lin");
        crate::commands::project::save_policy_for_test(
            &turned_off.directory,
            "simco/lumen-agents",
            &org,
            "account-lin",
        );
        crate::commands::project::turn_policy_off_for_test(&turned_off.directory);
        let personal = turned_off.target(
            "lin/lumen",
            OWN,
            serde_json::json!({"bound_by": "account-lin"}),
        );
        assert_eq!(turned_off.reconcile(&personal), (Decision::Own, vec![]));
        assert_eq!(
            crate::commands::project::uploads_automatically(&turned_off.directory),
            Some(false)
        );

        // The Hub binding the folder to the repository her turned-off terminal policy names, as
        // it does for a folder whose origin that repository is linked to: nothing moves, and the
        // policy stays off. This is the path a device that reports nothing takes too.
        let same = fixture("account-lin");
        crate::commands::project::save_policy_for_test(
            &same.directory,
            "simco/lumen-agents",
            &org,
            "account-lin",
        );
        crate::commands::project::turn_policy_off_for_test(&same.directory);
        let terminal_repository = same.target(
            "simco/lumen-agents",
            ORG,
            serde_json::json!({"bound_by": "account-lin"}),
        );
        assert_eq!(same.reconcile(&terminal_repository).1, vec![]);
        assert_eq!(
            crate::commands::project::uploads_automatically(&same.directory),
            Some(false)
        );

        // Her web binding first, then `agit on --repo` for the organization: the folder is the
        // organization's, and the personal project's new conversations hold, even after the next
        // reconcile.
        let web_first = fixture("account-lin");
        let personal = web_first.target(
            "lin/lumen",
            OWN,
            serde_json::json!({"bound_by": "account-lin"}),
        );
        assert_eq!(web_first.reconcile(&personal).0, Decision::Own);
        web_first.start("desktop-a");
        assert_eq!(web_first.uploads("desktop-a").as_deref(), Some("lin/lumen"));
        crate::commands::project::save_policy_for_test(
            &web_first.directory,
            "simco/lumen-agents",
            &org,
            "account-lin",
        );
        crate::rc::folder_destination::choose(
            &web_first.key("account-lin"),
            "simco/lumen-agents",
            ORG,
            crate::rc::folder_destination::ChosenFor::Folder,
        )
        .unwrap();
        assert_eq!(web_first.reconcile(&personal), (Decision::Hold, vec![]));
        web_first.start("desktop-b");
        assert_eq!(
            web_first.uploads("desktop-b"),
            None,
            "held, never sent to the organization"
        );
        assert_eq!(web_first.recovers("desktop-b"), None);
        assert_eq!(
            web_first.uploads("desktop-a").as_deref(),
            Some("lin/lumen"),
            "it keeps its start"
        );
        assert_eq!(web_first.chosen("account-lin"), "simco/lumen-agents");
    }

    /// What a folder bind reports to the Hub: the account's own record for the folder, else its
    /// terminal choice, and whether its uploads are off; read without writing, so a bind the
    /// person cancels, or one the Hub answers with a choice, leaves no record of a destination
    /// they never confirmed. A report that wrote the record would make the next binding read as
    /// "already the folder's destination".
    #[test]
    fn a_folder_bind_reports_where_the_folder_uploads_without_recording_it() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_folder_bind_reports_where_the_folder_uploads_without_recording_it",
        ) {
            return;
        }
        let folder = fixture("account-lin");
        let report =
            || local_repository::publication::folder_destination(&folder.directory, HUB).unwrap();
        assert_eq!(report(), None, "nothing chosen");
        crate::commands::project::save_policy_for_test(
            &folder.directory,
            "simco/lumen-agents",
            &RemoteIdentity::new(HUB, ORG).unwrap(),
            "account-lin",
        );
        assert_eq!(
            report(),
            Some(crate::protocol::PublicationTarget {
                repository: "simco/lumen-agents".into(),
                agent_id: ORG.into(),
            }),
            "the terminal choice"
        );
        assert!(
            crate::rc::folder_destination::chosen(&folder.key("account-lin"))
                .unwrap()
                .is_none(),
            "reporting it records nothing"
        );
        crate::commands::project::turn_policy_off_for_test(&folder.directory);
        assert_eq!(
            crate::commands::project::uploads_automatically(&folder.directory),
            Some(false),
            "a folder turned off reports so"
        );
    }

    /// Lin's folder at upgrade: her terminal policy names the organization repository, her
    /// personal project's binding is older than the Hub's record of who made it, and its
    /// device-local repository uploaded to the personal one. The folder keeps her terminal choice,
    /// so her terminal sessions don't move; the personal project's conversations hold, never
    /// going to the organization, and she is asked once. Yes moves the folder to the personal
    /// repository for conversations that start after it.
    #[test]
    fn at_upgrade_a_folder_keeps_its_terminal_choice_and_asks_about_a_different_project_binding() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::at_upgrade_a_folder_keeps_its_terminal_choice_and_asks_about_a_different_project_binding",
        ) {
            return;
        }
        let folder = fixture("account-lin");
        let repo = open(&folder.repository());
        local_repository::publication::Selection::prepare(&repo, Some("lin/lumen"), HUB)
            .unwrap()
            .unwrap()
            .bind(&repo, &RemoteIdentity::new(HUB, OWN).unwrap())
            .unwrap();
        folder.start("desktop-a");
        crate::commands::project::save_policy_for_test(
            &folder.directory,
            "simco/lumen-agents",
            &RemoteIdentity::new(HUB, ORG).unwrap(),
            "account-lin",
        );
        let older = folder.target("lin/lumen", OWN, serde_json::json!({"sole_binder": true}));
        assert_eq!(folder.reconcile(&older), (Decision::Hold, vec![]));
        assert_eq!(
            folder.uploads("desktop-a"),
            None,
            "never to the organization"
        );
        let key = folder.key("account-lin");
        assert_eq!(
            folder.chosen("account-lin"),
            "simco/lumen-agents",
            "her terminal sessions keep it"
        );
        let (project, binding) = crate::rc::folder_destination::unasked(&key)
            .unwrap()
            .unwrap();
        assert_eq!(binding.repository, "lin/lumen");
        assert!(crate::rc::folder_destination::answer(&key, &project, &binding, true).unwrap());
        assert_eq!(folder.reconcile(&older).0, Decision::Current);
        folder.start("desktop-b");
        assert_eq!(folder.uploads("desktop-b").as_deref(), Some("lin/lumen"));
    }

    /// A conversation is judged by the project that started it. One the terminal side started
    /// and captured, in the folder's terminal repository, keeps uploading there after the web
    /// observes or resumes it, whatever the workspace project's binding says, at its settles and
    /// in archive recovery alike.
    #[test]
    fn a_terminal_conversation_opened_on_the_web_keeps_its_terminal_destination() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_terminal_conversation_opened_on_the_web_keeps_its_terminal_destination",
        ) {
            return;
        }
        let folder = fixture("account-lin");
        let org = RemoteIdentity::new(HUB, ORG).unwrap();
        crate::commands::project::save_policy_for_test(
            &folder.directory,
            "simco/lumen-agents",
            &org,
            "account-lin",
        );
        let checkout = crate::domain::repo::Repo::init(
            &crate::infra::config::repo_dir("simco", "lumen-agents").unwrap(),
        )
        .unwrap();
        crate::hub::identity::pin(&checkout, &org).unwrap();
        checkout.set_auto_push(Some(true)).unwrap();
        let claim =
            crate::rc::lineage::AgitSession::new("simco/lumen-agents", ORG, "lumen-terminal")
                .unwrap();
        let personal = folder.target(
            "lin/lumen",
            OWN,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(folder.reconcile(&personal).0, Decision::Hold);
        assert!(matches!(
            crate::rc::capture::confirmed_destination(&checkout, &claim, Some("p")).unwrap(),
            crate::rc::capture::Confirmed::Ready(destination) if destination.repository == "simco/lumen-agents"
        ));
        assert_eq!(
            crate::rc::supervisor::archive_recovery::publication_target(
                &checkout,
                &claim,
                "claude-code",
                "native"
            )
            .unwrap()
            .repository,
            "simco/lumen-agents"
        );
        folder.start("desktop-a");
        assert_eq!(
            folder.uploads("desktop-a"),
            None,
            "a web-started one of the project holds"
        );
        assert_eq!(folder.recovers("desktop-a"), None);
    }

    /// The device's own account binds the project first, and another account rebinds it later:
    /// a conversation started before keeps its destination, and one started after holds instead
    /// of going where the project was first bound.
    #[test]
    fn a_rebinding_by_someone_else_holds_new_conversations() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_rebinding_by_someone_else_holds_new_conversations",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        let own = folder.target(
            "yuki/burrow-rc",
            OWN,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&own).0, Decision::Own);
        folder.start("desktop-a");
        assert_eq!(
            folder.uploads("desktop-a").as_deref(),
            Some("yuki/burrow-rc")
        );
        let rebound = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-hao"}),
        );
        assert_eq!(folder.reconcile(&rebound).0, Decision::Hold);
        folder.start("desktop-b");
        assert_eq!(
            folder.uploads("desktop-b"),
            None,
            "not to the project's first binding"
        );
        assert_eq!(folder.recovers("desktop-b"), None);
        assert_eq!(
            folder.uploads("desktop-a").as_deref(),
            Some("yuki/burrow-rc")
        );
    }

    /// `agit off` turns off every repository holding the folder's conversations, one a project no
    /// longer starts conversations in included, and a folder turned off stays off in the repository
    /// a later binding gives its new conversations, so a binding from the web never turns it back
    /// on.
    #[test]
    fn a_folder_turned_off_stays_off_across_a_change_of_destination() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::a_folder_turned_off_stays_off_across_a_change_of_destination",
        ) {
            return;
        }
        // Off after the change: the earlier repository's conversation stops too.
        let folder = fixture("account-yuki");
        let first = folder.target(
            "yuki/burrow-rc",
            OWN,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&first).0, Decision::Own);
        folder.start("desktop-a");
        assert_eq!(
            folder.recovers("desktop-a").as_deref(),
            Some("yuki/burrow-rc")
        );
        let second = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&second).0, Decision::Own);
        folder.start("desktop-b");
        assert_eq!(
            crate::commands::front::on::turn_off_bound_folders(&folder.directory)
                .unwrap()
                .len(),
            2,
            "both repositories holding the folder's conversations"
        );
        assert_eq!(
            folder.recovers("desktop-a"),
            None,
            "the earlier repository is off too"
        );
        assert_eq!(folder.recovers("desktop-b"), None);

        // Off before the change: the repository the new binding gives is off as well.
        let folder = fixture("account-yuki");
        assert_eq!(folder.reconcile(&first).0, Decision::Own);
        folder.start("desktop-a");
        crate::commands::front::on::turn_off_bound_folders(&folder.directory).unwrap();
        assert_eq!(folder.reconcile(&second).0, Decision::Own);
        let replaced = open(&folder.repository());
        assert!(replaced.uploads_turned_off().unwrap());
        assert!(
            !crate::commands::auto_push::publication_allowed(&replaced, "simco/burrow-agents")
                .unwrap()
        );
    }

    /// A conversation in a repository the project no longer starts conversations in, after the
    /// folder is turned off and on again: what it said meanwhile stays on this computer, as for
    /// every conversation the folder held when it was turned on, until the person saves it.
    #[test]
    fn turning_a_folder_back_on_keeps_an_earlier_repositorys_conversation_here() {
        if crate::rc::in_isolated_test(
            "rc::daemon::project_publication::tests::turning_a_folder_back_on_keeps_an_earlier_repositorys_conversation_here",
        ) {
            return;
        }
        let folder = fixture("account-yuki");
        let first = folder.target(
            "yuki/burrow-rc",
            OWN,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&first).0, Decision::Own);
        folder.start("desktop-a");
        open(&folder.conversations.borrow()["desktop-a"])
            .set_auto_push(Some(true))
            .unwrap();
        assert_eq!(
            folder.recovers_as_set("desktop-a").as_deref(),
            Some("yuki/burrow-rc")
        );
        let second = folder.target(
            "simco/burrow-agents",
            TEAM,
            serde_json::json!({"bound_by": "account-yuki"}),
        );
        assert_eq!(folder.reconcile(&second).0, Decision::Own);
        crate::commands::front::on::turn_off_bound_folders(&folder.directory).unwrap();
        assert_eq!(folder.recovers_as_set("desktop-a"), None, "off");
        crate::commands::front::on::turn_on_bound_folders(&folder.directory, |_| {
            Ok(vec!["claude-code:desktop-a".to_owned()])
        })
        .unwrap();
        assert_eq!(
            folder.recovers_as_set("desktop-a"),
            None,
            "it was there while the folder was off, so it stays here until it is saved"
        );
    }
}
