//! Recovery commits completed native prefixes without launching a runtime or acquiring its input.

use super::*;
use crate::{
    domain::privacy_receipt::outbox::{Capture, Entry},
    rc::{archive_jobs::Job, local_repository::Authority},
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
    /// agit interrupted it to stop while nobody chose to: a service manager's stop, a restart. The
    /// interrupt the transcript ends with is agit's, so the turn ends there like [`Self::Stopped`],
    /// unless a person stopped it first.
    StoppedByAgit,
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
                let stopped = (matches!(ending, TurnEnding::Stopped | TurnEnding::StoppedByAgit)
                    && matches!(self.driver, AnyDriver::ClaudeCode(_)))
                .then_some((generation.as_str(), turn_id));
                use crate::rc::native_claude::completion::{TurnEnd, turn_end};
                let agit_interrupt = ending == TurnEnding::StoppedByAgit;
                match turn_end(&native, tailer.path(), boundary, stopped, agit_interrupt)? {
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
        // A conversation the folder it runs in keeps back is not queued for saving.
        if !crate::rc::capture::consented(
            &self.cwd,
            &self.info.runtime,
            &native,
            self.info.project_id.is_some(),
        ) {
            return Ok(());
        }
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

/// Recovery cannot go further until state outside the job changes: another attempt meets the
/// same refusal, so the job waits for that state rather than backing off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Waiting {
    /// The session's settlement waits for the privacy check (`agit status` names its reason).
    #[error("archive settlement waits for the privacy check")]
    Privacy,
    /// The turn is committed locally; automatic publication is turned off for its repository or
    /// its destination (see [`publication_target`]).
    #[error("automatic archive publication is not enabled")]
    PushDisabled,
    /// The turn is committed locally; the conversation ran in a folder before its uploads were
    /// turned on, and stays here until the person saves it (see [`crate::rc::folder_cutoff`]).
    #[error("the conversation stays on this computer until it is saved")]
    HeldBack,
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
    // A job queued before its folder kept the conversation back is dropped unsaved.
    if !crate::rc::capture::consented(&job.cwd, &job.runtime, &job.native, false) {
        job.remove()?;
        return Err(KeptBack.into());
    }
    let lineage = job.session()?;
    let repo = crate::rc::capture::require(&lineage)?;
    let exe = crate::infra::own_image::own_image()?;
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
    if !job.covered_by(&repo, &committed.after, end)? {
        // Settlement that stops at the privacy check exits successfully and records the wait.
        if crate::domain::privacy::pending::load().contains_key(&job.claim_key()) {
            return Err(Waiting::Privacy.into());
        }
        anyhow::bail!("archive commit does not cover the completed native turn");
    }
    let destination = publication_target(&repo, &lineage, &job.runtime, &job.native)?;
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

/// Where the capture's turns publish, while automatic publication may carry them there: the
/// capture repository's own preference allows it, the person has not turned it off for the
/// destination, and the conversation (`native` of `runtime`) is not one its folder holds back.
/// Recovery and its recheck judge by this one rule; a recheck that read less would resume a job
/// the destination still refuses, and each resumed attempt lands and commits again.
pub(crate) fn publication_target(
    repo: &crate::domain::repo::Repo,
    lineage: &crate::rc::lineage::AgitSession,
    runtime: &str,
    native: &str,
) -> crate::Result<crate::rc::capture::PublicationDestination> {
    publication_target_by(repo, lineage, runtime, native, Authority::Process)
}

/// [`publication_target`], with a device-local capture repository verified against `authority`.
fn publication_target_by(
    repo: &crate::domain::repo::Repo,
    lineage: &crate::rc::lineage::AgitSession,
    runtime: &str,
    native: &str,
    authority: Authority,
) -> crate::Result<crate::rc::capture::PublicationDestination> {
    let destination = crate::rc::capture::destination_by(
        repo,
        lineage,
        &crate::infra::config::hub_url(),
        authority,
    )?
    .context("archive publication destination is not configured")?;
    if !crate::commands::auto_push::publication_allowed(repo, &destination.repository)? {
        return Err(Waiting::PushDisabled.into());
    }
    if crate::rc::folder_cutoff::holds_back(repo, runtime, native) {
        return Err(Waiting::HeldBack.into());
    }
    Ok(destination)
}

/// A session's queued turns that recovery leaves waiting because automatic publication is off
/// for them: where they publish, which setting keeps them here, and the narrowest command that
/// would upload them automatically, when one exists (see [`crate::commands::auto_push::refusal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicationWait {
    pub repository: String,
    /// The repository that records the conversation, named when its own setting keeps the turns.
    pub capture_repository: String,
    pub off: crate::commands::auto_push::Off,
    pub command: Option<String>,
}

/// The publication waits of the queued archive jobs, keyed by runtime and session id, judged by
/// [`publication_target`] as recovery judges them. A daemon parks such a job without retrying
/// or reporting it, so an inspection that judged by another rule would name a wait recovery
/// does not have, or miss one it has. Each job is judged as the daemon that queued it judges it,
/// by that daemon's saved identity, whichever namespace this process selected. Reads the queue
/// and the identities without writing either.
pub(crate) fn publication_waits()
-> crate::Result<std::collections::BTreeMap<(String, String), PublicationWait>> {
    let hub = crate::infra::config::hub_url();
    let mut waits = std::collections::BTreeMap::new();
    for (namespace, job) in crate::rc::archive_jobs::observe()? {
        let authority = Authority::Saved(namespace);
        let Ok(lineage) = job.session() else {
            continue;
        };
        let Ok(repo) = crate::rc::capture::require_by(&lineage, authority) else {
            continue;
        };
        let waiting = publication_target_by(&repo, &lineage, &job.runtime, &job.native, authority)
            .err()
            .is_some_and(|error| error.downcast_ref::<Waiting>() == Some(&Waiting::PushDisabled));
        if !waiting {
            continue;
        }
        let Ok(Some(destination)) =
            crate::rc::capture::destination_by(&repo, &lineage, &hub, authority)
        else {
            continue;
        };
        let (off, command) = crate::commands::auto_push::refusal(&repo, &destination.repository)?;
        waits.insert(
            (job.runtime.clone(), job.claim_key()),
            PublicationWait {
                repository: destination.repository,
                capture_repository: lineage.slug(),
                off,
                command,
            },
        );
    }
    Ok(waits)
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
    let exe = crate::infra::own_image::own_image()?;
    let saved = Entry::load(&repo, &request)?.context("archive publication intent is missing")?;
    if saved.publication.is_none() {
        let mut result = tempfile::NamedTempFile::new_in(repo.common_dir()?)?;
        result.write_all(&serde_json::to_vec(&request)?)?;
        let mut push = command(&exe, &job, &lineage, &[]);
        crate::commands::auto_push::automatic_push(push.as_std_mut(), &job.lineage);
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
        .env(
            crate::infra::own_image::PARENT_BUILD,
            crate::infra::own_image::BUILD,
        )
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        commands::auto_push::Off,
        domain::repo::Repo,
        rc::{lineage::AgitSession, local_repository::publication::Selection},
    };

    /// Recovery drops a queued turn whose folder keeps its conversation back, without landing or
    /// committing it. Settling an already queued turn without rechecking would save a conversation
    /// the person dropped.
    #[test]
    fn a_queued_turn_of_a_conversation_its_folder_left_out_is_dropped_unsaved() {
        if crate::rc::in_isolated_test(
            "rc::supervisor::archive_recovery::tests::a_queued_turn_of_a_conversation_its_folder_left_out_is_dropped_unsaved",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
        }
        crate::rc::select_local_authority();
        let folder = home.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        let lineage = AgitSession::new(
            "alice/photo",
            "6f35cd22-85ae-4ed5-a106-2d9069d6b346",
            "s/dropped",
        )
        .unwrap();
        let transcript = home.path().join("native.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        let job = Job::capture(
            "logical",
            "dropped-native",
            "claude-code",
            None,
            &folder,
            &lineage,
            "turn",
            &transcript,
            3,
            None,
        )
        .unwrap();
        job.record().unwrap();
        crate::commands::project::policy_for_test(
            &folder,
            true,
            true,
            &[("claude-code", "dropped-native")],
        );
        let (_lease, state) = tokio::sync::watch::channel(SettlementState {
            local_owner: true,
            epoch: 1,
            agent_identity_v1: true,
            session_start_idempotency_v1: false,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let Err(refused) = runtime.block_on(prepare(
            job,
            state,
            std::sync::Arc::new(Preemption::default()),
        )) else {
            panic!("a left-out conversation is not settled");
        };
        assert!(refused.downcast_ref::<KeptBack>().is_some(), "{refused:#}");
        assert!(crate::rc::archive_jobs::pending().unwrap().is_empty());
        assert!(
            !lineage.repo_dir().unwrap().exists(),
            "nothing was landed or committed"
        );
    }

    /// A device-local capture publishes on the person's behalf, so automatic publication turned
    /// off in its destination's checkout makes recovery wait even while the capture repository's
    /// own preference allows it. A gate that read only that preference would let recovery record
    /// a publication intent and push, which the push then refuses on every retry. `agit status`
    /// names the same wait for the queued turn, with a command for the destination's checkout: a
    /// listing that read the capture repository's preference would show nothing, and a command
    /// aimed at that repository would leave the turn waiting. Where only the user preference keeps
    /// the turn (the destination has no checkout here), no command is offered: turning that on
    /// would start automatic uploads of every repository without a setting of its own. Status runs
    /// in the plain namespace, not the desktop daemon's that queued the job: judged by the plain
    /// namespace's identity, the desktop repository fails its ownership check and the wait goes
    /// unreported, and loading that identity creates it, whether the plain namespace has none or
    /// one of its own.
    #[test]
    fn recovery_waits_while_its_destination_turned_automatic_publication_off() {
        if crate::rc::in_isolated_test(
            "rc::supervisor::archive_recovery::tests::recovery_waits_while_its_destination_turned_automatic_publication_off",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let hub = "http://127.0.0.1:9";
        // Process isolation keeps this home and Hub away from every other test.
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
            std::env::set_var("AGIT_HUB_URL", hub);
        }
        crate::rc::select_local_authority();
        let machine = crate::rc::identity::identity().unwrap();
        let lineage = AgitSession::new(
            "desktop-local/project",
            "00000000-0000-0000-0000-000000000001",
            "s/work",
        )
        .unwrap();
        let repo = Repo::open_or_init(&lineage.repo_dir().unwrap()).unwrap();
        repo.git(&["config", "agit.desktopIdentity", lineage.agent_id()])
            .unwrap();
        repo.git(&[
            "config",
            "agit.desktopAuthority",
            &format!("local:{}", machine.machine_fingerprint),
        ])
        .unwrap();
        let destination =
            crate::hub::identity::RemoteIdentity::new(hub, "00000000-0000-0000-0000-000000000002")
                .unwrap();
        Selection::prepare(&repo, Some("owner/project"), hub)
            .unwrap()
            .unwrap()
            .bind(&repo, &destination)
            .unwrap();
        repo.set_auto_push(Some(true)).unwrap();
        assert_eq!(
            publication_target(&repo, &lineage, "codex", "native")
                .unwrap()
                .repository,
            "owner/project"
        );

        let checkout_dir = crate::infra::config::repo_dir("owner", "project").unwrap();
        std::fs::create_dir_all(&checkout_dir).unwrap();
        let checkout = Repo::init(&checkout_dir).unwrap();
        checkout.set_auto_push(Some(false)).unwrap();
        let refused = publication_target(&repo, &lineage, "codex", "native").unwrap_err();
        assert_eq!(
            refused.downcast_ref::<Waiting>(),
            Some(&Waiting::PushDisabled)
        );

        let transcript = home.path().join("native.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        Job::capture(
            "logical",
            "native",
            "codex",
            None,
            home.path(),
            &lineage,
            "turn",
            &transcript,
            3,
            None,
        )
        .unwrap()
        .record()
        .unwrap();
        // From here on, a plain `agit status`: its own namespace, with no identity saved there.
        crate::rc::select_plain_authority();
        let plain = home.path().join("rc");
        assert!(!plain.exists());
        let wait = |off, command: Option<&str>| {
            std::collections::BTreeMap::from([(
                ("codex".into(), "native".into()),
                PublicationWait {
                    repository: "owner/project".into(),
                    capture_repository: "desktop-local/project".into(),
                    off,
                    command: command.map(str::to_owned),
                },
            )])
        };
        let destination_wait = wait(
            Off::Destination,
            Some("agit config --repo owner/project push.auto true"),
        );
        assert_eq!(publication_waits().unwrap(), destination_wait);
        assert!(!plain.exists());
        let other = crate::rc::identity::identity().unwrap();
        assert_ne!(other.machine_fingerprint, machine.machine_fingerprint);
        assert_eq!(publication_waits().unwrap(), destination_wait);

        checkout.set_auto_push(None).unwrap();
        assert!(publication_waits().unwrap().is_empty());

        std::fs::remove_dir_all(&checkout_dir).unwrap();
        crate::infra::config::set_global("push.auto", Some("false")).unwrap();
        assert_eq!(publication_waits().unwrap(), wait(Off::UserDefault, None));
    }
}

/// Recovery dropped a queued turn of a conversation its folder keeps back, without saving it.
#[derive(Debug)]
pub(crate) struct KeptBack;

impl std::fmt::Display for KeptBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the folder this conversation ran in keeps it from being saved automatically")
    }
}

impl std::error::Error for KeptBack {}
