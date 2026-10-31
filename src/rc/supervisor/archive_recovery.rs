//! Recovery commits completed native prefixes without launching a runtime or acquiring its input.

use super::*;
use crate::{
    domain::privacy_receipt::outbox::{Capture, Entry},
    rc::archive_jobs::Job,
};
use anyhow::{Context, ensure};
use std::io::Write;

/// How a turn ended, which decides what becomes of a tail no completion closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnEnding {
    /// The harness reported a normal completion.
    Completed,
    /// The person interrupted it. An open tail stays out of the archive until a later turn's cut
    /// covers it or the person publishes it: an interrupt is their quick stop, often right after
    /// sending something they regret.
    Interrupted,
    /// It stopped without anyone choosing to: a failed turn, or one still running when the session
    /// exits. An open tail ends where it stopped.
    Stopped,
}

impl Session {
    /// Queues the archive of a turn that ended, through the end its completed-only cut will reach.
    ///
    /// Only a harness this supervisor starts and reaps gets an end receipt for a stopped turn: an
    /// attached Claude outlives the supervisor and goes on to close its own transcript.
    pub(super) fn retain_completed_archive(
        &self,
        turn_id: &str,
        ending: TurnEnding,
    ) -> crate::Result<()> {
        let Some(lineage) = &self.agit_session else {
            return Ok(());
        };
        let native = self
            .driver
            .runtime_thread_id()
            .context("archive native identity is unavailable")?;
        let tailer = self
            .tailer
            .as_ref()
            .context("archive native transcript is unavailable")?;
        let boundary = tailer.consumed();
        #[cfg(unix)]
        let boundary = match &self.driver {
            AnyDriver::ClaudeNative(_) | AnyDriver::ClaudeCode(_) => {
                let generation = self.generation.to_string();
                let stopped = (ending == TurnEnding::Stopped
                    && matches!(self.driver, AnyDriver::ClaudeCode(_)))
                .then_some((generation.as_str(), turn_id));
                use crate::rc::native_claude::completion::{TurnEnd, turn_end};
                match turn_end(&native, tailer.path(), boundary, stopped)? {
                    TurnEnd::Closed(at) => at,
                    TurnEnd::Open(_) if ending == TurnEnding::Interrupted => return Ok(()),
                    TurnEnd::Open(at) => at,
                    TurnEnd::Held => return Ok(()),
                }
            }
            _ => boundary,
        };
        #[cfg(not(unix))]
        let _ = ending;
        ensure!(
            boundary <= tailer.consumed(),
            "native completion has not reached the transcript reader"
        );
        Job::capture(
            &self.info.session_id,
            &native,
            &self.info.runtime,
            self.info.native_source.clone(),
            &self.cwd,
            lineage,
            turn_id,
            tailer.path(),
            boundary,
            self.archive_handoff.clone(),
        )?
        .record()
    }
}

/// Landing validates a claim and creates a branch. A landing still running past this limit is
/// waiting on a lock nobody releases, and a later attempt costs less than holding the capture.
const LAND_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// A resume asks recovery of its conversation to yield the capture writer.
#[derive(Debug)]
pub(crate) struct Preemption(tokio::sync::watch::Sender<bool>);

impl Default for Preemption {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
}

impl Preemption {
    pub(crate) fn request(&self) {
        self.0.send_replace(true);
    }

    pub(crate) fn is_requested(&self) -> bool {
        *self.0.borrow()
    }

    async fn requested(&self) {
        let mut requested = self.0.subscribe();
        let _ = requested.wait_for(|requested| *requested).await;
    }
}

/// A recovery attempt stopped before it could fail on its own.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Interrupted {
    #[error("archive recovery yielded to a resume of its conversation")]
    Yielded,
    #[error("archive landing did not finish within its time limit")]
    LandTimedOut,
}

/// A recovery subprocess exited unsuccessfully; its exit code tells a refusal from a fault.
#[derive(Debug, thiserror::Error)]
#[error("archive {stage} failed ({status}): {detail}")]
pub(crate) struct StageFailure {
    pub stage: &'static str,
    pub code: Option<i32>,
    status: std::process::ExitStatus,
    detail: String,
}

pub(crate) struct Prepared {
    job: Job,
    request: SupervisorPushRequest,
    state: tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
}

