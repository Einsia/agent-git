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
        // A destination this computer isn't signed in to is an upload that didn't go through; the
        // refusal is kept to be recorded once the attempt ends.
        let mut signed_out = None;
        let prepared = (|| -> crate::Result<_> {
            let repo = Repo::at(std::path::Path::new(repository));
            let lineage = self
                .agit_session
                .as_ref()
                .context("capture binding is missing")?;
            let (destination, held) = match crate::rc::capture::confirmed_destination(
                &repo,
                lineage,
                self.info.project_id.as_deref(),
            )? {
                crate::rc::capture::Confirmed::Missing => {
                    status.readiness = Readiness::SetupRequired;
                    status.stage = Some(Stage::Configuration);
                    status.reason = Some(Reason::DestinationMissing);
                    return Ok(None);
                }
                crate::rc::capture::Confirmed::Held(destination) => (destination, true),
                crate::rc::capture::Confirmed::Ready(destination) => (destination, false),
            };
            status.target = Some(destination.target());
            ensure!(
                lineage.agent_id() == local_agent_id,
                "RC source repository identity changed"
            );
            let native_session_id = self
                .driver
                .runtime_thread_id()
                .context("RC native session is unavailable")?;
            // Off means no attempt at all: nothing is sent, recorded or shown.
            if !crate::commands::auto_push::publication_allowed(&repo, &destination.repository)? {
                return Ok(None);
            }
            // Kept here until this project's own destination is confirmed on this computer.
            if held {
                tracing_note(crate::rc::folder_destination::HELD);
                status.readiness = Readiness::SetupRequired;
                status.stage = Some(Stage::Configuration);
                status.reason = Some(Reason::DestinationUnconfirmed);
                return Ok(None);
            }
            if crate::rc::folder_cutoff::holds_back(&repo, &self.info.runtime, &native_session_id) {
                status.readiness = Readiness::SetupRequired;
                status.stage = Some(Stage::Configuration);
                status.reason = Some(Reason::PushDisabled);
                return Ok(None);
            }
            // The status a child that cannot sign in reports below: the source stays here, and
            // the next publication tries again.
            if let Err(refusal) = destination.require_sign_in() {
                tracing_note(&format!("{refusal:#}"));
                signed_out = Some(format!("{refusal:#}"));
                status.readiness = Readiness::SetupRequired;
                status.stage = Some(Stage::Consent);
                status.reason = Some(Reason::ConsentRequired);
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
                native_session_id,
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
            direct(&mut push, &request, result.path());
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
                // Recorded like a refused push, so the turn's end and `agit status` say why the
                // turn stays on this computer.
                if let Some(refusal) = signed_out {
                    use crate::commands::upload_outcome::{Refusal, recorded_async};
                    let record = self
                        .agit_session
                        .as_ref()
                        .context("capture binding is missing")
                        .and_then(|lineage| self.upload_place(&lineage.slug(), branch));
                    let refusal = Refusal::child(Some(crate::ExitCode::Auth.as_i32()), refusal);
                    let _ = recorded_async(record, source.to_owned(), async {}, move |()| {
                        Some(refusal)
                    })
                    .await;
                }
                // A durable receipt ends this attempt; an unconfigured source still needs
                // idle recovery when its project destination becomes ready.
                self.finish_publication_outcome(if retained {
                    PublicationOutcome::Complete(None)
                } else {
                    PublicationOutcome::Retry
                });
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
        let record = self
            .agit_session
            .as_ref()
            .context("capture binding is missing")
            .and_then(|lineage| self.upload_place(&lineage.slug(), branch));
        let source = source.to_owned();
        let publish = async move {
            use crate::commands::upload_outcome::{Refusal, recorded_async};
            let mut status = status;
            // The publication goes through only with a result receipt the outbox accepts.
            let recorded = recorded_async(
                record,
                source,
                async {
                    let output = guarded_output(&mut state, lease, push).await;
                    let receipt = output
                        .as_ref()
                        .filter(|output| output.status.success())
                        .map(|_| result_receipt(&repo, &request, result.path()));
                    (output, receipt)
                },
                |(output, receipt)| match (output, receipt) {
                    (Some(_), Some(Ok(()))) => None,
                    (Some(output), _) => Some(Refusal::of(output)),
                    (None, _) => Some(Refusal::unexplained()),
                },
            )
            .await;
            match recorded {
                Ok((Some(_), Some(Ok(())))) => {
                    status.readiness = Readiness::Ready;
                    status.progress = Progress::AwaitingAck;
                    status.stage = Some(Stage::Receiver);
                    status.reason = None;
                }
                Ok((Some(_), Some(Err(error)))) => {
                    status.progress = Progress::Failed;
                    status.stage = Some(Stage::Publication);
                    status.reason = Some(Reason::PublicationFailed);
                    tracing_note(&format!("RC publication result is unavailable: {error:#}"));
                }
                // The Hub or the network did not answer; the next publication tries again.
                Ok((Some(output), None))
                    if output.status.code() == Some(crate::ExitCode::Network as i32) =>
                {
                    status.progress = Progress::LocalSaved;
                    status.stage = Some(Stage::Publication);
                    status.reason = Some(Reason::ReceiverUnavailable);
                    tracing_note(
                        "RC automatic publication could not reach the Hub; the source remains local and the next publication tries again",
                    );
                }
                Ok((Some(output), None)) if matches!(output.status.code(), Some(code) if code == crate::ExitCode::Interactive as i32 || code == crate::ExitCode::Auth as i32) =>
                {
                    status.readiness = Readiness::SetupRequired;
                    status.progress = Progress::LocalSaved;
                    status.stage = Some(Stage::Consent);
                    status.reason = Some(Reason::ConsentRequired);
                }
                // Unrecorded, it would fail unseen; the source stays and the next attempt tries.
                Err(unrecorded) => {
                    status.progress = Progress::LocalSaved;
                    status.stage = Some(Stage::Publication);
                    status.reason = Some(Reason::PublicationFailed);
                    tracing_note(&format!(
                        "RC automatic publication did not run; the source remains local: {unrecorded}"
                    ));
                }
                Ok(_) => {
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
            if status.progress == Progress::AwaitingAck {
                PublicationOutcome::Complete(None)
            } else {
                // Restored credentials must resume an unpublished source without another turn.
                PublicationOutcome::Retry
            }
        };
        if boundary.is_some() {
            self.publication = Some(Publication(tokio::spawn(publish)));
        } else {
            self.finish_publication_outcome(publish.await);
        }
    }
}

/// Makes `push`, an automatic publication child, carry out `request` and answer in `result`. The
/// child resolves every Hub it calls from [`crate::infra::config::HUB_URL_ENV`], which names the
/// request's destination Hub here, whichever Hub this process defaults to. Left on the default
/// Hub, a child would send a turn recorded for another Hub there. Every supervised publication
/// child is pointed through here.
pub(super) fn direct(
    push: &mut tokio::process::Command,
    request: &SupervisorPushRequest,
    result: &std::path::Path,
) {
    push.env(crate::infra::config::HUB_URL_ENV, &request.destination.hub)
        .env(
            crate::hub::identity::EXPECTED_AGENT_ID_ENV,
            &request.destination.agent_id,
        )
        .env(
            crate::domain::privacy_receipt::SUPERVISOR_RESULT_ENV,
            result,
        );
}

pub(super) fn result_receipt(
    repo: &Repo,
    request: &SupervisorPushRequest,
    path: &std::path::Path,
) -> crate::Result<()> {
    let (selected, receipt) = request.read_managed_result(repo, path)?;
    // The durable record must agree with the verified child reply.
    Entry::complete(repo, &selected, &receipt)?;
    if let Err(error) = crate::rc::archive_jobs::publication_confirmed(repo, &selected) {
        tracing_note(&format!(
            "published archive cleanup remains pending: {error:#}"
        ));
    }
    Ok(())
}
