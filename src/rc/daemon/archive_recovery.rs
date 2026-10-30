//! Recovery reserves a capture writer only until its immutable publication intent is durable.

use super::*;
use crate::domain::privacy::pending;
use crate::rc::archive_jobs::Job;
use crate::rc::supervisor::archive_recovery::{Interrupted, Preemption, StageFailure, Waiting};
use std::time::Duration;
use tokio::time::Instant;

const CYCLE: Duration = Duration::from_secs(5);
const BATCH: usize = 16;
/// A privacy wait can end by itself (a deadline, a worker back-off) without anything recording
/// that it did, so a job waiting on one is attempted again after this long.
const PRIVACY_RECHECK: Duration = Duration::from_secs(30 * 60);
/// Reading the automatic-publication setting starts Git, so a job waiting for it to be turned
/// on reads it this often rather than every cycle.
const PUSH_RECHECK: Duration = Duration::from_secs(60);

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
        let push = schedule.awaiting_push(Instant::now());
        let scanned = tokio::task::spawn_blocking(move || {
            let privacy = pending::load();
            crate::rc::archive_jobs::pending().map(|jobs| {
                jobs.into_iter()
                    .map(|job| {
                        let mut evidence = Evidence::read(&job, &privacy);
                        if push.contains(&job.filename()) {
                            evidence.push = Some(auto_push_enabled(&job));
                        }
                        (job, evidence)
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
            .filter(|(job, evidence)| schedule.due(job, evidence, now))
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
            let evidence = if matches!(outcome, Outcome::Failed(_)) {
                let job = job.clone();
                tokio::task::spawn_blocking(move || Evidence::read(&job, &pending::load()))
                    .await
                    .unwrap_or_default()
            } else {
                Evidence::default()
            };
            schedule.record(&job, outcome, evidence, Instant::now());
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
    /// Recovery waits for state outside the job; see [`Waiting`].
    Waits(Waiting),
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
            if let Some(waiting) = cause.downcast_ref::<Waiting>() {
                return Self::Waits(*waiting);
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
            Self::Publication | Self::Permanent | Self::Waits(_) => {
                (Duration::from_secs(60), Duration::from_secs(30 * 60))
            }
        }
    }
}

struct Retry {
    failures: u32,
    at: Instant,
}

/// What a job's next attempt depends on besides time, read once per cycle.
#[derive(Clone, Debug, Default)]
struct Evidence {
    /// The digest of the claim file the job settles through.
    claim: Option<String>,
    /// The session's privacy wait, while its settlement waits for the privacy check.
    privacy: Option<pending::Waiting>,
    /// Whether automatic publication is on; read only for jobs that wait for it.
    push: Option<bool>,
}

impl Evidence {
    fn read(job: &Job, privacy: &std::collections::BTreeMap<String, pending::Waiting>) -> Self {
        Self {
            claim: job.claim_digest(),
            privacy: privacy.get(&job.claim_key()).cloned(),
            push: None,
        }
    }
}

fn auto_push_enabled(job: &Job) -> bool {
    job.session()
        .and_then(|lineage| crate::rc::capture::require(&lineage)?.auto_push_enabled())
        .unwrap_or(false)
}

/// A job that is not attempted until its claim changes or what it waits for ends.
struct Park {
    claim: Option<String>,
    until: Until,
}

enum Until {
    /// Nothing but the claim.
    Claim,
    /// The session's privacy wait changes or ends, or `recheck` passes. A refused record has no
    /// `recheck`: settling again meets the same record.
    Privacy {
        wait: Option<pending::Waiting>,
        recheck: Option<Instant>,
    },
    /// Automatic publication is turned on; `checked` is when the setting was last read.
    PushEnabled { checked: Instant },
}

impl Park {
    fn privacy(claim: Option<String>, wait: Option<pending::Waiting>, now: Instant) -> Self {
        let recheck = wait
            .as_ref()
            .is_none_or(|wait| wait.refused_bytes.is_none())
            .then(|| now + PRIVACY_RECHECK);
        Self {
            claim,
            until: Until::Privacy { wait, recheck },
        }
    }

    fn holds(&mut self, evidence: &Evidence, now: Instant) -> bool {
        if self.claim != evidence.claim {
            return false;
        }
        match &mut self.until {
            Until::Claim => true,
            Until::Privacy { wait, recheck } => {
                *wait == evidence.privacy && recheck.is_none_or(|at| now < at)
            }
            Until::PushEnabled { checked } => match evidence.push {
                Some(true) => false,
                Some(false) => {
                    *checked = now;
                    true
                }
                None => true,
            },
        }
    }
}

/// When each queued completion may be attempted again.
#[derive(Default)]
struct Schedule {
    /// One capture's jobs share an attempt, so they share its delay.
    retry: HashMap<String, Retry>,
    /// Jobs that wait for their claim or for state outside the job.
    parked: HashMap<String, Park>,
    /// Jobs this daemon has considered; the schedule starts empty after a restart.
    seen: std::collections::HashSet<String>,
}

impl Schedule {
    fn retain(&mut self, jobs: &[(Job, Evidence)]) {
        let ids: std::collections::HashSet<_> =
            jobs.iter().map(|(job, _)| job.filename()).collect();
        let captures: std::collections::HashSet<_> =
            jobs.iter().map(|(job, _)| job.capture_key()).collect();
        self.parked.retain(|id, _| ids.contains(id));
        self.seen.retain(|id| ids.contains(id));
        self.retry.retain(|capture, _| captures.contains(capture));
    }

    /// Jobs whose automatic-publication setting is due to be read again.
    fn awaiting_push(&self, now: Instant) -> std::collections::HashSet<String> {
        self.parked
            .iter()
            .filter(|(_, park)| {
                matches!(park.until, Until::PushEnabled { checked } if checked + PUSH_RECHECK <= now)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn due(&mut self, job: &Job, evidence: &Evidence, now: Instant) -> bool {
        let id = job.filename();
        let unseen = self.seen.insert(id.clone());
        if let Some(park) = self.parked.get_mut(&id) {
            if park.holds(evidence, now) {
                return false;
            }
            self.parked.remove(&id);
        } else if unseen && evidence.privacy.is_some() {
            // The privacy wait is durable and the schedule is not: after a restart, an attempt
            // at once would meet the same wait.
            self.parked.insert(
                id,
                Park::privacy(evidence.claim.clone(), evidence.privacy.clone(), now),
            );
            return false;
        }
        self.retry
            .get(&job.capture_key())
            .is_none_or(|retry| retry.at <= now)
    }

    fn record(&mut self, job: &Job, outcome: Outcome, evidence: Evidence, now: Instant) {
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
        let until = match class {
            Class::Permanent => {
                eprintln!(
                    "agitd: completed turn archive cannot settle and waits for its claim to change ({}, turn {}): {error:#}",
                    job.logical, job.turn_id
                );
                Some(Park {
                    claim: evidence.claim,
                    until: Until::Claim,
                })
            }
            Class::Waits(Waiting::Privacy) => {
                eprintln!(
                    "agitd: completed turn archive waits for the privacy check ({}, turn {}); `agit status` names the reason",
                    job.logical, job.turn_id
                );
                Some(Park::privacy(evidence.claim, evidence.privacy, now))
            }
            Class::Waits(Waiting::PushDisabled) => {
                eprintln!(
                    "agitd: completed turn archive is committed locally and waits for automatic publication to be turned on ({}, turn {})",
                    job.logical, job.turn_id
                );
                Some(Park {
                    claim: evidence.claim,
                    until: Until::PushEnabled { checked: now },
                })
            }
            _ => None,
        };
        if let Some(park) = until {
            self.parked.insert(job.filename(), park);
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

    fn claimed(claim: Option<&str>) -> Evidence {
        Evidence {
            claim: claim.map(Into::into),
            ..Evidence::default()
        }
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
            claimed(None),
            start,
        );
        assert!(!schedule.due(&sibling, &claimed(None), start));
        assert!(schedule.due(&sibling, &claimed(None), start + Duration::from_secs(30)));
        schedule.record(
            &job,
            failed(anyhow::anyhow!("lock unavailable")),
            claimed(None),
            start,
        );
        assert!(!schedule.due(&job, &claimed(None), start + Duration::from_secs(30)));
        assert!(schedule.due(&job, &claimed(None), start + Duration::from_secs(60)));
        for _ in 0..32 {
            schedule.record(
                &job,
                failed(anyhow::anyhow!("still failing")),
                claimed(None),
                start,
            );
        }
        assert!(schedule.due(&job, &claimed(None), start + Duration::from_secs(30 * 60)));
        schedule.record(&job, Outcome::Published, claimed(None), start);
        assert!(schedule.due(&job, &claimed(None), start));

        let refusal = crate::rc::archive_jobs::Unrecoverable("archive transcript carrier changed");
        schedule.record(
            &job,
            failed(anyhow::Error::from(refusal).context("archive recovery")),
            claimed(Some("claim-one")),
            start,
        );
        let much_later = start + Duration::from_secs(24 * 60 * 60);
        assert!(!schedule.due(&job, &claimed(Some("claim-one")), much_later));
        assert!(schedule.due(&sibling, &claimed(Some("claim-one")), start));
        assert!(schedule.due(&job, &claimed(Some("claim-two")), start));

        assert_eq!(
            Class::of(&crate::adapter::native_snapshot::Unavailable::BudgetExceeded.into()),
            Class::Permanent
        );
        assert_eq!(Class::of(&Interrupted::Yielded.into()), Class::Yielded);
    }

    /// A privacy wait parks its job instead of backing off: it is attempted again when the
    /// session's wait changes or ends, or after the recheck (never for a refused record), and a
    /// restart that finds the wait recorded does not attempt at once. A schedule that treated
    /// the wait as a transient failure would be due again within a minute.
    #[test]
    fn privacy_waits_park_until_the_wait_ends_and_across_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let job = crate::rc::archive_jobs::tests::fixture(root.path());
        let wait = pending::Waiting {
            reason: "deadline".into(),
            turns: 1,
            since: 0,
            refused_bytes: None,
        };
        let waiting = Evidence {
            privacy: Some(wait.clone()),
            ..claimed(Some("claim"))
        };
        let error =
            || Outcome::Failed(anyhow::Error::from(Waiting::Privacy).context("archive recovery"));
        let start = Instant::now();
        let mut schedule = Schedule::default();
        assert!(schedule.due(&job, &claimed(Some("claim")), start));
        schedule.record(&job, error(), waiting.clone(), start);
        assert!(!schedule.due(&job, &waiting, start + PRIVACY_RECHECK / 2));
        assert!(schedule.due(&job, &waiting, start + PRIVACY_RECHECK));
        schedule.record(&job, error(), waiting.clone(), start);
        assert!(schedule.due(&job, &claimed(Some("claim")), start));

        let refused = Evidence {
            privacy: Some(pending::Waiting {
                refused_bytes: Some(1),
                ..wait
            }),
            ..claimed(Some("claim"))
        };
        schedule.record(&job, error(), refused.clone(), start);
        let much_later = start + Duration::from_secs(24 * 60 * 60);
        assert!(!schedule.due(&job, &refused, much_later));
        assert!(schedule.due(&job, &waiting, much_later));

        let mut restarted = Schedule::default();
        assert!(!restarted.due(&job, &waiting, start));
        assert!(!restarted.due(&job, &waiting, start + PRIVACY_RECHECK / 2));
        assert!(restarted.due(&job, &claimed(Some("claim")), start));
    }

    /// A job committed locally while automatic publication is off is not landed and committed
    /// again: it waits for the setting, which is read at most once per recheck, or for its
    /// claim to change.
    #[test]
    fn disabled_publication_parks_until_the_setting_or_the_claim_changes() {
        let root = tempfile::tempdir().unwrap();
        let job = crate::rc::archive_jobs::tests::fixture(root.path());
        let start = Instant::now();
        let mut schedule = Schedule::default();
        schedule.record(
            &job,
            Outcome::Failed(Waiting::PushDisabled.into()),
            claimed(Some("claim")),
            start,
        );
        assert!(schedule.awaiting_push(start).is_empty());
        let much_later = start + Duration::from_secs(24 * 60 * 60);
        assert!(!schedule.due(&job, &claimed(Some("claim")), much_later));
        assert!(
            schedule
                .awaiting_push(start + PUSH_RECHECK)
                .contains(&job.filename())
        );
        let off = Evidence {
            push: Some(false),
            ..claimed(Some("claim"))
        };
        assert!(!schedule.due(&job, &off, start + PUSH_RECHECK));
        assert!(schedule.awaiting_push(start + PUSH_RECHECK).is_empty());
        let on = Evidence {
            push: Some(true),
            ..claimed(Some("claim"))
        };
        assert!(schedule.due(&job, &on, start + PUSH_RECHECK * 2));

        schedule.record(
            &job,
            Outcome::Failed(Waiting::PushDisabled.into()),
            claimed(Some("claim")),
            start,
        );
        assert!(schedule.due(&job, &claimed(Some("next-claim")), start));
    }
}
