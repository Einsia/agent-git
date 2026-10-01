//! Delivery selects private evidence locally and accepts receipts under current controller authority.

use super::*;
use crate::{
    domain::{privacy_receipt::outbox::Entry, repo::Repo},
    rc::{
        authority::Guard, capture::PublicationDestination as Destination, cloud::store,
        lineage::AgitSession,
    },
};
use agit_peer::publication::{Delivery, Executor};
use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

const BATCH_SIZE: usize = 16;
const BATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const BATCH_WORK_BUDGET: std::time::Duration = std::time::Duration::from_secs(25);
const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
                "publication delivery is busy; retry shortly",
            ))
        } else {
            state.prepare_publication(frame)
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    session_id: String,
    // caller_scope binds this shared routing field to the authenticated caller.
    #[serde(default, rename = "workspace_id")]
    _workspace_id: Option<String>,
    #[serde(default)]
    after: Option<String>,
}

pub(super) struct Prepared {
    authority: Guard,
    logical: String,
    native: String,
    native_source: Option<crate::protocol::NativeSourceRef>,
    runtime: String,
    lineage: AgitSession,
    credential_dir: PathBuf,
    after: Option<String>,
}

impl Daemon {
    pub(super) fn prepare_publication(&self, frame: &Frame) -> Result<Prepared, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        let request: Request = frame.params_as()?;
        if request
            .after
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
        {
            return Err(RpcError::new(
                ErrorCode::MalformedFrame,
                "invalid publication cursor",
            ));
        }
        let grant = frame.authority.publication_grant()?;
        let scope = grant.session_controller.as_ref().ok_or_else(forbidden)?;
        if !self.opts.local_owner || caller.workspace_id != crate::rc::endpoint::WORKSPACE {
            return Err(forbidden());
        }
        let (logical, row, native) = native_binding(
            &self.roster,
            &scope.runtime,
            &scope.session_id,
            &caller.workspace_id,
        )
        .ok_or_else(unbound)?;
        if request.session_id != logical && request.session_id != scope.session_id {
            return Err(forbidden());
        }
        let mut lineage = AgitSession::parse(
            row.agit_session.as_deref().ok_or_else(unbound)?,
            row.expected_agent_id.as_deref().ok_or_else(unbound)?,
        )
        .map_err(|_| forbidden())?;
        lineage.capture = self.roster.captures.get(logical).cloned();
        Ok(Prepared {
            authority: frame.authority.clone(),
            logical: logical.into(),
            native: native.into(),
            native_source: row.native_source.clone(),
            runtime: scope.runtime.clone(),
            lineage,
            credential_dir: store::directory().map_err(|_| unavailable())?,
            after: request.after,
        })
    }
}

fn native_binding<'a>(
    roster: &'a Roster,
    runtime: &str,
    reference: &str,
    workspace: &str,
) -> Option<(&'a str, &'a crate::rc::roster::Entry, &'a str)> {
    roster.sessions.iter().find_map(|(logical, row)| {
        if row.runtime != runtime || row.workspace_id != workspace {
            return None;
        }
        let native = std::iter::once(&row.thread_id)
            .chain(row.prior_threads.iter())
            .find(|native| match &row.native_source {
                Some(source) => source.session_ref(native) == reference,
                None => native.as_str() == reference,
            })?;
        Some((logical.as_str(), row, native.as_str()))
    })
}

impl Prepared {
    fn check_lineage(&self, daemon: &Daemon) -> Result<(), RpcError> {
        let row = daemon.roster.get(&self.logical).ok_or_else(forbidden)?;
        if !daemon.opts.local_owner
            || row.workspace_id != crate::rc::endpoint::WORKSPACE
            || row.runtime != self.runtime
            || row.native_source != self.native_source
            || (row.thread_id != self.native && !row.prior_threads.contains(&self.native))
            || row.agit_session.as_deref() != Some(self.lineage.to_string().as_str())
            || row.expected_agent_id.as_deref() != Some(self.lineage.agent_id())
            || daemon.roster.captures.get(&self.logical) != self.lineage.capture.as_ref()
        {
            return Err(forbidden());
        }
        Ok(())
    }