/// A capture reservation protects native settlement, not the network publication wait.
///
/// A preemption stops landing at once and stops the attempt before its commit starts; a commit
/// that already started finishes, because cancelling it can leave repository locks behind.
pub(crate) async fn prepare(
    job: Job,
    mut state: tokio::sync::watch::Receiver<SettlementState>,
    preemption: std::sync::Arc<Preemption>,
) -> crate::Result<Prepared> {
    let lease = settlement_lease(&state).context("archive recovery has no capture lease")?;
    ensure!(
        lease.local_owner,
        "archive recovery requires local capture authority"
    );
    let lineage = job.session()?;
    let repo = crate::rc::capture::require(&lineage)?;
    let exe = std::env::current_exe()?;
    let mut land_args = crate::commands::rc::land_argv(
        &lineage.slug(),
        lineage.agent_id(),
        lineage.branch(),
        &job.runtime,
        &job.native,
        &job.cwd.to_string_lossy(),
    );
    if let Some(source) = &job.native_source {
        land_args.extend([
            "--source-id".into(),
            source.source_id.clone(),
            "--source-generation".into(),
            source.generation.to_string(),
        ]);
    }
    land_args.push("--local-owner".into());
    let mut land = command(&exe, &job, &lineage, &land_args);
    if let Some(handoff) = &job.archive_handoff {
        land.env(
            crate::commands::commit::archive::NATIVE_ENV,
            serde_json::to_string(&handoff.native)?,
        )
        .env(
            crate::commands::commit::archive::ROLE_ENV,
            serde_json::to_string(&handoff.role)?,
        );
    }
    let landed = tokio::select! {
        biased;
        _ = preemption.requested() => return Err(Interrupted::Yielded.into()),
        landed = tokio::time::timeout(LAND_LIMIT, guarded_output(&mut state, lease, land)) => landed
            .map_err(|_| Interrupted::LandTimedOut)?
            .context("archive landing was cancelled")?,
    };
    require_success(&landed, "landing")?;
    let handoffs: Vec<_> = String::from_utf8_lossy(&landed.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix(crate::commands::commit::archive::RC_PREFIX))
        .map(serde_json::from_str::<crate::commands::commit::archive::RcHandoff>)
        .collect::<Result<_, _>>()?;
    ensure!(
        handoffs.len() <= 1,
        "archive landing returned multiple classifications"
    );
    let handoff = handoffs.first();
    if let Some(expected) = &job.archive_handoff {
        ensure!(
            handoff == Some(expected),
            "archive classification changed during recovery"
        );
    }
    if let Some(handoff) = handoff {
        ensure!(
            handoff.native.session_id == job.native
                && handoff.native.runtime == job.runtime
                && handoff.role.slug == lineage.slug()
                && handoff.role.branch == lineage.branch(),
            "archive landing returned another native capture"
        );
    }
    verify_native(&job, &lineage, true).await?;
    if preemption.is_requested() {
        return Err(Interrupted::Yielded.into());
    }
    let result = tempfile::NamedTempFile::new_in(repo.common_dir()?)?;
    let prepared = tempfile::NamedTempFile::new_in(repo.common_dir()?)?;
    let mut commit = command(
        &exe,
        &job,
        &lineage,
        &["commit".into(), "--from-supervisor".into()],
    );
    commit.env(crate::commands::commit::SUPERVISOR_RESULT_ENV, result.path())
        .env(crate::commands::commit::SUPERVISOR_PREPARED_ENV, prepared.path())
        .env(crate::commands::commit::archive::NATIVE_ENV, serde_json::json!({
            "runtime":job.runtime,"session_id":job.native_source.as_ref().map(|source| source.session_ref(&job.native)).unwrap_or_else(|| job.native.clone())
        }).to_string());
    if let Some(handoff) = handoff {
        commit.env(
            crate::commands::commit::archive::ROLE_ENV,
            serde_json::to_string(&handoff.role)?,
        );
    }
    let watermark = settlement_io::BranchWatermark {
        repository: repo.root().to_owned(),
        reference: settlement_watermark_ref(lineage.branch()),
    };
    let committed = settlement_io::LocalCommitRequest {
        session_id: job.logical.clone(),
        settlement: state.clone(),
        lease,
        watermark,
        commit,
        result_file: result,
        prepared_file: prepared,
    }
    .run()
    .await
    .context("archive settlement did not return a verified result")?;
    require_success(&committed.output, "commit")?;
    strict_settlement_candidate(
        &committed.before,
        &committed.output,
        &committed.after,
        committed.reported.as_deref(),
        None,
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        committed.reported.as_deref() != Some(crate::commands::commit::archive::WITHHELD_RESULT),
        "archive exploration is withheld from publication"
    );
    let end = verify_native(&job, &lineage, false).await?;
    ensure!(
        committed.output.status.success() && job.covered_by(&repo, &committed.after, end)?,
        "archive commit does not cover the completed native turn"
    );
    let destination =
        crate::rc::capture::destination(&repo, &lineage, &crate::infra::config::hub_url())?
            .context("archive publication destination is not configured")?;
    ensure!(
        repo.auto_push_enabled()?,
        "automatic archive publication is not enabled"
    );
    let mut request = SupervisorPushRequest {
        version: 1,
        request_id: uuid::Uuid::now_v7().to_string(),
        repository: destination.repository,
        branch: lineage.branch().into(),
        source: committed.after,
        destination: destination.identity,
        notification_id: None,
    };
    Entry::begin(
        &repo,
        &mut request,
        Capture {
            session_id: job.logical.clone(),
            native_session_id: job.native.clone(),
            runtime: job.runtime.clone(),
            generation: 0,
            incarnation: None,
            through_seq: None,
        },
    )?;
    Ok(Prepared {
        job,
        request,
        state,
        lease,
    })
}

