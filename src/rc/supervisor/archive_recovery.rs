//! Recovery commits completed native prefixes without launching a runtime or acquiring its input.

use super::*;
use crate::{
    domain::{
        link,
        privacy_receipt::outbox::{Capture, Entry},
        store::Store,
    },
    rc::archive_jobs::Job,
};
use anyhow::{Context, ensure};
use std::io::Write;

impl Session {
    pub(super) fn retain_completed_archive(&self, turn_id: &str) -> crate::Result<()> {
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
        Job::capture(
            &self.info.session_id,
            &native,
            &self.info.runtime,
            self.info.native_source.clone(),
            &self.cwd,
            lineage,
            turn_id,
            tailer.path(),
            tailer.consumed(),
            self.archive_handoff.clone(),
        )?
        .record()
    }
}

pub(crate) struct Prepared {
    job: Job,
    request: SupervisorPushRequest,
    state: tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
}

/// A capture reservation protects native settlement, not the network publication wait.
pub(crate) async fn prepare(
    job: Job,
    mut state: tokio::sync::watch::Receiver<SettlementState>,
) -> crate::Result<Prepared> {
    let lease = settlement_lease(&state).context("archive recovery has no capture lease")?;
    ensure!(
        lease.local_owner,
        "archive recovery requires local capture authority"
    );
    let lineage = job.session()?;
    job.verify_prefix(&job.transcript)?;
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
    let landed = guarded_output(&mut state, lease, land)
        .await
        .context("archive landing was cancelled")?;
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
    verify_native(&job, &lineage)?;
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
    ensure!(
        committed.output.status.success() && job.covered_by(&repo, &committed.after)?,
        "archive commit does not cover the completed native turn"
    );
    verify_native(&job, &lineage)?;
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
    verify_native(&job, &lineage)?;
    crate::rc::archive_jobs::publication_confirmed(&repo, &request)
}

fn require_success(output: &std::process::Output, stage: &str) -> crate::Result<()> {
    ensure!(
        output.status.success(),
        "archive {stage} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(4096)
            .collect::<String>()
    );
    Ok(())
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

fn verify_native(job: &Job, lineage: &crate::rc::lineage::AgitSession) -> crate::Result<()> {
    if let Some(reference) = &job.native_source {
        let source = crate::rc::runtime_sources::Registry::open()?.resolve(&reference.source_id)?;
        ensure!(
            source.generation == reference.generation,
            "archive native source generation changed"
        );
    }
    let native = job
        .native_source
        .as_ref()
        .map(|source| source.session_ref(&job.native))
        .unwrap_or_else(|| job.native.clone());
    let store = Store::open()?.context("archive native claim store is missing")?;
    let claim = link::get_checked(&store, &job.runtime, &native)?
        .context("archive native claim is missing")?;
    let current = crate::rc::capture::resolve(
        &job.runtime,
        &native,
        &job.cwd,
        Some(lineage),
        job.capture.as_ref(),
    )?
    .context("archive native capture route is missing")?;
    ensure!(
        current.to_string() == job.lineage && current.agent_id() == job.repository_id,
        "archive capture route changed"
    );
    job.verify_prefix(&claim.resolve().context("archive transcript is missing")?)
}