    pub(super) async fn execute(
        self,
        daemon: Arc<Mutex<Daemon>>,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Value, RpcError> {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => Err(unavailable()),
            result = tokio::time::timeout(BATCH_TIMEOUT, self.deliver(daemon)) => result.unwrap_or_else(|_| Err(unavailable())),
        }
    }

    async fn deliver(&self, daemon: Arc<Mutex<Daemon>>) -> Result<Value, RpcError> {
        let deadline = tokio::time::Instant::now() + BATCH_WORK_BUDGET;
        let grant = self.authority.publication_grant()?;
        let lineage = self.lineage.clone();
        let (native, runtime, hub) = (
            self.native.clone(),
            self.runtime.clone(),
            grant.target.owner.issuer.clone(),
        );
        let (repo_path, destination, records) = blocking(move || {
            let path = lineage.repo_dir()?;
            let repo = Repo::open(&path).context("publication repository is missing")?;
            let destination = crate::rc::capture::destination(&repo, &lineage, &hub)?;
            let records = Entry::records(&repo, lineage.branch(), &native, &runtime)?;
            Ok((path, destination, records))
        })
        .await?;
        let Some(destination) = destination else {
            self.authority.publication_grant()?;
            self.check_lineage(&*daemon.lock().await)?;
            return Ok(setup_response(
                &self.logical,
                &records,
                crate::protocol::PublicationReason::DestinationMissing,
            ));
        };
        let path = repo_path.clone();
        let target = destination.clone();
        let readiness = tokio::task::spawn_blocking(move || -> crate::Result<_> {
            let repo = Repo::open(&path).context("publication repository is missing")?;
            if !repo.auto_push_enabled()? {
                return Ok(Some(crate::protocol::PublicationReason::PushDisabled));
            }
            if !crate::commands::push::automatic_publication_consent(
                &repo,
                &target.repository,
                &target.identity,
            )? {
                return Ok(Some(crate::protocol::PublicationReason::ConsentRequired));
            }
            Ok(None)
        })
        .await
        .map_err(|_| unavailable())?;
        self.authority.publication_grant()?;
        self.check_lineage(&*daemon.lock().await)?;
        if let Some(reason) = match readiness {
            Ok(reason) => reason,
            Err(_) => Some(crate::protocol::PublicationReason::EligibilityUnchecked),
        } {
            return Ok(setup_response(&self.logical, &records, reason));
        }
        let mut pending = records
            .iter()
            .filter(|entry| entry.acknowledged.is_none())
            .count();
        let selected: Vec<_> = records
            .into_iter()
            .filter(|entry| {
                self.after
                    .as_ref()
                    .is_none_or(|after| &entry.notification_id > after)
            })
            .take(BATCH_SIZE + 1)
            .collect();
        let mut next_after =
            (selected.len() > BATCH_SIZE).then(|| selected[BATCH_SIZE - 1].notification_id.clone());
        let api =
            agit_peer::client::Client::new(&destination.identity.hub).map_err(|_| unavailable())?;
        let mut items = vec![];
        let mut processed = self.after.clone();
        for entry in selected.into_iter().take(BATCH_SIZE) {
            if tokio::time::Instant::now() >= deadline {
                next_after = processed;
                break;
            }
            processed = Some(entry.notification_id.clone());
            self.check_lineage(&*daemon.lock().await)?;
            let grant = self.authority.publication_grant()?;
            if entry.request().destination != destination.identity
                || entry.request().repository != destination.repository
                || entry.capture.session_id != self.logical
            {
                return Err(forbidden());
            }
            let id = entry.notification_id.clone();
            if entry.publication.is_none() && entry.prepared.is_none() {
                items.push(json!({"notification_id":id, "status":"awaiting_publication"}));
                continue;
            }
            let (directory, hub) = (
                self.credential_dir.clone(),
                destination.identity.hub.clone(),
            );
            let enrollment = blocking(move || {
                let enrollment =
                    store::load_in(&directory, &hub)?.context("device enrollment is missing")?;
                ensure!(enrollment.inbound_enabled, "cloud access is disabled");
                Ok(enrollment)
            })
            .await?;
            let credential = enrollment.credential;
            if !same_device(&credential.device, &grant.target) {
                return Err(forbidden());
            }
            let executor = Executor {
                owner: credential.device.owner.clone(),
                device_id: credential.device.id.clone(),
                credential_epoch: credential.device.credential_epoch,
            };
            let (path, request, lineage, hub) = (
                repo_path.clone(),
                entry.request().clone(),
                self.lineage.clone(),
                destination.identity.hub.clone(),
            );
            let authority = self.authority.clone();
            let expected_destination = destination.clone();
            let native_source = self.native_source.clone();
            let notification = blocking(move || {
                let repo = Repo::open(&path).context("publication repository is missing")?;
                ensure!(
                    checked_destination(&repo, &lineage, &hub)? == expected_destination,
                    "publication destination changed"
                );
                authority.publication_grant()?;
                let notification = Entry::bind_notification_for_source(
                    &repo,
                    &request,
                    executor,
                    native_source.as_ref(),
                )
                .ok();
                if notification.is_some() {
                    let _ = Entry::reclaim_acknowledged(&repo, &request);
                }
                Ok(notification)
            })
            .await?;
            let Some(notification) = notification else {
                self.authority.publication_grant()?;
                items.push(json!({"notification_id":id, "status":"local_state_unavailable"}));
                continue;
            };
            let delivery = Delivery::new(notification, &grant, now()).map_err(|_| forbidden())?;
            let receipt = if let Some(receipt) = entry.acknowledged.clone() {
                receipt
                    .validate(&delivery.notification)
                    .map_err(|_| unavailable())?;
                receipt
            } else {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let response = match tokio::time::timeout(
                    ATTEMPT_TIMEOUT.min(remaining),
                    api.confirm_publication(&credential, &grant, &delivery),
                )
                .await
                {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(error)) => Err(if agit_peer::client::is_transient(&error) {
                        "retry"
                    } else {
                        "rejected"
                    }),
                    Err(_) => Err("retry"),
                };
                let response = match response {
                    Ok(response) => response,
                    Err(status) => {
                        self.authority.publication_grant()?;
                        items.push(json!({"notification_id":id, "status":status}));
                        continue;
                    }
                };
                self.check_lineage(&*daemon.lock().await)?;
                let (path, lineage, hub, directory, request) = (
                    repo_path.clone(),
                    self.lineage.clone(),
                    destination.identity.hub.clone(),
                    self.credential_dir.clone(),
                    entry.request().clone(),
                );
                let authority = self.authority.clone();
                let accepted = response.receipt.clone();
                let expected_device = credential.device.clone();
                let checked_delivery = delivery.clone();
                let expected_destination = destination.clone();
                blocking(move || {
                    let repo = Repo::open(&path).context("publication repository is missing")?;
                    ensure!(
                        checked_destination(&repo, &lineage, &hub)? == expected_destination,
                        "publication destination changed"
                    );
                    let current = store::load_in(&directory, &hub)?
                        .context("device enrollment is missing")?;
                    ensure!(
                        current.inbound_enabled
                            && same_device(&current.credential.device, &expected_device),
                        "executor enrollment changed"
                    );
                    let current_grant = authority.publication_grant()?;
                    response.validate(&checked_delivery, &current_grant, now())?;
                    Entry::acknowledge(&repo, &request, &response.receipt)?;
                    let _ = Entry::reclaim_acknowledged(&repo, &request);
                    Ok(())
                })
                .await?;
                pending = pending.saturating_sub(1);
                accepted
            };
            self.authority.publication_grant()?;
            let state = daemon.lock().await;
            self.check_lineage(&state)?;
            let coverage = current_coverage(&state, &entry.capture);
            items.push(json!({"notification_id":id, "status":"acknowledged", "notification":delivery.notification, "receipt":receipt, "coverage":coverage}));
        }
        self.authority.publication_grant()?;
        let status = publication_status(&items, pending);
        Ok(
            json!({"session_id":self.logical, "items":items, "pending":pending,
            "next_after":next_after, "retry_after_ms": if pending > 0 { Some(5000) } else { None }, "publication":status}),
        )
    }
}

