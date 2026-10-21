//! Recovery reserves a capture writer only until its immutable publication intent is durable.

use super::*;
use crate::rc::archive_jobs::Job;
use crate::rc::supervisor::archive_recovery::{Interrupted, Preemption, StageFailure};
use std::time::Duration;
use tokio::time::Instant;

const CYCLE: Duration = Duration::from_secs(5);
const BATCH: usize = 16;

pub(super) struct Worker(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Worker {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// A capture reserved for recovery, and the way a resume of its conversation asks it to yield.
pub(super) struct Recovering {
    job: Job,
    preemption: Arc<Preemption>,
}

impl std::ops::Deref for Recovering {
    type Target = Job;

    fn deref(&self) -> &Job {
        &self.job
    }
}

// Recovery keeps its admission until completion or shutdown. A wall-clock cutoff can kill Git
// while it owns the index, leaving the durable retry unable to acquire repository locks.
async fn until_stopped<T>(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => None,
        result = work => Some(result),
    }
}

/// Observation and recovery run as separate tasks, so neither can delay the other.
pub(super) fn start(
    daemon: Arc<Mutex<Daemon>>,
    admission: crate::rc::admission::Admission,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Worker {
    Worker(vec![
        tokio::spawn(observe(daemon.clone(), admission.clone(), stop.clone())),
        tokio::spawn(recover(daemon, admission, stop)),
    ])
}

async fn observe(
    daemon: Arc<Mutex<Daemon>>,
    admission: crate::rc::admission::Admission,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut collector = super::observed_capture::Collector::default();
    let mut interval = tokio::time::interval(CYCLE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => break,
            _ = interval.tick() => {}
        }
        collector.reconcile(&daemon, &admission, &mut stop).await;
    }
}

async fn recover(
    daemon: Arc<Mutex<Daemon>>,
    admission: crate::rc::admission::Admission,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut cursor = 0usize;
    let mut schedule = Schedule::default();
    let mut unreadable = None;
    let mut interval = tokio::time::interval(CYCLE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => break,
            _ = interval.tick() => {}
        }
        let scanned = tokio::task::spawn_blocking(|| {
            crate::rc::archive_jobs::pending().map(|jobs| {
                jobs.into_iter()
                    .map(|job| {
                        let claim = job.claim_digest();
                        (job, claim)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .await;
        let jobs = match scanned {
            Ok(Ok(jobs)) => {
                unreadable = None;
                jobs
            }
            result => {
                // An unreadable queue stays unreadable until someone repairs it.
                let error = format!("{result:?}");
                if unreadable.as_ref() != Some(&error) {
                    eprintln!("agitd: pending archives could not be read: {error}");
                    unreadable = Some(error);
                }
                continue;
            }
        };
        schedule.retain(&jobs);
        let now = Instant::now();
        let mut jobs: Vec<_> = jobs
            .into_iter()
            .filter(|(job, claim)| schedule.due(job, claim.as_deref(), now))
            .map(|(job, _)| job)
            .collect();
        if !jobs.is_empty() {
            let offset = cursor % jobs.len();
            jobs.rotate_left(offset);
            cursor = cursor.wrapping_add(BATCH);
        }
        for job in recovery_batch(jobs) {
            let Some(_work) = admission.enter() else {
                return;
            };
            let Some(outcome) = attempt(&daemon, &job, &mut stop).await else {
                return;
            };
            let claim = if matches!(outcome, Outcome::Failed(_)) {
                let job = job.clone();
                tokio::task::spawn_blocking(move || job.claim_digest())
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            schedule.record(&job, outcome, claim, Instant::now());
        }
        interval.reset();
    }
}

fn recovery_batch(jobs: Vec<Job>) -> Vec<Job> {
    let mut captures: Vec<Job> = Vec::new();
    for job in jobs {
        if captures.iter().any(|capture| capture.same_capture(&job)) {
            continue;
        }
        captures.push(job);
        if captures.len() == BATCH {
            break;
        }
    }
    captures
}

enum Outcome {
    Published,
    /// The conversation is live, opening or held by another recovery.
    Unreserved,
    Failed(anyhow::Error),
}

/// One recovery attempt. `None` means the daemon is stopping.
async fn attempt(
    daemon: &Arc<Mutex<Daemon>>,
    job: &Job,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Option<Outcome> {
    // Placing the job in its lineage reads carrier headers only, so a job that can never settle
    // is classified without reserving its conversation.
    let placed = job.clone();
    let placed = run(stop, async move {
        tokio::task::spawn_blocking(move || placed.stream_end(&placed.claim()?)).await?
    })
    .await?;
    if let Err(error) = placed {
        return Some(Outcome::Failed(error));
    }
    let (authority, preemption, reservation) = {
        let mut state = daemon.lock().await;
        let Some(preemption) = state.reserve_archive(job) else {
            return Some(Outcome::Unreserved);
        };
        let reservation = Reservation {
            daemon: Some(daemon.clone()),
            logical: job.logical.clone(),
        };
        (state.settlement.subscribe(), preemption, reservation)
    };
    let prepared = run(
        stop,
        crate::rc::supervisor::archive_recovery::prepare(job.clone(), authority, preemption),
    )
    .await;
    reservation.release().await;
    let prepared = match prepared? {
        Ok(prepared) => prepared,
        Err(error) => return Some(Outcome::Failed(error)),
    };
    match run(
        stop,
        crate::rc::supervisor::archive_recovery::publish(prepared),
    )
    .await?
    {
        Ok(()) => Some(Outcome::Published),
        Err(error) => Some(Outcome::Failed(error)),
    }
}

/// Runs one stage on its own task: a panic fails the attempt instead of ending recovery, and
/// shutdown aborts the stage together with its subprocesses.
async fn run<T: Send + 'static>(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    stage: impl std::future::Future<Output = crate::Result<T>> + Send + 'static,
) -> Option<crate::Result<T>> {
    let task = tokio::spawn(stage);
    let abort = task.abort_handle();
    match until_stopped(stop, task).await {
        None => {
            abort.abort();
            None
        }
        Some(Ok(result)) => Some(result),
        Some(Err(error)) => Some(Err(anyhow::anyhow!(
            "archive recovery stage did not complete: {error}"
        ))),
    }
}

/// Releases a recovery reservation however its attempt ends, including a panic or an abort.
/// A reservation that outlives its attempt refuses every resume of the conversation.
struct Reservation {
    daemon: Option<Arc<Mutex<Daemon>>>,
    logical: String,
}

impl Reservation {
    async fn release(mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.lock().await.archive_recovering.remove(&self.logical);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let Some(daemon) = self.daemon.take() else {
            return;
        };
        let logical = std::mem::take(&mut self.logical);
        if let Ok(mut state) = daemon.try_lock() {
            state.archive_recovering.remove(&logical);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                daemon.lock().await.archive_recovering.remove(&logical);
            });
        }
    }
}

/// How a failed attempt decides the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// A resume took the conversation; recovery waits for it to go live.
    Yielded,
    Transient,
    /// Settlement succeeded locally; delivery failed.
    Publication,
    /// The job's evidence cannot settle as recorded. Retrying repeats the same refusal until
    /// its claim changes.
    Permanent,
}

impl Class {
    fn of(error: &anyhow::Error) -> Self {
        use crate::adapter::native_snapshot::Unavailable;
        for cause in error.chain() {
            if cause.is::<crate::rc::archive_jobs::Unrecoverable>()
                || cause.downcast_ref::<Unavailable>() == Some(&Unavailable::BudgetExceeded)
            {
                return Self::Permanent;
            }
            if matches!(
                cause.downcast_ref::<Interrupted>(),
                Some(Interrupted::Yielded)
            ) {
                return Self::Yielded;
            }
            if let Some(failure) = cause.downcast_ref::<StageFailure>() {
                return match failure.stage {
                    // Settlement refuses content that does not continue the branch, and the
                    // secret gate refuses content it blocks; neither changes on retry.
                    "commit" if failure.code == Some(crate::ExitCode::Policy.as_i32()) => {
                        Self::Permanent
                    }
                    "publication" => Self::Publication,
                    _ => Self::Transient,
                };
            }
        }
        Self::Transient
    }

    /// The first delay and the longest; each further failure doubles the delay.
    fn delays(self) -> (Duration, Duration) {
        match self {
            Self::Yielded => (Duration::from_secs(30), Duration::from_secs(30)),
            Self::Transient => (Duration::from_secs(30), Duration::from_secs(30 * 60)),
            Self::Publication | Self::Permanent => {
                (Duration::from_secs(60), Duration::from_secs(30 * 60))
            }
        }
    }
}

struct Retry {
    failures: u32,
    at: Instant,
}

/// When each queued completion may be attempted again.
#[derive(Default)]
struct Schedule {
    /// One capture's jobs share an attempt, so they share its delay.
    retry: HashMap<String, Retry>,
    /// Jobs that cannot settle, with the claim digest they failed under.
    parked: HashMap<String, Option<String>>,
}

impl Schedule {
    fn retain(&mut self, jobs: &[(Job, Option<String>)]) {
        let ids: std::collections::HashSet<_> =
            jobs.iter().map(|(job, _)| job.filename()).collect();
        let captures: std::collections::HashSet<_> =
            jobs.iter().map(|(job, _)| job.capture_key()).collect();
        self.parked.retain(|id, _| ids.contains(id));
        self.retry.retain(|capture, _| captures.contains(capture));
    }

    fn due(&mut self, job: &Job, claim: Option<&str>, now: Instant) -> bool {
        let id = job.filename();
        if let Some(parked) = self.parked.get(&id) {
            if parked.as_deref() == claim {
                return false;
            }
            self.parked.remove(&id);
        }
        self.retry
            .get(&job.capture_key())
            .is_none_or(|retry| retry.at <= now)
    }

    fn record(&mut self, job: &Job, outcome: Outcome, claim: Option<String>, now: Instant) {
        let error = match outcome {
            Outcome::Published => {
                self.retry.remove(&job.capture_key());
                self.parked.remove(&job.filename());
                return;
            }
            Outcome::Unreserved => return,
            Outcome::Failed(error) => error,
        };
        let class = Class::of(&error);
        if class == Class::Permanent {
            eprintln!(
                "agitd: completed turn archive cannot settle and waits for its claim to change ({}, turn {}): {error:#}",
                job.logical, job.turn_id
            );
            self.parked.insert(job.filename(), claim);
            return;
        }
        let retry = self.retry.entry(job.capture_key()).or_insert(Retry {
            failures: 0,
            at: now,
        });
        if class != Class::Yielded {
            retry.failures = retry.failures.saturating_add(1);
        }
        let (first, longest) = class.delays();
        let delay = first
            .saturating_mul(1 << retry.failures.saturating_sub(1).min(16))
            .min(longest);
        retry.at = now + delay;
        if class != Class::Yielded {
            eprintln!(
                "agitd: completed turn archive remains pending ({}); next attempt in {}s: {error:#}",
                job.logical,
                delay.as_secs()
            );
        }
    }
}

impl Daemon {
    fn reserve_archive(&mut self, job: &Job) -> Option<Arc<Preemption>> {
        if !self.opts.local_owner
            || !self.settlement.borrow().local_owner
            || self.sessions.contains_key(&job.logical)
            || self.opening_sessions.contains_key(&job.logical)
            || self.archive_recovering.contains_key(&job.logical)
        {
            return None;
        }
        let matches_native = |runtime: &str,
                              source: Option<&crate::protocol::NativeSourceRef>,
                              native: Option<&str>| {
            runtime == job.runtime
                && crate::protocol::NativeSourceRef::same_store(source, job.native_source.as_ref())
                && native == Some(job.native.as_str())
        };
        if self.sessions.values().any(|live| {
            matches_native(
                &live.info.runtime,
                live.info.native_source.as_ref(),
                live.runtime_thread_id.as_deref(),
            )
        }) || self.opening_sessions.values().any(|opening| {
            matches_native(
                &opening.runtime,
                opening.native_source.as_ref(),
                opening.native_id.as_deref(),
            )
        }) || self.archive_recovering.values().any(|pending| {
            matches_native(
                &pending.runtime,
                pending.native_source.as_ref(),
                Some(&pending.native),
            )
        }) {
            return None;
        }
        let row = self.roster.get(&job.logical)?;
        if row.workspace_id != crate::rc::endpoint::WORKSPACE
            || row.cwd != job.cwd.to_string_lossy()
            || row.runtime != job.runtime
            || !crate::protocol::NativeSourceRef::same_store(
                row.native_source.as_ref(),
                job.native_source.as_ref(),
            )
            || (row.thread_id != job.native && !row.prior_threads.contains(&job.native))
            || row.agit_session.as_deref() != Some(job.lineage.as_str())
            || row.expected_agent_id.as_deref() != Some(job.repository_id.as_str())
            || self.roster.captures.get(&job.logical) != job.capture.as_ref()
        {
            return None;
        }
        let preemption = Arc::new(Preemption::default());
        self.archive_recovering.insert(
            job.logical.clone(),
            Recovering {
                job: job.clone(),
                preemption: preemption.clone(),
            },
        );
        Some(preemption)
    }

    /// Asks recovery holding this conversation to yield to a resume.
    ///
    /// A launch is still refused as retryable while recovery holds the capture, but recovery
    /// stops landing at once and lets go before committing, so the retry finds it free.
    pub(super) fn preempt_archive(
        &self,
        logical: &str,
        runtime: &str,
        source: Option<&crate::protocol::NativeSourceRef>,
        native: Option<&str>,
    ) {
        for (held, recovering) in &self.archive_recovering {
            let same_native = recovering.runtime == runtime
                && crate::protocol::NativeSourceRef::same_store(
                    recovering.native_source.as_ref(),
                    source,
                )
                && native == Some(recovering.native.as_str());
            if held == logical || same_native {
                recovering.preemption.request();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpublished_turns_share_a_recovery_attempt_without_excluding_other_captures() {
        let root = tempfile::tempdir().unwrap();
        let first = crate::rc::archive_jobs::tests::fixture(root.path());
        let mut next_turn = first.clone();
        next_turn.turn_id = "next-turn".into();
        let mut other = first.clone();
        other.native = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            recovery_batch(vec![first.clone(), next_turn, other.clone()]),
            vec![first, other]
        );
    }

    /// Long local transactions keep restart admission until completion; shutdown still cancels.
    #[tokio::test(start_paused = true)]
    async fn archive_completion_outlives_request_budgets_but_observes_shutdown() {
        let admission = crate::rc::admission::Admission::default();
        let work = admission.enter().unwrap();
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(async move {
            let _work = work;
            until_stopped(&mut stopped, async {
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                "committed"
            })
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        assert!(!running.is_finished());
        assert!(admission.freeze().is_err());
        assert_eq!(running.await.unwrap(), Some("committed"));
        assert!(admission.freeze().is_ok());

        let mut stopped = stop.subscribe();
        let running =
            tokio::spawn(
                async move { until_stopped(&mut stopped, std::future::pending::<()>()).await },
            );
        stop.send(true).unwrap();
        assert_eq!(running.await.unwrap(), None);
    }

    /// Recovery and a native attachment cannot hold the same capture writer concurrently.
    #[tokio::test]
    async fn recovery_reservations_exclude_live_and_new_native_attachments() {
        let root = tempfile::tempdir().unwrap();
        let job = crate::rc::archive_jobs::tests::fixture(root.path());
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
        let mut state = daemon.lock().await;
        state.opts.local_owner = true;
        state
            .settlement
            .send_modify(|authority| authority.local_owner = true);
        state.roster.sessions.insert(
            job.logical.clone(),
            roster::Entry {
                native_source: job.native_source.clone(),
                runtime: job.runtime.clone(),
                thread_id: job.native.clone(),
                cwd: job.cwd.to_string_lossy().into_owned(),
                workspace_id: crate::rc::endpoint::WORKSPACE.into(),
                project_id: None,
                agit_session: Some(job.lineage.clone()),
                expected_agent_id: Some(job.repository_id.clone()),
                permission_mode: None,
                guard_attempts: Default::default(),
                prior_threads: vec![],
                ever_dangerous: false,
            },
        );
        assert!(state.reserve_archive(&job).is_some());
        assert!(state.reserve_archive(&job).is_none());
        let (sender, _receiver) = mpsc::channel(1);
        let mut alias = super::super::tests::rpc_test_live(
            "alias",
            1,
            sender,
            crate::protocol::PermissionMode::Default,
        );
        alias.info.runtime = job.runtime.clone();
        alias.info.native_source = job.native_source.clone();
        alias.runtime_thread_id = Some(job.native.clone());
        let spec = crate::rc::harness::LaunchSpec {
            cwd: job.cwd.clone(),
            resume_from: Some(job.native.clone()),
            agit_session: Some(job.session().unwrap()),
            model: None,
            dangerous: false,
            permission_mode: None,
        };
        let refused = state.require_launch_slot(&alias.info, &spec).unwrap_err();
        assert_eq!(refused.error.data.as_ref().unwrap()["retryable"], true);
        assert_eq!(refused.error.data.as_ref().unwrap()["outcome"], "not_sent");
        assert!(!refused.reached_launch);
        state.archive_recovering.clear();
        assert!(state.require_launch_slot(&alias.info, &spec).is_ok());
        state.sessions.insert("alias".into(), alias);
        assert!(state.reserve_archive(&job).is_none());
        state.sessions.clear();
        assert!(state.reserve_archive(&job).is_some());
    }

    fn reserved_daemon(job: &Job) -> Arc<Mutex<Daemon>> {
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
        let mut state = daemon.try_lock().unwrap();
        state.opts.local_owner = true;
        state
            .settlement
            .send_modify(|authority| authority.local_owner = true);
        state.roster.sessions.insert(
            job.logical.clone(),
            roster::Entry {
                native_source: job.native_source.clone(),
                runtime: job.runtime.clone(),
                thread_id: job.native.clone(),
                cwd: job.cwd.to_string_lossy().into_owned(),
                workspace_id: crate::rc::endpoint::WORKSPACE.into(),
                project_id: None,
                agit_session: Some(job.lineage.clone()),
                expected_agent_id: Some(job.repository_id.clone()),
                permission_mode: None,
                guard_attempts: Default::default(),
                prior_threads: vec![],
                ever_dangerous: false,
            },
        );
        drop(state);
        daemon
    }

    /// A resume of the conversation reaches the recovery holding it, and the reservation is
    /// released even when the attempt panics; a leaked reservation would refuse every later
    /// resume of the conversation.
    #[tokio::test]
    async fn resumes_preempt_recovery_and_a_panicking_attempt_releases_its_capture() {
        let root = tempfile::tempdir().unwrap();
        let job = crate::rc::archive_jobs::tests::fixture(root.path());
        let daemon = reserved_daemon(&job);
        let preemption = daemon.lock().await.reserve_archive(&job).unwrap();
        daemon.lock().await.preempt_archive(
            "another-conversation",
            "codex",
            None,
            Some("another-native"),
        );
        assert!(!preemption.is_requested());
        daemon.lock().await.preempt_archive(
            "another-logical",
            &job.runtime,
            job.native_source.as_ref(),
            Some(&job.native),
        );
        assert!(preemption.is_requested());
        let reservation = Reservation {
            daemon: Some(daemon.clone()),
            logical: job.logical.clone(),
        };
        let attempt = tokio::spawn(async move {
            let _reservation = reservation;
            panic!("recovery attempt failed unexpectedly");
        });
        assert!(attempt.await.unwrap_err().is_panic());
        tokio::task::yield_now().await;
        assert!(daemon.lock().await.archive_recovering.is_empty());
    }

    /// A refusal that a retry repeats is attempted once per claim, and other failures back off
    /// per capture rather than per job, so a capture's queued turns cannot take turns retrying
    /// every cycle.
    #[test]
    fn permanent_failures_wait_for_their_claim_and_others_back_off_per_capture() {
        let root = tempfile::tempdir().unwrap();
        let job = crate::rc::archive_jobs::tests::fixture(root.path());
        let mut sibling = job.clone();
        sibling.turn_id = "next-turn".into();
        let mut schedule = Schedule::default();
        let start = Instant::now();
        let failed = |error: anyhow::Error| Outcome::Failed(error);

        schedule.record(
            &job,
            failed(anyhow::anyhow!("lock unavailable")),
            None,
            start,
        );
        assert!(!schedule.due(&sibling, None, start));
        assert!(schedule.due(&sibling, None, start + Duration::from_secs(30)));
        schedule.record(
            &job,
            failed(anyhow::anyhow!("lock unavailable")),
            None,
            start,
        );
        assert!(!schedule.due(&job, None, start + Duration::from_secs(30)));
        assert!(schedule.due(&job, None, start + Duration::from_secs(60)));
        for _ in 0..32 {
            schedule.record(&job, failed(anyhow::anyhow!("still failing")), None, start);
        }
        assert!(schedule.due(&job, None, start + Duration::from_secs(30 * 60)));
        schedule.record(&job, Outcome::Published, None, start);
        assert!(schedule.due(&job, None, start));

        let refusal = crate::rc::archive_jobs::Unrecoverable("archive transcript carrier changed");
        schedule.record(
            &job,
            failed(anyhow::Error::from(refusal).context("archive recovery")),
            Some("claim-one".into()),
            start,
        );
        let much_later = start + Duration::from_secs(24 * 60 * 60);
        assert!(!schedule.due(&job, Some("claim-one"), much_later));
        assert!(schedule.due(&sibling, Some("claim-one"), start));
        assert!(schedule.due(&job, Some("claim-two"), start));

        assert_eq!(
            Class::of(&crate::adapter::native_snapshot::Unavailable::BudgetExceeded.into()),
            Class::Permanent
        );
        assert_eq!(Class::of(&Interrupted::Yielded.into()), Class::Yielded);
    }
}