pub(crate) async fn publish(prepared: Prepared) -> crate::Result<()> {
    let Prepared {
        job,
        request,
        mut state,
        lease,
    } = prepared;
    ensure!(
        settlement_lease_is_current(&state, lease),
        "archive capture lease changed"
    );
    let lineage = job.session()?;
    let repo = crate::rc::capture::require(&lineage)?;
    let exe = std::env::current_exe()?;
    let saved = Entry::load(&repo, &request)?.context("archive publication intent is missing")?;
    if saved.publication.is_none() {
        let mut result = tempfile::NamedTempFile::new_in(repo.common_dir()?)?;
        result.write_all(&serde_json::to_vec(&request)?)?;
        let mut push = command(&exe, &job, &lineage, &["push".into(), job.lineage.clone()]);
        crate::commands::auto_push::configure(push.as_std_mut());
        push.env(
            crate::hub::identity::EXPECTED_AGENT_ID_ENV,
            &request.destination.agent_id,
        )
        .env(
            crate::domain::privacy_receipt::SUPERVISOR_RESULT_ENV,
            result.path(),
        );
        let output = guarded_output(&mut state, lease, push)
            .await
            .context("archive publication was cancelled")?;
        require_success(&output, "publication")?;
        local_publication::result_receipt(&repo, &request, result.path())?;
    }
    ensure!(
        settlement_lease_is_current(&state, lease),
        "archive capture lease changed"
    );
    verify_native(&job, &lineage, false).await?;
    tokio::task::spawn_blocking(move || {
        crate::rc::archive_jobs::publication_confirmed(&repo, &request)
    })
    .await?
}

fn require_success(output: &std::process::Output, stage: &'static str) -> crate::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(StageFailure {
        stage,
        code: output.status.code(),
        status: output.status,
        detail: String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(4096)
            .collect(),
    }
    .into())
}

fn command(
    exe: &std::path::Path,
    job: &Job,
    lineage: &crate::rc::lineage::AgitSession,
    args: &[String],
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(exe);
    command
        .args(args)
        .current_dir(&job.cwd)
        .env("AGIT_SESSION", &job.lineage)
        .env("AGIT_LOCAL_AGENT_ID", lineage.agent_id())
        .env_remove(crate::hub::identity::EXPECTED_AGENT_ID_ENV)
        .env_remove(crate::rc::harness::SUPERVISED_HOOK_ENV)
        .env_remove(crate::commands::commit::archive::NATIVE_ENV)
        .env_remove(crate::commands::commit::archive::ROLE_ENV);
    if let Some(kind) = &job.capture {
        command.env(
            crate::rc::capture::CAPTURE_ENV,
            serde_json::to_string(kind).expect("capture kind serializes"),
        );
    }
    command
}

/// The job's route is current and its carrier is part of the conversation's lineage; with
/// `prefix`, the carrier also still holds the completed prefix. Returns the job's boundary in
/// capture-stream coordinates.
async fn verify_native(
    job: &Job,
    lineage: &crate::rc::lineage::AgitSession,
    prefix: bool,
) -> crate::Result<u64> {
    let job = job.clone();
    let lineage = lineage.clone();
    tokio::task::spawn_blocking(move || {
        // Resolution refuses a removed or replaced store; a job queued under an older
        // generation of the same store still names the same transcript.
        if let Some(reference) = &job.native_source {
            crate::rc::runtime_sources::Registry::open()?.resolve(&reference.source_id)?;
        }
        let current = crate::rc::capture::resolve(
            &job.runtime,
            &job.claim_key(),
            &job.cwd,
            Some(&lineage),
            job.capture.as_ref(),
        )?
        .context("archive native capture route is missing")?;
        ensure!(
            current.to_string() == job.lineage && current.agent_id() == job.repository_id,
            "archive capture route changed"
        );
        if prefix {
            job.verify_prefix()?;
        }
        job.stream_end(&job.claim()?)
    })
    .await?
}