fn setup_response(
    logical: &str,
    records: &[Entry],
    reason: crate::protocol::PublicationReason,
) -> Value {
    use crate::protocol::{
        PublicationProgress as Progress, PublicationReadiness as Readiness,
        PublicationReason as Reason, PublicationStage as Stage, SessionPublicationStatus,
    };
    let status = SessionPublicationStatus {
        readiness: if reason == Reason::EligibilityUnchecked {
            Readiness::Checking
        } else {
            Readiness::SetupRequired
        },
        progress: if records.is_empty() {
            Progress::Idle
        } else {
            Progress::LocalSaved
        },
        stage: Some(
            if matches!(reason, Reason::PushDisabled | Reason::DestinationMissing) {
                Stage::Configuration
            } else {
                Stage::Consent
            },
        ),
        reason: Some(reason),
    };
    json!({"session_id":logical,"items":[],"pending":records.iter().filter(|entry| entry.acknowledged.is_none()).count(),"next_after":null,"retry_after_ms":null,"publication":status})
}

fn publication_status(
    items: &[Value],
    pending: usize,
) -> crate::protocol::SessionPublicationStatus {
    use crate::protocol::{
        PublicationProgress as Progress, PublicationReadiness as Readiness,
        PublicationReason as Reason, PublicationStage as Stage, SessionPublicationStatus,
    };
    let (progress, stage, reason) = if items.iter().any(|item| item["status"] == "rejected") {
        (
            Progress::Failed,
            Some(Stage::Receiver),
            Some(Reason::ReceiverRejected),
        )
    } else if items.iter().any(|item| item["status"] == "retry") {
        (
            Progress::AwaitingAck,
            Some(Stage::Receiver),
            Some(Reason::ReceiverUnavailable),
        )
    } else if pending > 0 || items.iter().any(|item| item["status"] != "acknowledged") {
        (
            Progress::Publishing,
            Some(Stage::Publication),
            Some(Reason::PublicationPending),
        )
    } else if !items.is_empty() {
        (Progress::Acknowledged, None, None)
    } else {
        (Progress::Idle, None, None)
    };
    SessionPublicationStatus {
        readiness: Readiness::Ready,
        progress,
        stage,
        reason,
    }
}

