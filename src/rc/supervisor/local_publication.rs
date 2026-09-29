//! Local-owner publication retains Git evidence independently of remote notification delivery.

use super::*;
use crate::domain::{
    privacy_receipt::outbox::{Capture, Entry},
    repo::Repo,
};
use anyhow::{Context, ensure};
use std::io::Write;

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn publish_local_source(
        &mut self,
        lease: SettlementState,
        repository: &str,
        branch: &str,
        local_agent_id: &str,
        source: &str,
        mut push: tokio::process::Command,
        boundary: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) {
        let mut retained = false;
        use crate::protocol::{
            PublicationProgress as Progress, PublicationReadiness as Readiness,
            PublicationReason as Reason, PublicationStage as Stage, SessionPublicationStatus,
        };
        let mut status = SessionPublicationStatus {
            progress: Progress::LocalSaved,
            ..SessionPublicationStatus::checking()
        };
        let prepared = (|| -> crate::Result<_> {
            let repo = Repo::at(std::path::Path::new(repository));
            let lineage = self
                .agit_session
                .as_ref()
                .context("capture binding is missing")?;
            let Some(destination) =
                crate::rc::capture::destination(&repo, lineage, &crate::infra::config::hub_url())?
            else {
                status.readiness = Readiness::SetupRequired;
                status.stage = Some(Stage::Configuration);
                status.reason = Some(Reason::DestinationMissing);
                return Ok(None);
            };
            ensure!(
                lineage.agent_id() == local_agent_id,
                "RC source repository identity changed"
            );
            ensure!(
                destination.identity.hub
                    == crate::hub::identity::normalize_hub(&crate::infra::config::hub_url())?,
                "RC publication Hub changed"
            );
            if !repo.auto_push_enabled()? {
                status.readiness = Readiness::SetupRequired;
                status.stage = Some(Stage::Configuration);
                status.reason = Some(Reason::PushDisabled);
                return Ok(None);
            }
            let mut request = SupervisorPushRequest {
                version: 1,
                request_id: uuid::Uuid::now_v7().to_string(),
                repository: destination.repository,
                branch: branch.into(),
                source: source.into(),
                destination: destination.identity,
                notification_id: None,
            };
            let capture = Capture {
                session_id: self.info.session_id.clone(),
                native_session_id: self
                    .driver
                    .runtime_thread_id()
                    .context("RC native session is unavailable")?,
                runtime: self.info.runtime.clone(),
                generation: self.generation,
                incarnation: self.publication_incarnation.clone(),
                through_seq: boundary
                    .as_ref()
                    .filter(|_| self.publication_incarnation.is_some() && self.generation > 0)
                    .map(|boundary| boundary.load(std::sync::atomic::Ordering::Acquire))
                    .filter(|sequence| *sequence > 0),
            };
            let intent = Entry::begin(&repo, &mut request, capture)?;
            retained = true;
            status.progress = if intent.publication.is_some() {
                Progress::AwaitingAck
            } else {
                Progress::Publishing
            };
            if intent.publication.is_some() {
                return Ok(None);
            }
            let mut result = tempfile::NamedTempFile::new_in(repo.common_dir()?)?;
            result.write_all(&serde_json::to_vec(&request)?)?;
            push.env(
                crate::hub::identity::EXPECTED_AGENT_ID_ENV,
                &request.destination.agent_id,
            )
            .env(
                crate::domain::privacy_receipt::SUPERVISOR_RESULT_ENV,
                result.path(),
            );
            Ok(Some((repo, request, result)))
        })();
        let changed = Frame::notification(
            method::SESSION_PUBLICATION_CHANGED,
            serde_json::json!({"session_id":self.info.session_id,"publication":status}),
        );
        if retained || status.readiness == Readiness::SetupRequired {
            // Local-save delivery can precede intent creation; this hint follows its durable write.
            let _ = self.out.send(changed.clone()).await;
        }
        let (repo, request, result) = match prepared {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                self.finish_publication_outcome(PublicationOutcome::Complete(None));
                return;
            }
            Err(error) => {
                self.publication_retry.failed();
                tracing_note(&format!("RC publication remains pending: {error:#}"));
                status.progress = Progress::LocalSaved;
                status.stage = Some(Stage::Capture);
                status.reason = Some(Reason::BindingInvalid);
                let _ = self
                    .out
                    .send(Frame::notification(
                        method::SESSION_PUBLICATION_CHANGED,
                        serde_json::json!({"session_id":self.info.session_id,"publication":status}),
                    ))
                    .await;
                return;
            }
        };
        let mut state = self.settlement.clone();
        let out = self.out.clone();
        let logical = self.info.session_id.clone();
        let publish = async move {
            let mut status = status;
            match guarded_output(&mut state, lease, push).await {
                Some(output) if output.status.success() => {
                    if let Err(error) = result_receipt(&repo, &request, result.path()) {
                        status.progress = Progress::Failed;
                        status.stage = Some(Stage::Publication);
                        status.reason = Some(Reason::PublicationFailed);
                        tracing_note(&format!("RC publication result is unavailable: {error:#}"));
                    } else {
                        status.readiness = Readiness::Ready;
                        status.progress = Progress::AwaitingAck;
                        status.stage = Some(Stage::Receiver);
                        status.reason = None;
                    }
                }
                Some(output) if matches!(output.status.code(), Some(code) if code == crate::ExitCode::Interactive as i32 || code == crate::ExitCode::Auth as i32) =>
                {
                    status.readiness = Readiness::SetupRequired;
                    status.progress = Progress::LocalSaved;
                    status.stage = Some(Stage::Consent);
                    status.reason = Some(Reason::ConsentRequired);
                }
                _ => {
                    status.progress = Progress::Failed;
                    status.stage = Some(Stage::Publication);
                    status.reason = Some(Reason::PublicationFailed);
                    tracing_note(
                        "RC automatic publication did not complete; the source remains local",
                    );
                }
            }
            let _ = out
                .send(Frame::notification(
                    method::SESSION_PUBLICATION_CHANGED,
                    serde_json::json!({"session_id":logical,"publication":status}),
                ))
                .await;
            // A verified Git result remains in the outbox until a durable receiver accepts it.
            if status.progress == Progress::Failed {
                PublicationOutcome::Retry
            } else {
                PublicationOutcome::Complete(None)
            }
        };
        if boundary.is_some() {
            self.publication = Some(Publication(tokio::spawn(publish)));
        } else {
            self.finish_publication_outcome(publish.await);
        }
    }
}

fn result_receipt(
    repo: &Repo,
    request: &SupervisorPushRequest,
    path: &std::path::Path,
) -> crate::Result<()> {
    let (selected, receipt) = request.read_managed_result(repo, path)?;
    // The durable record must agree with the verified child reply.
    Entry::complete(repo, &selected, &receipt)
}