fn unbound() -> RpcError {
    let mut error = RpcError::new(
        ErrorCode::Forbidden,
        "session has no verified capture binding",
    );
    error.data = Some(json!({"publication":crate::protocol::SessionPublicationStatus::unbound()}));
    error
}

fn current_coverage(daemon: &Daemon, capture: &agit_peer::publication::Capture) -> Option<Value> {
    let live = daemon.sessions.get(&capture.session_id)?;
    if capture.incarnation.as_deref() != Some(&daemon.identity.instance_id)
        || capture.generation != live.generation
        || capture.runtime != live.info.runtime
        || live.runtime_thread_id.as_deref() != Some(&capture.native_session_id)
    {
        return None;
    }
    Some(
        json!({"session_id":capture.session_id, "incarnation":capture.incarnation,
        "generation":capture.generation, "through_seq":capture.through_seq?}),
    )
}

fn checked_destination(
    repo: &Repo,
    lineage: &AgitSession,
    hub: &str,
) -> crate::Result<Destination> {
    crate::rc::capture::destination(repo, lineage, hub)?
        .context("publication destination is missing")
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> crate::Result<T> + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn forbidden() -> RpcError {
    RpcError::new(
        ErrorCode::Forbidden,
        "publication delivery is outside current session authority",
    )
}
fn unavailable() -> RpcError {
    RpcError::new(
        ErrorCode::SessionBusy,
        "publication delivery remains pending; retry after local state is available",
    )
}

pub(in crate::rc) fn same_device(
    a: &agit_peer::cloud::Device,
    b: &agit_peer::cloud::Device,
) -> bool {
    a.id == b.id
        && a.owner == b.owner
        && a.certificate == b.certificate
        && a.credential_epoch == b.credential_epoch
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::protocol::{PublicationProgress as Progress, PublicationReason as Reason};

    #[tokio::test]
    async fn publication_binding_keeps_native_store_identity_through_delivery() {
        let lineage = AgitSession::new(
            "desktop-local/project",
            "00000000-0000-0000-0000-000000000001",
            "s/work",
        )
        .unwrap();
        let mut roster = Roster::default();
        for (logical, source) in [
            ("alpha", Some("alpha")),
            ("beta", Some("beta")),
            ("legacy", None),
        ] {
            roster.record(logical, serde_json::from_value(json!({
                "runtime":"codex", "thread_id":"copied-native", "cwd":"/project",
                "workspace_id":"local-owner", "prior_threads":["earlier-native"],
                "native_source":source.map(|source_id| json!({"source_id":source_id,"generation":7})),
                "agit_session":lineage.to_string(), "expected_agent_id":lineage.agent_id()
            })).unwrap()).unwrap();
        }
        let source = roster.get("beta").unwrap().native_source.clone().unwrap();
        let reference = source.session_ref("copied-native");
        let (logical, row, native) =
            native_binding(&roster, "codex", &reference, "local-owner").unwrap();
        assert_eq!((logical, native), ("beta", "copied-native"));
        assert_eq!(row.native_source.as_ref(), Some(&source));
        assert_eq!(
            native_binding(&roster, "codex", "copied-native", "local-owner")
                .unwrap()
                .0,
            "legacy"
        );
        assert_eq!(
            native_binding(
                &roster,
                "codex",
                &source.session_ref("earlier-native"),
                "local-owner"
            )
            .unwrap()
            .0,
            "beta"
        );
        assert!(native_binding(&roster, "claude-code", &reference, "local-owner").is_none());
        assert!(native_binding(&roster, "codex", &reference, "another-workspace").is_none());
        let prepared = Prepared {
            authority: Guard::default(),
            logical: logical.into(),
            native: native.into(),
            native_source: Some(source),
            runtime: "codex".into(),
            lineage,
            credential_dir: PathBuf::new(),
            after: None,
        };
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), roster);
        let mut daemon = daemon.lock().await;
        daemon.opts.local_owner = true;
        prepared.check_lineage(&daemon).unwrap();
        daemon
            .roster
            .sessions
            .get_mut("beta")
            .unwrap()
            .native_source
            .as_mut()
            .unwrap()
            .generation += 1;
        assert!(prepared.check_lineage(&daemon).is_err());
    }

    #[test]
    fn only_receiver_evidence_reports_rejection_or_complete_acknowledgement() {
        let acknowledged = [json!({"status":"acknowledged"})];
        assert_eq!(
            publication_status(&acknowledged, 0).progress,
            Progress::Acknowledged
        );
        assert_eq!(
            publication_status(&acknowledged, 1).progress,
            Progress::Publishing
        );
        assert_eq!(
            publication_status(&[json!({"status":"local_state_unavailable"})], 1).reason,
            Some(Reason::PublicationPending)
        );
        assert_eq!(
            publication_status(&[json!({"status":"retry"})], 1).reason,
            Some(Reason::ReceiverUnavailable)
        );
        assert_eq!(
            publication_status(&[json!({"status":"rejected"})], 1).reason,
            Some(Reason::ReceiverRejected)
        );
    }
}
